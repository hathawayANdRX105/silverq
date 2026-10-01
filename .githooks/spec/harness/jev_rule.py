"""jev rule - config-driven jev judgment over changed files.

Lets a project define its own spec checks as pure configuration: pick the
information scope (changed file content, extra context files), write the
questions with the three primitives (noul / choice / score), set thresholds.
No code. The gate feeds changed files on stdin (mode:file); each matched
file becomes one jev state; verdicts below warn are dropped (that drop is
the false-positive filter); no key / request failure emits the file as a
raw WARN candidate (recall kept, disposition still required).

Config (JSON), keys are rule-ids:

  "<rule-id>": {
    "intent": "what this rule enforces (goes into the jev state verbatim)",
    "paths_include": ["**/*.tsx"],          # finer filter within engine-matched files
    "paths_exclude": ["target/"],
    "state": {
      "file": true,                          # include changed file content
      "diff": false,                         # include hunks instead/alongside
      "context_files": ["docs/specs/web.md"] # repo files inlined as rule context
    },
    "questions": {
      "<qid>": {
        "type": "noul",                      # bool | choice | score
        "instructions": "...",
        "criteria": {"true": "...", "false": "..."},
        "fail_labels": ["violation"],        # choice only: labels that mean violation
        "fail": 0.85, "warn": 0.6            # noul: p thresholds; score: level thresholds
      }
    }
  }

Checklist yaml wiring:

  mode: file
  harness:
    command: "python3"
    args: [".githooks/spec/harness/jev_rule.py", "--config", ".githooks/spec/custom/web_spec.json"]
  sla: l2
  timeout: 300
"""

import concurrent.futures
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request

MAX_FILES = 20          # per run, most-changed first is not knowable; stdin order wins
STATE_CAP = 16_000      # chars per jev state
POOL = 8                # concurrent jev calls
TIMEOUT = 60            # per jev call, seconds


def parse_files(payload: str):
    """gate mode:file payload -> [(rel_path, content)]."""
    out, cur, buf = [], None, []
    for line in payload.splitlines():
        if line.startswith("===== FILE: ") and line.endswith(" ====="):
            if cur is not None:
                out.append((cur, "\n".join(buf)))
            cur, buf = line[len("===== FILE: "):-len(" =====")], []
        elif cur is not None:
            buf.append(line)
    if cur is not None:
        out.append((cur, "\n".join(buf)))
    return out


def loose_match(rel: str, pat: str) -> bool:
    """Glob 匹配，与引擎 matches_include 的宽松语义一致。

    曾经只支持结尾的 `/**`，中段/开头的 `**` 一律不匹配——于是
    `crates/web/**/*.rs` 这类 patterns 静默零命中，规则照报 ALL PASS（假绿）。
    现在统一翻译成正则：
      `**/` 匹配零或多层目录，`*` 只跨一段，`?` 匹配单字符。
    """
    pat = pat.strip("/")
    if rel == pat or rel.endswith("/" + pat):
        return True
    if any(c in pat for c in "*?["):
        regex = re.escape(pat).replace(r"\*\*/", "(?:.*/)?")
        regex = regex.replace(r"\*\*", ".*").replace(r"\*", "[^/]*").replace(r"\?", "[^/]")
        return re.fullmatch(regex, rel) is not None
    return rel == pat or rel.endswith("/" + pat)


def file_matches(rule: dict, rel: str) -> bool:
    for x in rule.get("paths_exclude", []):
        if x.rstrip("/") in rel:
            return False
    inc = rule.get("paths_include", [])
    return True if not inc else any(loose_match(rel, p) for p in inc)


