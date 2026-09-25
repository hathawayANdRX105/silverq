#!/usr/bin/env python3
"""review chain — tiered model review with graceful degradation.

Tier order (first available wins):
  1. jev (TypeSafe System One)   — TYPESAFE_API_KEY (+ TYPESAFE_API_BASE/JEV_MODEL)
       calibrated probabilities per typed question; 100-500ms
  2. small LLM (OpenAI-compat)   — REVIEW_LLM_BASE_URL + REVIEW_LLM_API_KEY + REVIEW_LLM_MODEL
       same findings schema via one constrained chat call
  3. none                         — INFO note; deterministic tool gates still apply

Per-question thresholds (confidence-gated routing): each question may carry
"fail"/"warn" floats in the questions JSON; absent → harness defaults.
    p(issue) >= fail -> FAIL   (hard block: must fix or split before push)
    p(issue) >= warn -> WARN   (must be dispositioned: fix, or written rejection
                                 with evidence recorded in the PR review log)
    otherwise        -> INFO   (confidence carried; batchable backlog, no per-item disposition)

Gate protocol: stdin = diff (mode: diff); stdout = findings JSON array with
extras {tier, confidence, question}. Any infra failure (no key / network /
unparseable) degrades a tier down or to INFO — never blocks on infrastructure.

Env:
  TYPESAFE_API_KEY / TYPESAFE_API_BASE / JEV_MODEL
  REVIEW_LLM_BASE_URL / REVIEW_LLM_API_KEY / REVIEW_LLM_MODEL
"""
import json
import os
import sys
import urllib.error
import urllib.request

TIMEOUT = {"jev": 55, "llm": 120}

# Docs guidance: "include only the context relevant to the current
# questions" — a full merge-base diff (hundreds of KB) is context rot and
# trips upstream body-size limits (observed: jev API 400 on 269KB state).
MAX_STATE_CHARS = 24000


