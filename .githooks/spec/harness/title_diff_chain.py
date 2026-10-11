#!/usr/bin/env python3
"""title_diff chain — semantic consistency of the PR title/body vs the diff.

CM-03 only compares the (title type, branch) string tuple: "fix: 修内存泄漏"
on a UI-only diff sails through. This is the semantic complement — a compare-
shaped judgment (jev_compare on the whole diff vs the description): does what
the PR title + description say actually match what the diff changes?

Gate protocol (mode: diff):
  stdin  = branch diff
  stdout = findings JSON array

PR title/body source, first wins:
  1. GATE_PR_TITLE_FILE / GATE_PR_BODY_FILE env hooks (files; test hook so
     tests need neither gh nor the network);
  2. `gh pr view --json title,body` (subprocess, same pattern as
     delivery_chain.find_pr_body).
Neither obtainable -> a single INFO "no PR context, skipped", exit 0.

Question direction: one noul whose true = "the PR title and description
misdescribe or contradict what this diff actually changes" (HIGH = BAD, same
direction as severity). Declarative negative proposition only — an
interrogative would flip the verdict (measured pitfall, see review_chain).

Severity cap: compare-shaped judgments misfire far more often than per-claim
verification, so the harness caps its output at WARN ("semantic advisory"):
a p above the fail line is still reported as WARN; a p below the warn line
produces no finding at all (this check must not be a noise generator). The
checklist pins fail_severity: WARN as well.

Degradation (DWJ contract): endpoint pair via jev_env (JEV_API_* wins,
TYPESAFE_* fallback). No key / network / unparseable -> a single INFO
finding; this layer never blocks on its own infrastructure.

Manual: canon check pr_consistency
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import review_chain  # 同族复用 post()/finding()/thresholds()/MAX_STATE_CHARS

# Compare-shaped judgment: thresholds from the question itself; the harness
# caps the emitted severity at WARN regardless (see module docstring).
MISMATCH = {
    "type": "noul",
    "instructions": (
        "The PR title and description misdescribe or contradict what this "
        "diff actually changes."
    ),
    "criteria": {
        "true": "the description contradicts the diff or clearly misdescribes its actual content",
        "false": "consistent with the diff, or only worded more broadly than necessary",
    },
    "fail": 0.85,
    "warn": 0.6,
}


def info(fid: str, message: str, extra: dict | None = None) -> dict:
    f = {"id": fid, "severity": "INFO", "path": "", "line": 0, "message": message}
    if extra:
        f.update(extra)
    return f


def fetch_title_body() -> tuple:
    """Return (title, body, source); both empty means "no PR context"."""
    t_hook = os.environ.get("GATE_PR_TITLE_FILE")
    b_hook = os.environ.get("GATE_PR_BODY_FILE")
    if t_hook or b_hook:
        title, body = "", ""
        if t_hook:
            try:
                with open(t_hook, encoding="utf-8") as fh:
                    title = fh.read().strip()
            except OSError as e:
                return "", "", f"GATE_PR_TITLE_FILE unreadable ({e})"
        if b_hook:
            try:
                with open(b_hook, encoding="utf-8") as fh:
                    body = fh.read()
            except OSError as e:
                return "", "", f"GATE_PR_BODY_FILE unreadable ({e})"
        return title, body, "env hooks"
    try:
        # Two single-field fetches, the exact shape delivery_chain proved:
        # environments' gh shims only answer `-q .field`, not jq programs.
        out_t = subprocess.run(
            ["gh", "pr", "view", "--json", "title", "-q", ".title"],
            capture_output=True, text=True, timeout=20, check=True,
        )
        out_b = subprocess.run(
            ["gh", "pr", "view", "--json", "body", "-q", ".body"],
            capture_output=True, text=True, timeout=20, check=True,
        )
    except Exception as e:  # noqa: BLE001 - no gh / no network / no PR is a skip, not a block
        return "", "", f"gh pr view failed ({e})"
    return out_t.stdout.strip(), out_b.stdout, "gh"


def build_state(title: str, body: str, diff: str) -> str:
    # Diff is chronological, so its tail carries the changes under review
    # (same reasoning as review_chain/delivery_chain).
    evidence = diff
    if len(evidence) > review_chain.MAX_STATE_CHARS:
        evidence = evidence[-review_chain.MAX_STATE_CHARS :]
    return f"PR TITLE:\n{title}\n\nPR BODY:\n{body}\n\nBRANCH DIFF:\n{evidence}"


def judge(title: str, body: str, diff: str) -> tuple:
    findings, err = review_chain.run_jev(
        build_state(title, body, diff), {"mismatch": MISMATCH}, MISMATCH["fail"], MISMATCH["warn"]
    )
    if findings is None:
        return None, err
    out = []
    for f in findings:
        if f["severity"] == "INFO":
            continue  # 低 p 只作参照, 这条检查不该是噪声发生器
        f = dict(f)
        f["id"] = "TITLE-DIFF-MISMATCH"
        if f["severity"] == "FAIL":
            # Compare-shaped judgment: cap at WARN, keep the verdict data.
            f["severity"] = "WARN"
            f["message"] = (
                f"jev: mismatch p(issue)={f['confidence']} "
                f"(fail>={MISMATCH['fail']}, warn>={MISMATCH['warn']}) — "
                "DISPOSITION REQUIRED: 描述与 diff 实际改动失配/矛盾，改描述或改 diff。"
                " [semantic advisory: judgment layer capped at WARN]"
            )
        out.append(f)
    return out, None


def main() -> int:
    diff = sys.stdin.read()
    title, body, source = fetch_title_body()
    if not (title.strip() or body.strip()):
        json.dump(
            [info("TITLE-DIFF", f"无 PR 标题/正文，跳过标题-diff 语义一致性检查（{source}）")],
            sys.stdout,
        )
        return 0
    findings, err = judge(title, body, diff)
    if findings is None:
        print(f"title_diff_chain: {err}", file=sys.stderr)
        json.dump([info("TITLE-DIFF", f"jev 不可用，降级 INFO 不阻断（{err}）")], sys.stdout)
        return 0
    json.dump(findings, sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