def build_state(rule: dict, rel: str, content: str, diff_text: str, root: str) -> list:
    """Returns (state_text, missing_context_files)."""
    parts = [f"RULE INTENT:\n{rule.get('intent', '')}"]
    missing = []
    ctx = rule.get("state", {}).get("context_files", [])
    for cf in ctx:
        try:
            with open(os.path.join(root, cf)) as f:
                parts.append(f"PROJECT SPEC CONTEXT ({cf}):\n{f.read()[:8_000]}")
        except OSError:
            missing.append(cf)
            parts.append(f"PROJECT SPEC CONTEXT ({cf}): <missing>")
    st = rule.get("state", {})
    if st.get("diff"):
        parts.append(f"CHANGED HUNKS ({rel}):\n{diff_text[:8_000]}")
    if st.get("file", True):
        body = content
        if len(body) > STATE_CAP:
            body = body[:STATE_CAP] + "\n…[truncated]"
        parts.append(f"FILE CONTENT ({rel}):\n{body}")
    return "\n\n".join(parts), missing


def diff_for(root: str, rel: str) -> str:
    """Hunks of `rel` vs HEAD, so jev judges the CHANGED code, not the whole file.

    The gate's mode:file payload carries full file content only; the engine does
    not embed hunks. Without this the 'state.diff' config flag is dead and jev
    reviews pre-existing code as if it were new (false positives on large files).
    """
    try:
        out = subprocess.run(
            ["git", "-C", root, "diff", "HEAD", "--unified=3", "--no-color", "--", rel],
            capture_output=True, text=True, timeout=30, check=False).stdout
    except (OSError, subprocess.SubprocessError):
        return ""
    return out[:8_000]


def post(url: str, headers: dict, payload: dict, timeout: int) -> dict:
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", **headers}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())


def judge_one(base: str, key: str, model: str, state: str, questions: dict) -> dict:
    """One jev call; returns {qid: float|str} normalized for scoring."""
    payload = post(f"{base}/v1/systemone",
                   {"Authorization": f"Bearer {key}", "x-api-key": key},
                   {"state": state, "model": model, "questions": questions}, TIMEOUT)
    out = {}
    for qid in questions:
        ans = (payload.get("answers") or {}).get(qid)
        if not ans:
            continue
        t = ans.get("type")
        if t == "noul":
            out[qid] = float(ans.get("noul", 0.0))
        elif t == "score":
            out[qid] = float(ans.get("score", 0.0))
        elif t == "choice":
            out[qid] = (ans.get("choice", ""), ans.get("probabilities", {}) or {})
    return out


def severity_for(q: dict, ans) -> str:
    """Map one answer to FAIL/WARN/None by the question's thresholds."""
    fail, warn = float(q.get("fail", 1.1)), float(q.get("warn", 1.1))
    if q.get("type") == "choice":
        _label, probs = ans
        bad = set(q.get("fail_labels", []))
        p = sum(v for k, v in probs.items() if k in bad)
        return "FAIL" if p >= fail else ("WARN" if p >= warn else None)
    return "FAIL" if ans >= fail else ("WARN" if ans >= warn else None)


def changed_since(root, rev):
    """基线以来改动过的文件。git 失败时返回空集（宁可少判，不静默全仓判）。"""
    for spec in (f"{rev}...HEAD", f"{rev}..HEAD", rev):
        out = subprocess.run(
            ["git", "diff", "--name-only", "-z", spec],
            cwd=root, capture_output=True, check=False,
        )
        if out.returncode == 0:
            return {p for p in out.stdout.decode("utf-8", "replace").split("\0") if p}
    return set()


