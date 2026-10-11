#!/usr/bin/env python3
"""delivery chain — verify PR `Delivery record` claims against diff evidence.

canon's PR spec requires every review finding to be dispositioned in the
`## Delivery record` section (fixed, or rejected in writing), but the gate only
ever checked its *shape* — section present, checkbox count. The content was
decoration. This is the L3 check: the branch diff is the evidence, jev judges
each delivery claim against it (the claims-vs-evidence shape of jev_verify,
the one capability this repo never used).

Gate protocol (mode: diff):
  stdin  = branch diff
  stdout = findings JSON array, extras {tier, confidence, claim}

PR body source, first wins:
  1. GATE_PR_BODY_FILE — a file path (test hook, so tests need neither gh nor
     the network);
  2. `gh pr view --json body -q .body` (subprocess).
  Failure or empty body -> a single INFO "no PR context, skipped", exit 0.

Claims: a two-layer extraction. Layer 1 (candidates) grabs every non-empty
fragment of the Delivery record section — bullets/numbered items, bold lines,
table rows, and regular paragraphs — verbatim, at most MAX_CANDIDATES.
Layer 2 (jev pick) is one batched choice call: the model picks which
candidates are rule dispositions (p >= EXTRACT_CUTOFF counts) and never
rewrites them, so final claims stay verbatim regex fragments in original
order. If the jev tier is unavailable the layer falls back to the plain
regex extraction (extract_claims) plus a single INFO "claims via regex
fallback" — recall is preserved. Final claims are capped at MAX_CLAIMS;
overflow is reported explicitly as INFO rather than silently truncated.

Direction: each claim question is a noul whose true = "this claim is unsupported
by, or contradicted by, the diff evidence" (HIGH = BAD, same direction as
severity). The question must be phrased as the declarative negative proposition
above, never as a "Does the evidence support...?" question: the model reads
true through the question's own wording, so an interrogative flips the verdict
(measured: real claims 0.79/0.82 vs fabricated claim low — inverted; declarative
gives fabricated 0.92 vs real 0.27). Thresholds from jev_questions_delivery.json
(0.85 fail / 0.6 warn); p >= fail -> FAIL, p >= warn -> WARN, else INFO.

Degradation (policy layer, DWJ contract): jev tier only — endpoint pair via
review_chain/jev_env (JEV_API_* wins, TYPESAFE_* fallback). Any
infrastructure failure (no key / network / unparseable answers) degrades to a
single INFO finding; this layer never blocks on its own infrastructure.

Manual: canon check delivery_claims
"""
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import review_chain  # 同族复用 post()/finding()/MAX_STATE_CHARS

QUESTIONS = "jev_questions_delivery.json"
MAX_CLAIMS = 12

# `## Delivery record` and friends; the section ends at the next heading of
# any level, so a missing closing heading truncates at end-of-body (never at
# "the next Delivery-ish sentence").
SECTION_RE = re.compile(r"^#{1,6}\s*delivery\s*record\b", re.IGNORECASE)
HEADING_RE = re.compile(r"^#{1,6}\s")
BULLET_RE = re.compile(r"^\s*(?:[-*+]|\d+\.)\s+(.*\S)")
# A loose sentence is still a claim ("Rejected: retained because ..."), but a
# paragraph that is really a paragraph gets collapsed to its first line: jev
# judges the claim, not the prose around it.
MAX_PARA_CHARS = 300
# Unconventional dispositions (bold line / table row / nested bullet) are still
# dispositions; the old bullet-only regex either dropped or glued them into
# surrounding prose. Candidates are extracted verbatim — the model only picks
# which ones are dispositions, it never rewrites them.
BOLD_LINE_RE = re.compile(r"^\s*(?:\*\*(.+?)\*\*|__(.+?)__)\s*$")
TABLE_ROW_RE = re.compile(r"^\s*\|.*\|\s*$")
TABLE_SEP_RE = re.compile(r"^\s*\|(?:\s*:?-+:?\s*\|)+\s*$")
MAX_CANDIDATES = 40
# p(disposition) at/above this line counts as a claim (choice question, the
# sum over fail_labels that run_jev already computes for us).
EXTRACT_CUTOFF = 0.6


def info(fid: str, message: str, extra: dict | None = None) -> dict:
    f = {"id": fid, "severity": "INFO", "path": "", "line": 0, "message": message}
    if extra:
        f.update(extra)
    return f


def find_pr_body() -> tuple:
    """Return (body, source). body == "" means "no PR context, skip"."""
    hook = os.environ.get("GATE_PR_BODY_FILE")
    if hook:
        try:
            with open(hook, encoding="utf-8") as fh:
                return fh.read(), f"file:{hook}"
        except OSError as e:
            return "", f"GATE_PR_BODY_FILE unreadable ({e})"
    try:
        out = subprocess.run(
            ["gh", "pr", "view", "--json", "body", "-q", ".body"],
            capture_output=True, text=True, timeout=20, check=True,
        )
    except Exception as e:  # noqa: BLE001 - no gh / no network / no PR is a skip, not a block
        return "", f"gh pr view failed ({e})"
    return out.stdout, "gh"


