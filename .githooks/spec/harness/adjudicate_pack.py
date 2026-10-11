#!/usr/bin/env python3
"""adjudicate pack — 把「待裁决」发现打包成有界选项简报给维护者(离线工具)。

输入两路:
  --pr N          gh 取 PR body + files; 待裁决项 = Delivery record 里含
                  待裁决/pending adjudication/规则缺陷 标记的条目;
  --stdin         canon check --json 的 findings 数组; 待裁决项 = WARN 级
                  且无处置匹配(该 rule 名未出现在 Delivery record 处置条目里)。
两者可并用(同一 PR 的 findings + body 一起定位)。

每个待裁决项一次 jev 调用: 固定四选项 choice `verdict`
(accept_reject 书面驳回成立 / demand_fix 应修 / revise_rule 规则本身疑似
缺陷 / insufficient_evidence 证据不足, 标签穷尽) + `confidence` noul
(证据充分度)。verdict 是**决策**不是违规检测: 读整张概率表, 不用
fail_labels 坏档语义。

输出: markdown 裁决简报(stdout, 或 -o file): 每项 = 原条目 + 四选项概率
表 + 证据充分度 + 证据摘要, 页脚「维护者裁决, 本工具不代批」。**零副作用**:
不写 PR、不发评论、不改任何配置(-o 只写简报本身)。

降级契约: 无 jev 端点对 / 单条调用失败 → 该项降级为纯证据包(无概率, 标注
degraded), rc=0, 永不阻断。
凭据: 同 review_chain(JEV_API_* / TYPESAFE_API_*, 见 jev_env)。
手动跑法: python3 specs/harness/adjudicate_pack.py --pr 46
           canon check --sla l3 --json < x | python3 specs/harness/adjudicate_pack.py --stdin
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import delivery_chain  # 同族复用 Delivery record 抽取
import jev_env
from review_chain import wrap_untrusted

VERDICT_LABELS = ("accept_reject", "demand_fix", "revise_rule", "insufficient_evidence")
MARKER_PAT = re.compile(r"待裁决|pending\s+adjudication|规则缺陷", re.IGNORECASE)
TOKEN_PAT = re.compile(r"[a-z][a-z0-9_-]{2,}")
HTTP_TIMEOUT = 55


def post(url: str, headers: dict, body: dict, timeout: int):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json", **headers})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


def run_gh(args: list) -> str:
    out = subprocess.run(["gh"] + args, capture_output=True, text=True, timeout=30, check=False)
    if out.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args)} failed: {out.stderr.strip()[:200]}")
    return out.stdout


def _norm(s: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")


def pr_context(pr_no: str, repo: str | None) -> dict:
    """gh 取 PR title/body/files + 带待裁决标记的 Delivery record 条目。"""
    args = ["pr", "view", pr_no, "--json", "title,body,files"]
    if repo:
        args += ["--repo", repo]
    pr = json.loads(run_gh(args))
    body = pr.get("body") or ""
    items = [c for c in delivery_chain.extract_claims(body) if MARKER_PAT.search(c)]
    return {"title": pr.get("title", ""), "body": body,
            "files": [f.get("path") for f in pr.get("files") or []], "items": items}


def unhandled_findings(findings: list, body: str) -> list:
    """WARN 级且无处置匹配: 该 finding 的 rule 名没出现在 Delivery record
    的任何处置条目里(规则名粗匹配, 归一化后取首段)。"""
    dispositioned = set()
    for c in delivery_chain.extract_claims(body):
        normed = _norm(c).replace("-", " ")
        m = TOKEN_PAT.search(normed)
        if m:
            dispositioned.add(m.group(0))
    out = []
    for f in findings:
        if f.get("severity") != "WARN":
            continue
        rid = _norm((f.get("id") or "").split("-")[0])
        if rid and rid not in dispositioned:
            out.append(f)
    return out


def judge_item(base: str, key: str, model: str, item: dict, evidence: str) -> dict:
    """一次 jev 调用: 四选项 verdict + confidence。"""
    state = json.dumps({"item": item, "evidence": (evidence or "")[:4000]},
                       ensure_ascii=False)[:20000]
    state = wrap_untrusted(state)
    qs = {
        "verdict": {
            "type": "choice",
            "instructions": ("Bounded adjudication of this item. Pick exactly one: "
                             "accept_reject (the written rejection stands as argued), "
                             "demand_fix (the item should actually be fixed), "
                             "revise_rule (the rule itself is defective or mis-scoped), "
                             "insufficient_evidence (cannot tell from what is shown)."),
            "labels": list(VERDICT_LABELS),
            "criteria": {
                "accept_reject": "the item's stated rejection reason cites concrete, checkable evidence that makes the rule inapplicable here",
                "demand_fix": "the evidence shows a real defect this rule is meant to catch, and it is not addressed",
                "revise_rule": "the evidence shows the rule's intent, globs or threshold misfire on this class of change",
                "insufficient_evidence": "the shown evidence is too thin or contradictory to pick the other three"
            },
        },
        "confidence": {
            "type": "noul",
            "instructions": ("How sufficient is the evidence in state to adjudicate this item? "
                             "(0 = none of the decisive evidence shown, 1 = complete)"),
            "criteria": {
                "true": "state shows concrete evidence (diff lines, cited issue, measurable threshold) enough to adjudicate",
                "false": "state omits the decisive evidence (no diff, no issue reference, claims without artifacts)"
            },
        },
    }
    payload = post(f"{base}/v1/systemone",
                   {"Authorization": f"Bearer {key}", "x-api-key": key},
                   {"state": state, "model": model, "questions": qs},
                   HTTP_TIMEOUT)
    ans = payload.get("answers") or {}
    probs = (ans.get("verdict") or {}).get("probabilities", {}) or {}
    conf = float((ans.get("confidence") or {}).get("noul", 0.0))
    return {"probs": {k: probs.get(k, 0.0) for k in VERDICT_LABELS}, "conf": conf}


def build_report(items: list, judged: dict, meta: dict) -> str:
    L = ["# 裁决简报(adjudicate pack)", "",
         f"来源: {meta.get('source', 'n/a')}", ""]
    if meta.get("files"):
        L += ["涉及文件:", *[f"- `{f}`" for f in meta["files"]], ""]
    L += [f"共 {len(items)} 个待裁决项。", ""]
    if judged and all(j.get("degraded") for j in judged.values()):
        L += ["_(degraded: 无 jev 端点/调用失败——以下全为纯证据包, 无概率)_", ""]
    for i, it in enumerate(items):
        text = it.get("text") or it.get("message") or json.dumps(it, ensure_ascii=False)
        L += [f"## 待裁决 {i + 1}", "", f"> {text}", ""]
        j = judged.get(i) or {}
        if j.get("probs") is not None:
            L += ["| 选项 | 概率 |", "| --- | --- |"]
            for k in VERDICT_LABELS:
                L.append(f"| {k} | {j['probs'][k]:.2f} |")
            L.append(f"证据充分度 confidence = {j['conf']:.2f}")
        else:
            L.append("_(degraded: 本条调用失败——纯证据包, 由维护者直接裁决)_")
        L += ["", "---", ""]
    L.append("**维护者裁决, 本工具不代批、不写 PR、不改配置。**")
    return "\n".join(L)


def collect_inputs(argv: list) -> dict:
    pr_no = argv[argv.index("--pr") + 1] if "--pr" in argv else None
    repo = argv[argv.index("--repo") + 1] if "--repo" in argv else None
    out_path = argv[argv.index("-o") + 1] if "-o" in argv else None
    stdin_items = None
    if "--stdin" in argv:
        stdin_items = json.loads(sys.stdin.read() or "[]")
    return {"pr_no": pr_no, "repo": repo, "out_path": out_path,
            "stdin_items": stdin_items}


def main() -> int:
    a = collect_inputs(sys.argv[1:])
    if not a["pr_no"] and a["stdin_items"] is None:
        print("adjudicate_pack: usage: --pr N [--repo R] [-o file] | --stdin [-o file]",
              file=sys.stderr)
        return 2

    pr_ctx = pr_context(a["pr_no"], a["repo"]) if a["pr_no"] else \
        {"title": "", "body": "", "files": [], "items": []}
    meta = {"source": f"PR #{a['pr_no']}" if a["pr_no"] else "findings(JSON)",
            "files": pr_ctx["files"]}
    items = [{"text": c} for c in pr_ctx["items"]]
    if a["stdin_items"] is not None:
        items += unhandled_findings(a["stdin_items"], pr_ctx["body"])

    base, key, model = jev_env.resolve()
    judged: dict = {}
    if not items:
        report = ("# 裁决简报(adjudicate pack)\n\n"
                  + f"来源: {meta['source']}\n\n"
                  "无待裁决项(Delivery record 无 待裁决/规则缺陷 标记, "
                  "且无未处置 WARN finding)。")
    elif not base or not key:
        judged = {i: {"degraded": True} for i in range(len(items))}
        report = build_report(items, judged, meta)
    else:
        for i, it in enumerate(items):
            evidence = pr_ctx["body"] if a["pr_no"] else json.dumps(it, ensure_ascii=False)
            try:
                judged[i] = judge_item(base, key, model, it, evidence)
            except Exception:  # noqa: BLE001 - 单条失败降级为纯证据包, 不阻断
                judged[i] = {"degraded": True}
        report = build_report(items, judged, meta)

    if a["out_path"]:
        with open(a["out_path"], "w", encoding="utf-8") as fh:
            fh.write(report + "\n")
        print(f"adjudicate_pack: wrote {a['out_path']} ({len(items)} items)")
    else:
        print(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
