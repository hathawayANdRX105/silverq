//! 节点服务器域名的真实上游 DNS 解析。
//!
//! 背景（2026-09-19 事故）：silverq 开 TUN 后本机 `/etc/resolv.conf` 指向
//! 自家 fake-IP DNS（198.18.0.1），任何走系统解析的进程——包括 silverq
//! 自己——对节点域名拿到的都是 198.18.x 假地址；假地址路由进 meow-tun，
//! 「拨节点」变成「经候选链代理拨节点」的自指递归，健康检查大规模误杀
//! （实测直测 48 个池节点 10 个存活，silverq 测成 800 选 1）。
//!
//! 此模块在构建 adapter 前用真实上游 DNS 把节点域名预解析成 IP 写入
//! `NodeSpec.dial_addr`。真实 IP 不在 fake-IP 段（198.18.0.0/15）内，
//! 拨号走物理网卡，天然绕开 TUN，无需 route/mark 特殊处理。
//! SNI / ws Host / 兜底标识仍用原 `server` 域名（factory 侧保持不变）。
//!
//! 失败回退：解析失败保持 `dial_addr = None`，build_proxy 退回旧行为
//! （按 server 原文交给系统解析）——只降级，不丢节点。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::Semaphore;

use crate::proxy::nodespec::NodeSpec;

/// 默认真实上游。系统解析已被 fake-IP 接管后不可再用；
/// 选国内可达的公共 DNS（UDP 53 出站走物理网卡，不进 TUN）。
const DEFAULT_UPSTREAMS: &[&str] = &["223.5.5.5", "119.29.29.29"];

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CONCURRENCY: usize = 100;

/// 上游列表：环境变量 `SILVERQ_RESOLVE_UPSTREAMS`（逗号分隔）可覆盖。
pub fn upstreams() -> Vec<IpAddr> {
    let raw = std::env::var("SILVERQ_RESOLVE_UPSTREAMS").unwrap_or_default();
    let list: Vec<IpAddr> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if list.is_empty() {
        DEFAULT_UPSTREAMS
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect()
    } else {
        list
    }
}

static QUERY_ID: AtomicU16 = AtomicU16::new(0x1a2b);

/// 国内域名表路径：默认 `~/.config/silverq/rules/china-domains.txt`，
/// 环境变量 `SILVERQ_CHINA_DOMAINS` 可覆盖。
pub fn china_domains_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SILVERQ_CHINA_DOMAINS") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("silverq")
        .join("rules")
        .join("china-domains.txt")
}

/// 读国内域名表：一行一个域名，`#` 注释跳过。文件不存在/为空 → 空表
/// （所有调用方对空表都是 no-op，不阻断启动）。
pub fn load_china_domains(path: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 国内域名后缀集合：label 对齐的后缀匹配（`example.com` 命中自身与
/// `*.example.com`，不命中 `notexample.com`）。11 万级条目下每次匹配
/// 只做 O(域名 label 数) 次 HashSet 查找。
pub struct ChinaSet {
    patterns: Vec<String>,
    suffixes: HashSet<String>,
}

impl ChinaSet {
    pub fn new(patterns: &[String]) -> Self {
        Self {
            patterns: patterns.to_vec(),
            suffixes: patterns.iter().cloned().collect(),
        }
    }

    /// 原始 pattern 列表（TUN 侧喂给 meow-dns 的 Skipper）。
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub fn is_empty(&self) -> bool {
        self.suffixes.is_empty()
    }

    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let mut rest = host.as_str();
        loop {
            if self.suffixes.contains(rest) {
                return true;
            }
            match rest.split_once('.') {
                Some((_, tail)) => rest = tail,
                None => return false,
            }
        }
    }
}

/// 构造最小 DNS 查询报文（RD=1，单问题，IN class）。
fn build_query(id: u16, name: &str, qtype: u16) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(17 + name.len());
    buf.extend_from_slice(&id.to_be_bytes());
    // flags: RD=1；QDCOUNT=1，其余计数 0
    buf.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    for label in name.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return None;
        }
        buf.push(bytes.len() as u8);
        buf.extend_from_slice(bytes);
    }
    if name.is_empty() || name.ends_with('.') {
        return None;
    }
    buf.push(0); // 根标签结束
    buf.extend_from_slice(&qtype.to_be_bytes());
    buf.extend_from_slice(&[0x00, 0x01]); // class IN
    Some(buf)
}

/// 跳过报文里的域名（处理 0xC0 指针压缩），返回记录数据起始偏移。
fn skip_name(data: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *data.get(i)?;
        if len & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        i += 1;
        if len == 0 {
            return Some(i);
        }
        i += len as usize;
    }
}