def extract_claims(body: str) -> list:
    """Bullets and short paragraphs of the Delivery record section, in order."""
    lines = body.splitlines()
    start = next((i for i, ln in enumerate(lines) if SECTION_RE.match(ln)), None)
    if start is None:
        return []
    end = next(
        (i for i in range(start + 1, len(lines)) if HEADING_RE.match(lines[i])), len(lines)
    )
    claims: list = []
    para: list = []
    for raw in lines[start + 1 : end]:
        m = BULLET_RE.match(raw)
        if m:
            if para:
                claims.append(" ".join(para))
                para = []
            claims.append(m.group(1).strip())
        elif raw.strip():
            para.append(raw.strip())
        elif para:
            claims.append(" ".join(para))
            para = []
    if para:
        claims.append(" ".join(para))
    return [c[:MAX_PARA_CHARS] for c in claims if c.strip()]


def _section_span(body: str):
    """(start, end) of the Delivery record section; None when absent."""
    lines = body.splitlines()
    start = next((i for i, ln in enumerate(lines) if SECTION_RE.match(ln)), None)
    if start is None:
        return None
    end = next(
        (i for i in range(start + 1, len(lines)) if HEADING_RE.match(lines[i])), len(lines)
    )
    return start, end


def extract_candidates(body: str) -> list:
    """Every non-empty fragment of the Delivery record section, in order.

    Bullets/numbered items, bold lines, table rows (separator rows dropped),
    and regular paragraphs. This is the candidate pool the model picks from;
    it is *wider* than extract_claims on purpose — recall first, then let the
    judgment layer prune. Values stay verbatim regex fragments (markers
    stripped, each truncated to MAX_PARA_CHARS, capped at MAX_CANDIDATES).
    """
    span = _section_span(body)
    if span is None:
        return []
    start, end = span
    lines = body.splitlines()
    candidates: list = []
    para: list = []
    for raw in lines[start + 1 : end]:
        s = raw.strip()
        if not s:
            if para:
                candidates.append(" ".join(para))
                para = []
            continue
        bullet = BULLET_RE.match(raw)
        if bullet:
            if para:
                candidates.append(" ".join(para))
                para = []
            candidates.append(bullet.group(1).strip())
            continue
        bold = BOLD_LINE_RE.match(raw)
        if bold:
            if para:
                candidates.append(" ".join(para))
                para = []
            candidates.append((bold.group(1) or bold.group(2)).strip())
            continue
        if TABLE_ROW_RE.match(raw):
            if TABLE_SEP_RE.match(raw):
                continue  # |---|---| is formatting, not content
            if para:
                candidates.append(" ".join(para))
                para = []
            candidates.append(s)
            continue
        para.append(s)
    if para:
        candidates.append(" ".join(para))
    out = [c[:MAX_PARA_CHARS] for c in candidates if c.strip()]
    return out[:MAX_CANDIDATES]


def pick_claims(candidates: list, d_fail: float, d_warn: float) -> tuple:
    """Batch choice call: one cand_<i> per candidate, keep p(disposition)>=0.6.

    The model only picks (disposition vs prose); it never writes the value,
    so the returned claims are verbatim regex fragments in original order.
    Returns ([claims], None) on success, (None, err) when the jev tier is
    unavailable so the caller falls back to regex extraction.
    """
    if not candidates:
        return [], None
    questions = {}
    for i, cand in enumerate(candidates):
        questions[f"cand_{i}"] = {
            "type": "choice",
            "instructions": (
                "This delivery-record fragment is a rule disposition (a rule id, "
                "root cause, fix, or written rejection) rather than explanatory "
                f"prose. Fragment: `{cand}`"
            ),
            "criteria": {
                "disposition": "states a rule id, root cause, fix, or written rejection",
                "prose": "explanatory prose, template phrasing, title restatement, or progress report",
            },
            "fail_labels": ["disposition"],
        }
    findings, err = review_chain.run_jev(
        _section_text(candidates), questions, d_fail, d_warn
    )
    if findings is None:
        return None, err
    by_id = {f["id"]: f for f in findings}
    kept: list = []
    for i, cand in enumerate(candidates):
        f = by_id.get(f"REVIEW-CAND_{i}")
        if not f:
            continue
        p = float(f["confidence"])
        if p >= EXTRACT_CUTOFF:
            kept.append(cand)
    return kept, None


def _section_text(candidates: list) -> str:
    # Feed the judge the candidates themselves as state; the fragment is
    # re-stated in each question, so a compact joined view keeps state small.
    return "\n".join(candidates)[: review_chain.MAX_STATE_CHARS]


