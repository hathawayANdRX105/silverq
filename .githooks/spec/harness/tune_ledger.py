#!/usr/bin/env python3
"""tune ledger — 处置账本 + 阈值调优提案(离线工具, 零自动改配置)。

规则命中后, 维护者在 PR 的 Delivery record 里写处置(修复或书面驳回)。
驳回写多了说明规则可能在错地方开火——但**阈值变更永远人批**, 本工具
只记账 + 出提案:

- `collect`: 从 gh 读最近 N 个 merged PR 的 body(默认 30), 抽 Delivery
  record 处置条目(复用 delivery_chain 的抽取), 追加到账本
  `specs/ledger/findings.jsonl`(默认, 进仓)。字段:
  `ts, pr, rule_id, action(repair|reject), reason_sha(sha256 前 12),
  has_evidence(bool)`。**不存 PR 原文**——账本进仓, 只存 sha256 摘要。
  幂等: 同一 (pr, rule_id, action) 已存在则跳过。
- `propose`: 读账本按 rule_id 聚合 → reject 占比 ≥0.5 且样本 ≥5 的规则,
  一次 jev 批量判每条驳回质量(sound = 有具体技术依据 / rubber_stamp =
  模板话/无依据), 只对 sound 占比 ≥0.6 的规则输出提案(markdown 到
  stdout)。无端点对时退化为纯统计版, 标注 `no-jev: quality unclassified`。
  **本工具永不改任何配置**——提案只是给人看的 markdown。

手动跑法:
  python3 specs/harness/tune_ledger.py collect --limit 30
  python3 specs/harness/tune_ledger.py propose
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.request
from datetime import datetime, timezone

import delivery_chain  # 同族复用 Delivery record 抽取
import jev_env
import rule_selector  # 规则目录 → 归一化 rule stem

DEFAULT_LEDGER = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                              "..", "ledger", "findings.jsonl")

REJECT_PAT = re.compile(r"\breject(?:ed)?\b|驳回", re.IGNORECASE)
EVIDENCE_PAT = re.compile(r"#\d+|evidence|实测|smoke|\btest", re.IGNORECASE)
CK_PAT = re.compile(r"\bCK-([A-Z0-9-]+)")


def run_gh(args: list, repo: str | None) -> str:
    """唯一碰 gh 的入口(测试 mock 这里)。"""
    cmd = ["gh"] + args + (["--repo", repo] if repo else [])
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=30, check=False)
    if out.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args)} failed: {out.stderr.strip()[:200]}")
    return out.stdout


def _norm(s: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")


def rule_stems() -> list:
    """目录规则归一化 stem(最长优先): slop_comment → slop-comment。"""
    try:
        rules = rule_selector.load_catalog(".")
    except Exception:  # noqa: BLE001 - 目录读不了退回 CK-* 裸匹配
        return []
    stems = set()
    for r in rules:
        s = _norm(r["id"])
        s = s.removeprefix("checklist-")
        if s:
            stems.add(s)
    return sorted(stems, key=len, reverse=True)


def classify_claims(claims: list) -> list:
    """claim → [{rule_id, action, reason}]: 只收引用了规则名的处置条目。"""
    stems = rule_stems()
    out = []
    for claim in claims:
        action = "reject" if REJECT_PAT.search(claim) else "repair"
        rule_id = None
        if stems and any(_norm(stem) in _norm(claim) for stem in stems if len(stem) > 2):
            rule_id = next(s for s in stems if len(s) > 2 and _norm(s) in _norm(claim))
        else:
            m = CK_PAT.search(claim)
            if m:
                rule_id = _norm(m.group(1))
        if not rule_id:
            continue
        reason = claim
        m = re.search(r"(?:[:：]\s*)?(repair|reject(?:ed)?|驳回)", claim, re.IGNORECASE)
        if m:
            reason = claim[m.end():].strip(" -:—")
        out.append({"rule_id": _norm(rule_id), "action": action, "reason": reason or claim})
    return out


def load_rows(ledger: str) -> list:
    if not os.path.exists(ledger):
        return []
    rows = []
    with open(ledger, encoding="utf-8") as fh:
        for ln in fh:
            if ln.strip():
                rows.append(json.loads(ln))
    return rows


def cmd_collect(limit: int, repo: str | None, ledger: str) -> int:
    prs = json.loads(run_gh(["pr", "list", "--state", "merged",
                            "--limit", str(limit), "--json", "number,title,body"], repo))
    rows = load_rows(ledger)
    seen = {(r["pr"], r["rule_id"], r["action"]) for r in rows}
    new_rows = []
    for pr in prs:
        claims = delivery_chain.extract_claims(pr.get("body") or "")
        for c in classify_claims(claims):
            key = (pr["number"], c["rule_id"], c["action"])
            if key in seen:
                continue
            new_rows.append({
                "ts": datetime.now(timezone.utc).isoformat(timespec="seconds"),
                "pr": pr["number"],
                "rule_id": c["rule_id"],
                "action": c["action"],
                "reason_sha": hashlib.sha256(c["reason"].encode()).hexdigest()[:12],
                "has_evidence": bool(EVIDENCE_PAT.search(c["reason"])),
            })
            seen.add(key)
    if new_rows:
        os.makedirs(os.path.dirname(ledger) or ".", exist_ok=True)
        with open(ledger, "a", encoding="utf-8") as fh:
            fh.write("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in new_rows))
    print(f"tune_ledger collect: {len(new_rows)} new, {len(rows) + len(new_rows)} total -> {ledger}")
    return 0


def post(url: str, headers: dict, body: dict, timeout: int):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json", **headers})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


def _reject_reasons(rows: list, repo: str | None) -> list:
    """把账本里的驳回行捞回原文(gh 按 pr 取 body, reason_sha 对回去);
    取不到的行跳过, 不阻塞统计。"""
    by_pr: dict = {}
    for r in rows:
        if r["action"] != "reject":
            continue
        by_pr.setdefault(r["pr"], []).append(r)
    out = []
    for pr_no, items in by_pr.items():
        try:
            pr = json.loads(run_gh(["pr", "view", str(pr_no), "--json", "body"], repo))
        except Exception as e:  # noqa: BLE001 - PR 删了/无 gh: 跳过, 不阻塞
            print(f"tune_ledger: pr {pr_no} body unavailable ({e}); 该行跳过", file=sys.stderr)
            continue
        for c in classify_claims(delivery_chain.extract_claims(pr.get("body") or "")):
            sha = hashlib.sha256(c["reason"].encode()).hexdigest()[:12]
            for it in items:
                if it["reason_sha"] == sha:
                    out.append(it | {"reason": c["reason"]})
                    break
    return out


def classify_quality(base: str, key: str, model: str, rule_id: str, items: list) -> float:
    """一次批量 choice: 每条驳回一个题, p(sound) 占比。"""
    qs = {}
    state = {"rule": rule_id,
             "rejections": [i["reason"][:400] for i in items]}
    for k, i in enumerate(items):
        qs[f"rej_{k}"] = {
            "type": "choice",
            "instructions": (f"Rejection reason #{k} of rule {rule_id}: "
                             f"「{i['reason'][:200]}」。 "
                             "This written rejection has concrete technical grounding "
                             "(a specific reason why the rule misfires here), not "
                             "boilerplate."),
            "labels": ["sound", "rubber_stamp"],
            "fail_labels": ["sound"],
            # criteria 必填: wire 上缺 criteria 的 choice 题被拒(实测 400),
            # 每条标签一句可观察证据(gate-spec 编写标准)。
            "criteria": {
                "sound": "the rejection cites a checkable technical reason for this "
                         "change — a measured value, a cited issue, a structural fact "
                         "visible in the diff",
                "rubber_stamp": "generic prose — 'not applicable here', 'works for "
                                "now', 'kept as is' — with nothing checkable for this "
                                "specific change",
            },
        }
    payload = post(f"{base}/v1/systemone",
                   {"Authorization": f"Bearer {key}", "x-api-key": key},
                   {"state": json.dumps(state, ensure_ascii=False)[:20000],
                    "model": model, "questions": qs}, 55)
    votes = 0
    for k in range(len(items)):
        ans = (payload.get("answers") or {}).get(f"rej_{k}") or {}
        probs = ans.get("probabilities", {}) or {}
        if probs.get("sound", 0.0) >= 0.5:
            votes += 1
    return votes / len(items)


def cmd_propose(ledger: str, repo: str | None) -> int:
    rows = load_rows(ledger)
    if not rows:
        print("tune_ledger propose: empty ledger, nothing to aggregate")
        return 0
    by_rule: dict = {}
    for r in rows:
        by_rule.setdefault(r["rule_id"], []).append(r)
    base, key, model = jev_env.resolve()
    lines = ["# 阈值调优提案(tune ledger)", "",
             f"账本 `{os.path.relpath(ledger)}` 共 {len(rows)} 条处置。", ""]
    any_proposal = False
    for rule_id in sorted(by_rule):
        items = by_rule[rule_id]
        rejects = [r for r in items if r["action"] == "reject"]
        if len(items) < 5 or len(rejects) / len(items) < 0.5:
            continue
        # 样本够、驳回占比够 → 候选规则
        sound_frac = None
        note = ""
        if key:
            reasons = _reject_reasons(rejects, repo)
            if reasons:
                try:
                    sound_frac = classify_quality(base, key, model, rule_id, reasons)
                except Exception:  # noqa: BLE001 - 判决失败退回纯统计
                    note = " (quality call failed, 纯统计版)"
            else:
                note = " (驳回原文不可得, 纯统计版)"
        else:
            note = " (no-jev: quality unclassified)"
        gated = sound_frac is None or sound_frac >= 0.6
        tag = "提案" if gated else "暂不提案(驳回多为模板话)"
        lines += [
            f"## {rule_id} — {tag}",
            "",
            f"- 样本 {len(items)} 条, 书面驳回 {len(rejects)} 条"
            + (f", 其中有实质依据 {round((sound_frac or 0) * len(rejects))} 条"
               if sound_frac is not None else "")
            + note,
            ""]
        if gated:
            lines.append(
                f"建议复核 {rule_id} 的阈值/适用范围(驳回 {len(rejects)}/{len(items)}"
                f"{' 且实质占比 ' + format(round((sound_frac or 0) * 100)) + '%' if sound_frac is not None else ''})"
                " —— **本工具只出提案, 永不改配置, 须维护者批准。**")
            lines.append("")
            any_proposal = True
    if not any_proposal:
        lines.append("本次无可提案规则(样本不足 5 条或驳回占比 <0.5, 或驳回多为模板话)。")
    print("\n".join(lines))
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("collect")
    c.add_argument("--limit", type=int, default=30)
    c.add_argument("--repo", default=None)
    c.add_argument("--ledger", default=os.path.normpath(DEFAULT_LEDGER))
    p = sub.add_parser("propose")
    p.add_argument("--repo", default=None)
    p.add_argument("--ledger", default=os.path.normpath(DEFAULT_LEDGER))
    a = ap.parse_args()
    if a.cmd == "collect":
        return cmd_collect(a.limit, a.repo, a.ledger)
    return cmd_propose(a.ledger, a.repo)


if __name__ == "__main__":
    sys.exit(main())
