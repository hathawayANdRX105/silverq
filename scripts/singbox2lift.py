#!/usr/bin/env python3
"""把 sing-box 的 nodes.json 转成 lift 的节点表 YAML。

用途：拿真实节点池跑 lift，验证四种协议的 adapter 构建与真实测速。

凭证只在本地文件间流动：读 nodes.json → 写 out.yaml，脚本不打印任何密钥。
stdout 只输出结构化统计（各协议数量、跳过原因）。

    ./scripts/singbox2lift.py ~/.config/sing-box/nodes.json /tmp/real-nodes.yaml

lift 当前支持 vless / trojan / shadowsocks / hysteria2；
vmess / http 等会被跳过并计入统计。
"""

import json
import sys
from collections import Counter


def conv_vless(o):
    tls = o.get("tls") or {}
    spec = {"uuid": o["uuid"]}
    if sni := tls.get("server_name"):
        spec["sni"] = sni
    # sing-box 的 flow 在顶层；lift 只认 xtls-rprx-vision
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
    # lift 的 factory 对 vless 一定会建 TLS 层；没开 tls 的节点接不了
    if not tls.get("enabled"):
        return None, "vless 未启用 tls（lift factory 目前必建 TLS 层）"
    # transport（ws/grpc 等）lift 还没接
    if o.get("transport"):
        return None, f"transport={o['transport'].get('type', '?')} 未支持"
    return {"protocol": "vless", "vless": spec}, None


def conv_trojan(o):
    tls = o.get("tls") or {}
    spec = {"password": o["password"]}
    if sni := tls.get("server_name"):
        spec["sni"] = sni
    spec["skip_cert_verify"] = bool(tls.get("insecure", True))
    if o.get("transport"):
        return None, f"transport={o['transport'].get('type', '?')} 未支持"
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