def main():
    args = sys.argv[1:]
    cfg_path = args[args.index("--config") + 1] if "--config" in args else ""
    try:
        with open(cfg_path) as fh:
            rules = json.loads(fh.read())
    except (OSError, ValueError) as e:
        print(json.dumps([{"id": "JEV-RULE", "severity": "WARN", "path": ".", "line": 0,
                           "message": f"jev_rule config unreadable ({e}); cannot run custom spec"}]))
        return
    files = parse_files(sys.stdin.read())[:MAX_FILES]
    if not files:
        print(json.dumps([]))
        return
    root = os.popen("git rev-parse --show-toplevel").read().strip() or "."
    # --base（或 GATE_BASE）把范围收窄到基线以来的改动。不给就是全仓：存量仓会
    # 一次判几百个文件，而规则管的是「别新写」，不是「清历史债」。全仓审计走
    # merge 阶段显式不给 --base。
    if "--base" in args:
        rev = args[args.index("--base") + 1] if args.index("--base") + 1 < len(args) else ""
    else:
        rev = ""
    rev = rev or os.environ.get("GATE_BASE", "")
    if rev:
        files = [f for f in files if f[0] in changed_since(root, rev)]
    key = os.environ.get("TYPESAFE_API_KEY")
    base = os.environ.get("TYPESAFE_API_BASE", "https://api.typesafe.ai").rstrip("/")
    model = os.environ.get("JEV_MODEL", "jev-latest")

    jobs = []   # (rule_id, rule, rel, content)
    for rel, content in files:
        for rid, rule in rules.items():
            # 配置里允许放 `_comment` 这类说明键（值是字符串）。只对 dict 规则
            # 调 file_matches，否则一条说明就能让 harness 崩掉——崩掉会被引擎
            # 记成 WARN，于是整条规则静默失效并报 ALL PASS（fail-open）。
            if not isinstance(rule, dict):
                continue
            if file_matches(rule, rel):
                jobs.append((rid, rule, rel, content))
    if not jobs:
        print(json.dumps([]))
        return

    judged, notes, missing_map = {}, {}, {}
    if key:
        with concurrent.futures.ThreadPoolExecutor(max_workers=POOL) as ex:
            futs = {}
            for i, (rid, rule, rel, content) in enumerate(jobs):
                q = rule.get("questions", {})
                st, missing_ctx = build_state(rule, rel, content, diff_for(root, rel), root)
                missing_map[i] = missing_ctx
                futs[i] = ex.submit(judge_one, base, key, model, st, q)
            for i, fut in futs.items():
                try:
                    judged[i] = fut.result()
                except Exception as e:  # noqa: BLE001 - judge failure degrades to a note
                    judged[i], notes[i] = None, f"{type(e).__name__}: {e}"[:120]
    else:
        for i in range(len(jobs)):
            judged[i], notes[i] = None, "TYPESAFE_API_KEY not set"

    findings = []
    for i, (rid, rule, rel, _c) in enumerate(jobs):
        r = judged.get(i)
        if missing_map.get(i):
            findings.append({"id": rid.upper(), "severity": "WARN", "path": rel, "line": 0,
                             "message": f"context file(s) missing: {', '.join(missing_map[i])}",
                             "tier": "context-missing"})
        if r is None:
            findings.append({"id": rid.upper(), "severity": "WARN", "path": rel, "line": 0,
                             "message": f"{rule.get('intent', rid)[:100]}",
                             "tier": "candidates-raw",
                             "note": f"unverified ({notes.get(i, '')}) - candidate from custom spec"})
            continue
        worst, extra = "INFO", {"tier": "jev"}
        for qid, q in rule.get("questions", {}).items():
            if qid not in r:
                continue
            sev = severity_for(q, r[qid])
            if sev == "FAIL":
                worst = "FAIL"
                extra[qid] = (f"{r[qid]:.2f}" if isinstance(r[qid], float) else str(r[qid][0]))
            elif sev == "WARN" and worst != "FAIL":
                worst = "WARN"
                extra[qid] = (f"{r[qid]:.2f}" if isinstance(r[qid], float) else str(r[qid][0]))
        if worst == "INFO":
            continue          # below warn - the false-positive filter
        # finding 的 message 用规则自带的 `message`（一句话+怎么改），没配才退回
        # intent 截断。intent 是给模型看的条文，200 字截断后对人没有可执行信息。
        findings.append({"id": rid.upper(), "severity": worst, "path": rel, "line": 0,
                         "message": rule.get("message") or rule.get("intent", rid)[:200],
                         **extra})
    print(json.dumps(findings))


if __name__ == "__main__":
    main()