def load_claims(claims: list) -> dict:
    with open(os.path.join(HERE, QUESTIONS), encoding="utf-8") as fh:
        qfile = json.load(fh)
    fail = float(qfile.get("default_fail", 0.85))
    warn = float(qfile.get("default_warn", 0.6))
    questions = {}
    for i, claim in enumerate(claims):
        # Declarative negative proposition: the instruction text must agree with
        # criteria.true, because the model reads "true" through the question's
        # own wording. An interrogative ("Does the evidence support...?") makes
        # true=supported, which flips every verdict against the criteria (measured
        # on the same endpoint: real claims scored high, fabricated low).
        questions[f"claim_{i}"] = {
            "type": "noul",
            "instructions": (
                "This delivery claim is unsupported by the diff evidence, or "
                f"contradicted by it. Claim: `{claim}` Judge only what the "
                "evidence shows, not what the claim asserts."
            ),
            "criteria": {
                "true": "evidence is absent, unrelated, or contradicts the claim",
                "false": "evidence contains the change the claim describes",
            },
            "fail": fail,
            "warn": warn,
        }
    for k, v in qfile.items():
        if isinstance(v, dict) and "type" in v:
            questions[k] = v
    return questions


def build_state(claims: list, diff: str) -> dict:
    # Same reasoning as review_chain: a diff is chronological, so its tail
    # carries the newest hunk; a truncated head would cut the very changes
    # under review.
    evidence = diff
    if len(evidence) > review_chain.MAX_STATE_CHARS:
        evidence = evidence[-review_chain.MAX_STATE_CHARS :]
    return {
        "claims": [review_chain.wrap_untrusted(c) for c in claims],
        "diff_evidence": review_chain.wrap_untrusted(evidence),
    }


def judge(state: dict, questions: dict) -> tuple:
    findings, err = review_chain.run_jev(json.dumps(state), questions, 0.85, 0.6)
    if findings is None:
        return None, err
    by_id = {f["id"]: f for f in findings}
    out = []
    for i, raw_claim in enumerate(state["claims"]):
        claim = review_chain.unwrap_untrusted(raw_claim)
        f = by_id.get(f"REVIEW-CLAIM_{i}")
        if not f or f["severity"] == "INFO":
            continue
        out.append(
            {
                "id": f"DELIVERY-CLAIM-{i}",
                "severity": f["severity"],
                "path": "",
                "line": 0,
                "message": f"声明未被证据支持 (p={f['confidence']}): {claim[:80]}",
                "tier": "jev",
                "confidence": f["confidence"],
                "claim": claim,
            }
        )
    ev = by_id.get("REVIEW-EVIDENCE_SUFFICIENT")
    if ev and ev["severity"] != "INFO":
        out.append(
            {
                "id": "DELIVERY-EVIDENCE",
                "severity": ev["severity"],
                "path": "",
                "line": 0,
                "message": (
                    f"diff 证据不足以支撑 Delivery record 声明 (p={ev['confidence']})"
                    "；补可核对的改动，或把驳回写清楚"
                ),
                "tier": "jev",
                "confidence": ev["confidence"],
                "claim": "",
            }
        )
    return out, None


def main() -> int:
    diff = sys.stdin.read()
    body, source = find_pr_body()
    if not body.strip():
        json.dump([info("DELIVERY-CLAIMS", f"无 PR 上下文，跳过 delivery claims 核验（{source}）")], sys.stdout)
        return 0
    notes = []
    candidates = extract_candidates(body)
    picked, pick_err = pick_claims(candidates, 0.85, 0.6)
    if picked is None:
        # jev 不可用: 退回既有正则抽取 (召回不丢) + 单条 INFO, 与降级哲学一致
        claims = extract_claims(body)
        notes.append(
            info(
                "DELIVERY-CLAIMS-FALLBACK",
                f"jev 抽取不可用，claims via regex fallback 不阻断（{pick_err}）",
            )
        )
    else:
        claims = picked
    if len(claims) < 2:
        json.dump(
            notes + [info("DELIVERY-CLAIMS", f"Delivery record 无可核验声明（{len(claims)} 条），跳过")],
            sys.stdout,
        )
        return 0
    if len(claims) > MAX_CLAIMS:
        notes.append(
            info(
                "DELIVERY-CLAIMS-TRUNCATED",
                f"Delivery record 共 {len(claims)} 条声明，只核验前 {MAX_CLAIMS} 条，其余未入题",
            )
        )
        claims = claims[:MAX_CLAIMS]

    findings, err = judge(build_state(claims, diff), load_claims(claims))
    if findings is None:
        print(f"delivery_chain: {err}", file=sys.stderr)
        json.dump(
            notes + [info("DELIVERY-CLAIMS", f"jev 不可用，降级 INFO 不阻断（{err}）")], sys.stdout
        )
        return 0
    json.dump(notes + findings, sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