/// 解析响应中的 A/AAAA 记录（`qtype` = 1 或 28），校验事务 ID。
fn parse_answers(data: &[u8], qtype: u16, expect_id: u16) -> Vec<IpAddr> {
    let mut out = Vec::new();
    if data.len() < 12 || u16::from_be_bytes([data[0], data[1]]) != expect_id {
        return out;
    }
    let ancount = u16::from_be_bytes([data[6], data[7]]) as usize;
    let Some(mut i) = skip_name(data, 12) else {
        return out;
    };
    i += 4; // 跳过 QTYPE + QCLASS
    for _ in 0..ancount {
        let Some(next) = skip_name(data, i) else {
            break;
        };
        i = next;
        if i + 10 > data.len() {
            break;
        }
        let typ = u16::from_be_bytes([data[i], data[i + 1]]);
        let rdlen = u16::from_be_bytes([data[i + 8], data[i + 9]]) as usize;
        i += 10;
        if i + rdlen > data.len() {
            break;
        }
        let rdata = &data[i..i + rdlen];
        match (typ, qtype) {
            (1, 1) if rdlen == 4 => {
                out.push(IpAddr::V4(Ipv4Addr::new(
                    rdata[0], rdata[1], rdata[2], rdata[3],
                )));
            }
            (28, 28) if rdlen == 16 => {
                let octets: [u8; 16] = rdata.try_into().expect("rdlen checked == 16");
                out.push(IpAddr::V6(octets.into()));
            }
            _ => {}
        }
        i += rdlen;
    }
    out
}

/// 向单个上游发一次查询，返回第一条匹配记录。
async fn query(upstream: IpAddr, name: &str, qtype: u16) -> Option<IpAddr> {
    let id = QUERY_ID.fetch_add(1, Ordering::Relaxed);
    let payload = build_query(id, name, qtype)?;
    let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sock.connect((upstream, 53)).await.ok()?;
    sock.send(&payload).await.ok()?;
    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(QUERY_TIMEOUT, sock.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    parse_answers(&buf[..n], qtype, id).into_iter().next()
}

/// 单域名解析：IP 字面量直接返回；否则 A 优先（本机多半无全局 v6 路由），
/// 全部上游失败再试 AAAA（兜 v6-only 节点），仍失败返回 None。
pub async fn resolve_host(host: &str) -> Option<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }
    let ups = upstreams();
    for qtype in [1u16, 28] {
        for up in &ups {
            if let Some(ip) = query(*up, host, qtype).await {
                return Some(ip);
            }
        }
    }
    None
}

