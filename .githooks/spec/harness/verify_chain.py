"""verify chain — jev verification cascade over l1 grep candidates.

Architecture: deterministic regexes stay the *candidate generator* (cheap,
high recall, milliseconds); Jev becomes the *judge* (semantic verification +
severity). A grep hit alone no longer decides severity.

Pipeline (per rule):
  stdin (gate mode:file, "===== FILE: rel =====" sections, changed files only)
    -> candidate extractor (per-rule regex/parse, this file)
    -> one TypeSafe request per candidate, all questions in that call,
       sent concurrently (ThreadPoolExecutor)
    -> thresholds from jev_questions_verify.json (per-question fail/warn)
    -> p >= fail -> FAIL, p >= warn -> WARN, else dropped (that drop IS the
       false-positive filter)

Degradation (never lose recall, never block on infrastructure):
  - no TYPESAFE_API_KEY, or every request failed
      -> all candidates emitted as WARN with extra {tier: "candidates-raw"};
         disposition still required, nothing silently vanishes
  - some requests failed -> those candidates emitted raw-WARN, judged ones
    keep their tier

Gate protocol: stdout = findings JSON array; extras {tier, confidence,
question}. Env: TYPESAFE_API_KEY / TYPESAFE_API_BASE / JEV_MODEL (same as
review_chain.py).

Usage (from checklist yaml, mode: file):
  python3 verify_chain.py --rule hardcoded_secret
  python3 verify_chain.py --rule slop_comment
  python3 verify_chain.py --rule rust_test_no_assert
"""
import concurrent.futures
import json
import os
import re
import sys
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG = os.path.join(HERE, "jev_questions_verify.json")
TIMEOUT_HTTP = 30
MAX_CANDIDATES = 30
CAP_STATE = 6000
POOL = 8

SECRET_PAT = re.compile(
    r"(password|passwd|secret|api_key|apikey|token|auth_token|access_token"
    r"|private_key|connection_string)\s*[:=]\s*[\"'][^\"'\s]{8,}[\"']", re.I)
SECRET_EXCLUDE = re.compile(
    r"(your-|xxx|todo|placeholder|example|test-key|mock|fake|dummy"
    r"|change-me|replace-me|insert-|changeme|replaceme)", re.I)
SLOP_PATS = [
    re.compile(r"\bStep\s*\d+\b"),
    re.compile(r"\b[一二三四五六七八九十]+\s*步\b"),
    re.compile(r"\b[首先然后接着最后]\b.*\b[首先然后接着最后]\b"),
    re.compile(r"\bThis\s+(function|method|class|module)\b"),
    re.compile(r"该函数|该模块|该类"),
]
TEST_FN = re.compile(r"#\[test\]")
FN_NAME = re.compile(r"^\s*fn\s+(\w+)")


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


def candidates_hardcoded_secret(files):
    out = []
    for path, content in files:
        for i, line in enumerate(content.splitlines(), 1):
            if SECRET_PAT.search(line) and not SECRET_EXCLUDE.search(line):
                out.append({"path": path, "line": i, "excerpt": line.strip()[:300]})
    return out


def candidates_slop_comment(files):
    out = []
    for path, content in files:
        lines = content.splitlines()
        for i, line in enumerate(lines, 1):
            stripped = line.strip()
            if not (stripped.startswith("//") or stripped.startswith("#")
                    or stripped.startswith("*") or stripped.startswith("///")):
                continue
            code = re.sub(r"^[/*#\s]+", "", stripped)
            if len(code) < 8:
                continue
            if any(p.search(code) for p in SLOP_PATS):
                ctx = "\n".join(lines[max(0, i - 3):i + 2])[:800]
                out.append({"path": path, "line": i,
                            "excerpt": stripped[:300], "context": ctx})
    return out


def candidates_rust_test_no_assert(files):
    out = []
    for path, content in files:
        if not path.endswith(".rs"):
            continue
        lines = content.splitlines()
        i = 0
        while i < len(lines):
            if TEST_FN.search(lines[i]):
                fn_name, j, body = "", i, []
                k = i + 1
                while k < len(lines):
                    m = FN_NAME.match(lines[k])
                    if m and not fn_name:
                        fn_name = m.group(1)
                    body.append(lines[k])
                    if lines[k].strip() == "}" and k > i + 1:
                        break
                    k += 1
                text = "\n".join(body)
                if not re.search(r"\bassert", text):
                    out.append({"path": path, "line": i + 1,
                                "excerpt": f"fn {fn_name} (no literal assert)",
                                "context": text[:2000]})
                i = k
            i += 1
    return out



# --- antislop: 五类词法候选（占位/拖延/对冲/搁置/空桩），jev 判合法用法 ---
# placeholder-marker(TODO/FIXME) 不在此列: 裸 TODO 的执法归 rust_todo_needs_issue
# （MECE——antislop 只抓其他规则不覆盖的拖延/对冲/搁置/空桩）
ANTISLOP_RES = [
    ("deferral",           re.compile(r"\b(for now|temporar\w*|provisional)\b", re.I)),
    ("placeholder-word",   re.compile(r"\b(placeholder|stub|dummy)\b", re.I)),
    ("hedging",            re.compile(r"\b(hopefully|should work|works in theory|might not)\b", re.I)),
    ("revisit",            re.compile(r"\b(revisit|reconsider|circle back)\b", re.I)),
    ("rust-stub",          re.compile(r"\bfn\s+\w+[^{;]*\{\s*\}")),
]


