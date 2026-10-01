#!/usr/bin/env python3
"""jev_rule_batch - full-repo chunked wrapper over jev_rule.py.

Why this exists: the engine has no "feed every tracked file to a mode:file
harness" path, and jev_rule.py caps stdin at MAX_FILES=20 (silent truncation).
This wrapper is wired as mode:grep (stdin empty): it enumerates the tracked
file set, filters by each rule's paths_include/exclude, chunks <=16 files, and
invokes jev_rule.py per chunk with the SAME --config, merging findings.

Scope semantics: complement-dedup channel (2026-10-01). When the engine
exports GATE_BASE, files changed since the baseline are SKIPPED — the
incremental channel (checklist_web_spec, mode:file, diff scope) judges them
in the same gate run; skipping avoids double jev cost + duplicated findings
while the union still covers the whole repo. Without GATE_BASE (manual
local run) the audit is the full tracked set. An explicit --base overrides
the env. Judgment, thresholds, state caps and the no-key degradation
(candidates-raw WARN) are entirely jev_rule.py's concern; this file only
splits and merges.
"""

import json
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jev_rule  # reuse file_matches so both layers share matching semantics

CHUNK = 16          # stay under jev_rule.MAX_FILES (20)
MAX_FILES = 400     # per-rule global guard; exceeding emits an INFO finding


def main():
    args = sys.argv[1:]
    cfg = args[args.index("--config") + 1] if "--config" in args else ""
    try:
        with open(cfg) as f:
            rules = json.loads(f.read())
    except (OSError, json.JSONDecodeError) as e:
        print(json.dumps([{"id": "JEV-RULE-BATCH", "severity": "WARN", "path": ".", "line": 0,
                           "message": f"config unreadable ({e}); cannot run batched custom spec"}]))
        return

    root = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                          capture_output=True, text=True, check=False).stdout.strip() or os.getcwd()
    tracked = [p for p in subprocess.run(["git", "ls-files", "-z"], cwd=root,
                                         capture_output=True, check=False).stdout.decode("utf-8", "replace").split("\0") if p]

    # --base passthrough only when explicit (then strip GATE_BASE so the
    # explicit choice wins). GATE_BASE otherwise marks the diff boundary:
    # skip those files (judged by the incremental channel), keep the rest.
    env = dict(os.environ)
    sub_args = []
    if "--base" in args and args.index("--base") + 1 < len(args):
        sub_args = ["--base", args[args.index("--base") + 1]]
        env.pop("GATE_BASE", None)
    skip = set()
    gate_base = os.environ.get("GATE_BASE", "")
    if gate_base:
        out = subprocess.run(["git", "diff", "--name-only", f"{gate_base}..HEAD"],
                             cwd=root, capture_output=True, text=True, check=False).stdout
        skip = {q for q in out.splitlines() if q}

    script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "jev_rule.py")
    findings = []
    for rid, rule in rules.items():
        if not isinstance(rule, dict):
            continue
        sel = [p for p in tracked if jev_rule.file_matches(rule, p) and p not in skip]
        if not sel:
            continue
        if len(sel) > MAX_FILES:
            findings.append({"id": rid.upper(), "severity": "INFO", "path": ".", "line": 0,
                             "message": f"{len(sel)} files exceed batch cap {MAX_FILES}; "
                                        f"judging first {MAX_FILES} (git ls-files order)"})
            sel = sel[:MAX_FILES]
        for i in range(0, len(sel), CHUNK):
            chunk = sel[i:i + CHUNK]
            payload = "".join(f"===== FILE: {p} =====\n{_read(os.path.join(root, p))}\n"
                              for p in chunk)
            try:
                r = subprocess.run([sys.executable, script, "--config", cfg, *sub_args],
                                   input=payload, capture_output=True, text=True,
                                   cwd=root, env=env, timeout=1200, check=False)
                out = json.loads(r.stdout)
                if not isinstance(out, list):
                    raise TypeError("stdout is not a JSON array")
                findings.extend(out)
            except (OSError, subprocess.SubprocessError, json.JSONDecodeError, TypeError) as e:
                findings.append({"id": rid.upper(), "severity": "WARN", "path": ".", "line": 0,
                                 "message": f"batch chunk {i // CHUNK} failed ({e}); "
                                            f"files: {', '.join(chunk[:3])}…",
                                 "tier": "batch-error"})
    print(json.dumps(findings))


def _read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


if __name__ == "__main__":
    main()
