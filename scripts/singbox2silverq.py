#!/usr/bin/env python3
"""把 sing-box 的 nodes.json 转成 silverq 的节点表 YAML。

用途：拿真实节点池跑 silverq，验证四种协议的 adapter 构建与真实测速。

凭证只在本地文件间流动：读 nodes.json → 写 out.yaml，脚本不打印任何密钥。
stdout 只输出结构化统计（各协议数量、跳过原因）。

    ./scripts/singbox2lift.py ~/.config/sing-box/nodes.json /tmp/real-nodes.yaml

silverq 当前支持 vless / trojan / shadowsocks / hysteria2；
vmess / http 等会被跳过并计入统计。
"""

import json
import sys
from collections import Counter


def conv_transport(o):
    """sing-box transport -> silverq transport spec。不支持的返回 (None, 原因)。"""
    t = o.get("transport")
    if not t:
        return None, None
    kind = t.get("type")
    if kind == "ws":
        spec = {"type": "ws", "path": t.get("path") or "/"}
        # sing-box 的 ws host 在 headers.Host（大小写不定）
        headers = t.get("headers") or {}
        host = next((v for k, v in headers.items() if k.lower() == "host"), None)
        if isinstance(host, list):
            host = host[0] if host else None
        if host:
            spec["host"] = host
        return spec, None
    if kind == "grpc":
        return {
            "type": "grpc",
            "service_name": t.get("service_name") or "GunService",
        }, None
    return None, f"transport={kind} 未支持"


def conv_vless(o):
    tls = o.get("tls") or {}
    spec = {"uuid": o["uuid"]}
    if sni := tls.get("server_name"):
        spec["sni"] = sni
    # sing-box 的 flow 在顶层；silverq 只认 xtls-rprx-vision
    if o.get("flow") == "xtls-rprx-vision":
        spec["flow"] = "xtls-rprx-vision"
    if reality := tls.get("reality"):
        if not reality.get("public_key"):
            return None, "reality 缺 public_key"
        r = {"public_key": reality["public_key"]}
        if sid := reality.get("short_id"):
            r["short_id"] = sid
        spec["reality"] = r
    if utls := tls.get("utls"):
        if fp := utls.get("fingerprint"):
            spec["fingerprint"] = fp
    # TLS 可选：明文 + ws/grpc 伪装的节点也能接
    spec["tls"] = bool(tls.get("enabled"))
    tr, reason = conv_transport(o)
    if reason:
        return None, reason
    if tr:
        spec["transport"] = tr
    elif not spec["tls"]:
        # 既没 TLS 又没传输层伪装 = 裸 VLESS over TCP，服务端基本不会这么配
        return None, "vless 无 tls 且无 transport"
    return {"protocol": "vless", "vless": spec}, None


def conv_trojan(o):
    tls = o.get("tls") or {}
    spec = {"password": o["password"]}
    if sni := tls.get("server_name"):
        spec["sni"] = sni
    spec["skip_cert_verify"] = bool(tls.get("insecure", True))
    # silverq 的 trojan adapter 还没接 transport 层（VLESS 才有 TransportChain 入口）
    if o.get("transport"):
        return None, f"trojan transport={o['transport'].get('type', '?')} 未支持"
    return {"protocol": "trojan", "trojan": spec}, None


def conv_ss(o):
    return {
        "protocol": "shadowsocks",
        "shadowsocks": {"password": o["password"], "cipher": o["method"]},
    }, None


def conv_hy2(o):
    tls = o.get("tls") or {}
    spec = {"password": o.get("password") or ""}
    if sni := tls.get("server_name"):
        spec["sni"] = sni
    if not spec["password"]:
        return None, "hysteria2 缺 password"
    if o.get("obfs"):
        return None, "hysteria2 obfs 未支持"
    return {"protocol": "hysteria2", "hysteria2": spec}, None


CONV = {
    "vless": conv_vless,
    "trojan": conv_trojan,
    "shadowsocks": conv_ss,
    "hysteria2": conv_hy2,
}


def yaml_quote(s):
    """最小 YAML 字符串转义：一律双引号 + 转义反斜杠和双引号。"""
    return '"' + str(s).replace("\\", "\\\\").replace('"', '\\"') + '"'


def emit(nodes):
    lines = ["# 由 scripts/singbox2lift.py 从 sing-box nodes.json 生成", "nodes:"]
    for n in nodes:
        lines.append(f"  - tag: {yaml_quote(n['tag'])}")
        lines.append(f"    protocol: {n['protocol']}")
        lines.append(f"    server: {yaml_quote(n['server'])}")
        lines.append(f"    port: {n['port']}")
        key = n["protocol"]
        if sub := n.get(key):
            lines.append(f"    {key}:")
            for k, v in sub.items():
                if isinstance(v, dict):
                    lines.append(f"      {k}:")
                    for k2, v2 in v.items():
                        if isinstance(v2, bool):
                            lines.append(f"        {k2}: {'true' if v2 else 'false'}")
                        else:
                            lines.append(f"        {k2}: {yaml_quote(v2)}")
                elif isinstance(v, bool):
                    lines.append(f"      {k}: {'true' if v else 'false'}")
                else:
                    lines.append(f"      {k}: {yaml_quote(v)}")
    return "\n".join(lines) + "\n"


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    src, dst = sys.argv[1], sys.argv[2]

    obs = json.load(open(src))["outbounds"]
    out, skipped = [], Counter()

    for o in obs:
        t = o.get("type")
        conv = CONV.get(t)
        if not conv:
            skipped[f"type={t} 未支持"] += 1
            continue
        if not o.get("server") or not o.get("server_port"):
            skipped["缺 server/port"] += 1
            continue
        try:
            spec, reason = conv(o)
        except KeyError as e:
            skipped[f"{t} 缺字段 {e}"] += 1
            continue
        if spec is None:
            skipped[reason] += 1
            continue
        spec.update(tag=o["tag"], server=o["server"], port=o["server_port"])
        out.append(spec)

    with open(dst, "w") as f:
        f.write(emit(out))

    print(f"converted {len(out)}/{len(obs)} -> {dst}")
    print("by protocol:", dict(Counter(n["protocol"] for n in out)))
    if skipped:
        print("skipped:")
        for reason, n in skipped.most_common():
            print(f"  {n:3d}  {reason}")


if __name__ == "__main__":
    main()