def candidates_antislop(files):
    out = []
    for path, content in files:
        for lineno, line in enumerate(content.splitlines(), 1):
            for cat, pat in ANTISLOP_RES:
                m = pat.search(line)
                if m:
                    out.append({"path": path, "line": lineno,
                                "excerpt": f"[{cat}] {line.strip()[:200]}"})
                    break          # 一行一候选：命中即算，避免同类刷屏
    return out


# --- duplication: 4+ 连续非空行块(>80字符)在本次变更内重复出现 ---
def candidates_duplication(files):
    import hashlib
    blocks = {}
    for path, content in files:
        if not path.endswith(".rs") or "/tests/" in path or "test_" in path.rsplit("/", 1)[-1]:
            continue
        lines = content.splitlines()
        i = 0
        while i < len(lines):
            if lines[i].strip():
                j = i
                while j < len(lines) and lines[j].strip():
                    j += 1
                block = tuple(l.rstrip() for l in lines[i:j])
                if len(block) >= 4 and len("\n".join(block)) > 80:
                    key = hashlib.md5("\n".join(block).encode()).hexdigest()
                    blocks.setdefault(key, []).append((path, i + 1, block))
                i = j
            else:
                i += 1
    out = []
    for occ in blocks.values():
        if len(occ) >= 2:
            out.append({"path": occ[0][0], "line": occ[0][1],
                        "excerpt": f"{len(occ)}x identical block: {', '.join(o[0] for o in occ)}",
                        "block": "\n".join(occ[0][2])[:400]})
    return out

EXTRACTORS = {
    "hardcoded_secret": candidates_hardcoded_secret,
    "slop_comment": candidates_slop_comment,
    "rust_test_no_assert": candidates_rust_test_no_assert,
    "antislop": candidates_antislop,
    "duplication": candidates_duplication,
}


def post(url, headers, body, timeout):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={**headers, "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())


def judge_one(rule_cfg, cand):
    state = {"rule_intent": rule_cfg.get("intent", ""), **cand}
    state = {k: (v[:CAP_STATE] if isinstance(v, str) else v) for k, v in state.items()}
    key = os.environ.get("TYPESAFE_API_KEY")
    base = os.environ.get("TYPESAFE_API_BASE", "https://api.typesafe.ai").rstrip("/")
    model = os.environ.get("JEV_MODEL", "jev-latest")
    payload = post(f"{base}/v1/systemone",
                   {"Authorization": f"Bearer {key}", "x-api-key": key},
                   {"state": state, "model": model, "questions": rule_cfg["questions"]},
                   TIMEOUT_HTTP)
    out = {}
    for qid, ans in (payload.get("answers") or {}).items():
        t = ans.get("type")
        if t == "noul":
            out[qid] = float(ans.get("noul", 0.0))
        elif t == "score":
            out[qid] = float(ans.get("score", 0.0))   # 0-indexed, continuous
        elif t == "choice":
            probs = ans.get("probabilities", {}) or {}
            out[qid] = max(probs.values()) if probs else 0.0
    return out


def finding(rule, cand, severity, extra):
    f = {"id": f"{rule.upper().replace('_','-')}", "severity": severity,
         "path": cand["path"], "line": cand["line"],
         "message": f"{rule}: {cand.get('excerpt', '')[:160]}"}
    f.update(extra)
    return f


def main():
    args = sys.argv[1:]
    rule = args[args.index("--rule") + 1] if "--rule" in args else ""
    cfg = json.loads(open(CONFIG).read())
    if rule not in cfg or rule not in EXTRACTORS:
        print(json.dumps([])); return
    rule_cfg = cfg[rule]
    files = parse_files(sys.stdin.read())
    cands = EXTRACTORS[rule](files)[:MAX_CANDIDATES]
    if not cands:
        print(json.dumps([])); return

    judged, notes = {}, {}
    key = os.environ.get("TYPESAFE_API_KEY")
    if key:
        with concurrent.futures.ThreadPoolExecutor(max_workers=POOL) as ex:
            futs = {i: ex.submit(judge_one, rule_cfg, c) for i, c in enumerate(cands)}
            for i, fut in futs.items():
                try:
                    judged[i] = fut.result()
                except Exception as e:
                    judged[i], notes[i] = None, f"{type(e).__name__}: {e}"[:120]
    else:
        for i in range(len(cands)):
            judged[i], notes[i] = None, "TYPESAFE_API_KEY not set"

    findings = []
    for i, c in enumerate(cands):
        r = judged.get(i)
        if r is None:
            findings.append(finding(rule, c, "WARN", {
                "tier": "candidates-raw",
                "note": f"unverified ({notes.get(i, '')}) - candidate from deterministic scan"}))
            continue
        worst, extra = "INFO", {"tier": "jev"}
        for qid, q in rule_cfg["questions"].items():
            if qid not in r:
                continue
            p = r[qid]
            fail, warn = float(q.get("fail", 1.1)), float(q.get("warn", 1.1))
            if p >= fail:
                worst = "FAIL"
                extra[qid] = f"{p:.2f}"
            elif p >= warn and worst != "FAIL":
                worst = "WARN"
                extra[qid] = f"{p:.2f}"
        if worst == "INFO":
            continue          # below warn threshold - the false-positive filter
        findings.append(finding(rule, c, worst, extra))
    print(json.dumps(findings))


if __name__ == "__main__":
    main()