/// 批量预解析节点拨号地址：域名去重、并发上限 [`MAX_CONCURRENCY`]，
/// 成功者写 `spec.dial_addr`，失败者保持 None（build_proxy 回退旧行为）。
pub async fn resolve_dial_addrs(specs: &mut [NodeSpec]) {
    let hosts: Vec<String> = {
        let set: HashSet<String> = specs
            .iter()
            .filter(|s| s.dial_addr.is_none())
            .map(|s| s.server.clone())
            .filter(|h| h.parse::<IpAddr>().is_err())
            .collect();
        set.into_iter().collect()
    };
    if hosts.is_empty() {
        return;
    }

    let sem = Arc::new(Semaphore::new(MAX_CONCURRENCY));
    let mut tasks = Vec::with_capacity(hosts.len());
    for host in &hosts {
        tasks.push(tokio::spawn({
            let sem = Arc::clone(&sem);
            let host = host.clone();
            async move {
                let _permit = sem.acquire().await;
                let ip = resolve_host(&host).await;
                (host, ip)
            }
        }));
    }

    let mut resolved: HashMap<String, IpAddr> = HashMap::new();
    for t in tasks {
        if let Ok((host, Some(ip))) = t.await {
            resolved.insert(host, ip);
        }
    }
    for s in specs.iter_mut() {
        if s.dial_addr.is_none() {
            if let Some(ip) = resolved.get(&s.server) {
                s.dial_addr = Some(ip.to_string());
            }
        }
    }
    tracing::info!(
        resolved = resolved.len(),
        failed = hosts.len() - resolved.len(),
        total = hosts.len(),
        "节点服务器域名真实 DNS 预解析完成（失败者回退系统解析）"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    // example.com @223.5.5.5 的真实响应（2026-09-19 抓取，含两个答案 +
    // 0xC0 名字指针压缩），ID = 0xabcd。
    const A_RESP: &str = "abcd81800001000200000000076578616d706c6503636f6d0000\
        010001c00c000100010000008d0004ac4293f3c00c000100010000008d00046814179a";
    const AAAA_RESP: &str = "abcd81800001000200000000076578616d706c6503636f6d0000\
        1c0001c00c001c00010000001300102606470000100000000000006814179a\
        c00c001c0001000000130010260647000010000000000000ac4293f3";

    #[test]
    fn parse_a_response_extracts_all_answers() {
        let ips = parse_answers(&hex(A_RESP), 1, 0xabcd);
        assert_eq!(ips.len(), 2);
        assert!(ips.contains(&"172.66.147.243".parse::<IpAddr>().unwrap()));
        assert!(ips.contains(&"104.20.23.154".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn parse_aaaa_response_extracts_all_answers() {
        let ips = parse_answers(&hex(AAAA_RESP), 28, 0xabcd);
        assert_eq!(ips.len(), 2);
        assert!(ips.contains(&"2606:4700:10::6814:179a".parse::<IpAddr>().unwrap()));
        assert!(ips.contains(&"2606:4700:10::ac42:93f3".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn parse_rejects_mismatched_transaction_id() {
        let mut bytes = hex(A_RESP);
        bytes[0] ^= 0xff;
        assert!(parse_answers(&bytes, 1, 0xabcd).is_empty());
    }

    #[test]
    fn build_query_encodes_name_and_type() {
        let q = build_query(0x1234, "a.example.com", 28).expect("valid name");
        assert_eq!(&q[0..2], &[0x12u8, 0x34]);
        // QNAME: 1"a" 7"example" 3"com" 0
        let expect: &[u8] = b"\x01a\x07example\x03com\x00";
        assert!(q.windows(expect.len()).any(|w| w == expect));
        // QTYPE=AAAA(28) IN(1) 收尾
        assert_eq!(&q[q.len() - 4..], &[0x00u8, 28, 0x00, 0x01]);
    }

    #[test]
    fn build_query_rejects_bad_names() {
        assert!(build_query(1, "", 1).is_none());
        assert!(build_query(1, "a..b", 1).is_none());
        assert!(build_query(1, "ok.example.", 1).is_none());
        assert!(build_query(1, &"x".repeat(64), 1).is_none());
    }

    #[test]
    fn china_set_matches_suffix_not_substring() {
        let cs = ChinaSet::new(&["example.com".into(), "baidu.com".into()]);
        assert!(cs.matches("example.com"));
        assert!(cs.matches("a.example.com"));
        assert!(cs.matches("EXAMPLE.com."));
        assert!(cs.matches("www.baidu.com"));
        assert!(!cs.matches("notexample.com"), "子串不能误判为后缀命中");
        assert!(!cs.matches("baidu.cn"));
        assert!(!cs.matches("example.org"));
    }

    #[test]
    fn load_china_domains_skips_comments_and_blank() {
        let dir = std::env::temp_dir().join("silverq-china-test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("domains.txt");
        std::fs::write(&p, "a.com\n\n# comment\n b.com \n").unwrap();
        assert_eq!(
            load_china_domains(&p),
            vec!["a.com".to_string(), "b.com".to_string()]
        );
        assert!(load_china_domains(&dir.join("missing.txt")).is_empty());
    }

    #[test]
    fn resolve_host_passes_through_ip_literals() {
        // 字面量不发包：127.0.0.1 在无网环境也必须原样返回。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ip = rt.block_on(resolve_host("127.0.0.1"));
        assert_eq!(ip, Some("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn resolve_dial_addrs_fills_only_domain_nodes() {
        // 字面量节点不该有 dial_addr（无域名可解）；域名节点用假上游
        // （127.0.0.1:53 无服务，连接即拒）→ 解析失败保持 None（回退旧行为）。
        std::env::set_var("SILVERQ_RESOLVE_UPSTREAMS", "127.0.0.1");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut specs = vec![
            NodeSpec {
                tag: "ip-node".into(),
                protocol: crate::proxy::nodespec::Protocol::Shadowsocks,
                server: "127.0.0.1".into(),
                port: 1,
                dial_addr: None,
                vless: None,
                trojan: None,
                shadowsocks: None,
                hysteria2: None,
            },
            NodeSpec {
                tag: "domain-node".into(),
                protocol: crate::proxy::nodespec::Protocol::Shadowsocks,
                server: "invalid.invalid".into(),
                port: 1,
                dial_addr: None,
                vless: None,
                trojan: None,
                shadowsocks: None,
                hysteria2: None,
            },
        ];
        rt.block_on(resolve_dial_addrs(&mut specs));
        assert!(specs[0].dial_addr.is_none(), "IP 字面量无需预解析");
        assert!(
            specs[1].dial_addr.is_none(),
            "解析失败应回退系统解析（dial_addr 保持 None）"
        );
    }
}