def post(url: str, headers: dict, body: dict, timeout: int):
    req = urllib.request.Request(
        url, data=json.dumps(body).encode(), headers={"Content-Type": "application/json", **headers}
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


def finding(qid: str, p: float, fail: float, warn: float, tier: str, instructions: str) -> dict:
    sev = "FAIL" if p >= fail else ("WARN" if p >= warn else "INFO")
    return {
        "id": f"REVIEW-{qid.upper()}",
        "severity": sev,
        "path": "",
        "line": 0,
        "message": (
            f"{tier}: {qid} p(issue)={p:.2f} "
            f"(fail>={fail}, warn>={warn}) — "
            + ("BLOCK: fix or split before push" if sev == "FAIL"
               else "DISPOSITION REQUIRED: fix this, or record a written rejection with evidence in the PR review log (silent skip = violation)" if sev == "WARN"
               else "reference only; fold into backlog, no per-item disposition needed")
        ),
        "tier": tier,
        "confidence": f"{p:.2f}",
        "question": json.dumps(instructions)[:300],
    }


def thresholds(q: dict, d_fail: float, d_warn: float) -> tuple:
    return float(q.get("fail", d_fail)), float(q.get("warn", d_warn))


def run_jev(diff: str, questions: dict, d_fail: float, d_warn: float):
    key = os.environ.get("TYPESAFE_API_KEY")
    if not key:
        return None, "jev: TYPESAFE_API_KEY not set"
    base = os.environ.get("TYPESAFE_API_BASE", "https://api.typesafe.ai").rstrip("/")
    model = os.environ.get("JEV_MODEL", "jev-latest")
    try:
        payload = post(
            f"{base}/v1/systemone",
            {"Authorization": f"Bearer {key}", "x-api-key": key},
            {"state": diff, "model": model, "questions": questions},
            TIMEOUT["jev"],
        )
    except Exception as e:
        return None, f"jev request failed: {e}"
    out = []
    for qid, ans in (payload.get("answers") or {}).items():
        q = questions.get(qid, {})
        fail, warn = thresholds(q, d_fail, d_warn)
        t = ans.get("type")
        if t == "noul":
            p = float(ans.get("noul", 0.0))
        elif t == "choice":
            probs = ans.get("probabilities", {}) or {}
            p = min(sum(v for k, v in probs.items() if k.endswith("_bad") or k == "problematic"), 1.0)
        elif t == "score":
            p = float(ans.get("score", 0.0)) / 9.0
        else:
            p = 0.0
        out.append(finding(qid, p, fail, warn, "jev", q.get("instructions", "")))
    return out, None


def run_small_llm(diff: str, questions: dict, d_fail: float, d_warn: float):
    base = os.environ.get("REVIEW_LLM_BASE_URL")
    model = os.environ.get("REVIEW_LLM_MODEL")
    if not base or not model:
        return None, "small-llm: REVIEW_LLM_BASE_URL/REVIEW_LLM_MODEL not set"
    key = os.environ.get("REVIEW_LLM_API_KEY", "")
    prompt = (
        "Judge the diff. For EACH question id reply with one JSON array entry "
        '[{"id": "<qid>", "p": <0..1 probability the issue applies>, "why": "..."}]. '
        "Reply with the JSON array only, no prose.\n\n"
        f"QUESTIONS: {json.dumps(questions)}\n\nDIFF:\n{diff[:12000]}"
    )
    try:
        payload = post(
            f"{base.rstrip('/')}/v1/chat/completions",
            {"Authorization": f"Bearer {key}", "x-api-key": key},
            {"model": model, "messages": [{"role": "user", "content": prompt}], "temperature": 0},
            TIMEOUT["llm"],
        )
        content = payload["choices"][0]["message"]["content"]
    except Exception as e:
        return None, f"small-llm request failed: {e}"
    try:
        start, end = content.index("["), content.rindex("]")
        verdicts = {v["id"]: float(v["p"]) for v in json.loads(content[start : end + 1])}
    except Exception as e:
        return None, f"small-llm response unparseable: {e}"
    out = []
    for qid, q in questions.items():
        if qid not in verdicts:
            continue
        fail, warn = thresholds(q, d_fail, d_warn)
        out.append(finding(qid, verdicts[qid], fail, warn, "small-llm", q.get("instructions", "")))
    return out, None


def load_questions(qpath: str, d_fail: float, d_warn: float, state):
    """Question set construction.

    Two modes:
    - review mode (state is a diff string): every top-level entry with a
      "type" is a static question from the file.
    - done-when mode (state is a JSON object with done_when_items): each
      acceptance item becomes its own atomic question (`item_N`, threshold
      from `default_fail`/`default_warn` in the file), plus any static
      questions the file defines (evidence sufficiency, verdict).
    """
    with open(qpath) as fh:
        qfile = json.load(fh)

    if not isinstance(state, dict) or "done_when_items" not in state:
        return {k: v for k, v in qfile.items() if isinstance(v, dict) and "type" in v}

    i_fail = float(qfile.get("default_fail", d_fail))
    i_warn = float(qfile.get("default_warn", d_warn))
    questions = {}
    for i, item in enumerate(state["done_when_items"]):
        questions[f"item_{i}"] = {
            "type": "noul",
            "instructions": (
                f"Is this acceptance item satisfied by the evidence? Item: `{item}` "
                "Judge only what the evidence shows, not what it claims."
            ),
            "criteria": {"true": "evidence demonstrates the item is done", "false": "not demonstrated"},
            "fail": i_fail,
            "warn": i_warn,
        }
    for k, v in qfile.items():
        if isinstance(v, dict) and "type" in v:
            questions[k] = v
    return questions


def main() -> int:
    qpath = sys.argv[1] if len(sys.argv) > 1 else ""
    d_fail = float(sys.argv[2]) if len(sys.argv) > 2 else 0.8
    d_warn = float(sys.argv[3]) if len(sys.argv) > 3 else 0.5
    raw = sys.stdin.read()
    if not qpath or not raw.strip():
        print("[]")
        return 0
    try:
        state = json.loads(raw)
        if not isinstance(state, dict):
            state = raw.strip()
    except Exception:
        state = raw.strip()
    questions = load_questions(qpath, d_fail, d_warn, state)
    if isinstance(state, dict):
        # JSON state: keep the head (items array lives there).
        payload = json.dumps(state)[:MAX_STATE_CHARS]
    else:
        # Diff string: chronological, so the tail carries the latest changes.
        payload = state[-MAX_STATE_CHARS:] if len(state) > MAX_STATE_CHARS else state

    reasons = []
    for tier, run in (("jev", run_jev), ("small-llm", run_small_llm)):
        findings, err = run(payload, questions, d_fail, d_warn)
        if findings is not None:
            print(f"review_chain: tier={tier}", file=sys.stderr)
            json.dump(findings, sys.stdout)
            return 0
        if err:
            reasons.append(err)
            print(f"review_chain: {err}", file=sys.stderr)

    # No model tier: deterministic tool gates still cover the code-level checks.
    json.dump(
        [
            {
                "id": "REVIEW-CHAIN",
                "severity": "INFO",
                "path": "",
                "line": 0,
                "message": (
                    "no model tier available — "
                    + "; ".join(reasons)
                    + "；确定性工具检查 (ccn, duplication, antislop, slop_comment) 仍生效"
                ),
                "tier": "none",
            }
        ],
        sys.stdout,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
