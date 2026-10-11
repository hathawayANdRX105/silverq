#!/usr/bin/env python3
"""ccn gate — per-function cyclomatic ceiling with a rename-aware ratchet.

Mechanism reference: crapkit (CRAP / ccn ceiling / ratchet, marks may only fall).
Gate protocol: stdout = JSON array of {"id","severity","path","line","message"}.

Usage:
  ccn_gate.py seed  [--root R] [--ceiling 6]            # book whole-repo debt -> ratchet.tsv
  ccn_gate.py judge [--root R] [--ceiling 6] --stdin-files   # mode:file stdin (===== FILE: rel =====)
  ccn_gate.py judge [--root R] [--ceiling 6] --changed a.rs b.rs

Semantics (shared ratchet ledger, see ratchet_lib):
  - function ccn > ceiling and NOT in ratchet  -> FAIL (new violation)
  - function ccn > ceiling, in ratchet, rose   -> FAIL (worsened)
  - function ccn > ceiling, in ratchet, same   -> tolerated debt (silent)
  - marked function fell below its mark        -> mark auto-lowers / clears (marks may only fall)
  - a zero-complexity rename/move inherits its mark (body-hash match) and is
    NOT dropped and re-reported as "new"

Ledger is 3-column `key\tccn\tbody_hash` (2-column legacy rows still read; a row
without a hash simply can't inherit across a rename).

Dependency: `lizard`. If not importable, this harness degrades to a silent skip
(prints [] / exits 0 for judge; exits non-zero for seed) so a repo that has not
installed lizard is not blocked — the FAIL severity only bites once lizard is
present.
"""
import argparse
import json
import sys
from pathlib import Path

from ratchet_lib import (
    RATCHET,
    Entry,
    JudgeFinding,
    Mark,
    function_body_hash,
    git_files,
    judge_batch,
    load_marks,
    resolve_root,
    save_marks,
)

try:
    import lizard
except ImportError:  # pragma: no cover - environment-dependent
    lizard = None

CODE_EXTS = {
    ".rs", ".ts", ".tsx", ".js", ".py", ".go", ".java", ".c", ".cpp", ".h",
    ".hpp", ".zig", ".swift", ".sh", ".ps1", ".vue",
}


def is_code(rel: str) -> bool:
    return Path(rel).suffix.lower() in CODE_EXTS


def _skip(rel: str, exc: BaseException) -> None:
    """Unparseable input is skipped, never a finding: note it on stderr."""
    print(f"ccn_gate: skip {rel}: {type(exc).__name__}: {exc}", file=sys.stderr)


def collect_over(root: Path, rel: str, ceiling: int) -> list[tuple[str, Entry]]:
    """[(key, Entry)] for the functions over the ceiling in file rel.

    Empty if the file is missing/unparseable (skipped, never a finding). Each
    Entry carries a name-free body hash so a pure rename can inherit its debt.
    """
    path = root / rel
    if not path.is_file() or not is_code(rel):
        return []
    try:
        result = lizard.analyze_file(str(path))
    except Exception as exc:  # noqa: BLE001 - foreign input parse failure, not a finding
        _skip(rel, exc)
        return []
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except Exception:  # noqa: BLE001 - unreadable body just disables inheritance for this file
        lines = []
    out: list[tuple[str, Entry]] = []
    for fn in result.function_list:
        ccn = int(fn.cyclomatic_complexity)
        if ccn <= ceiling:
            continue
        bh = (
            function_body_hash(lines, fn.name, fn.start_line, fn.end_line)
            if lines else None
        )
        out.append((f"{rel}::{fn.name}", Entry(ccn, bh, fn.start_line)))
    return out


def _finding(rel: str, f: JudgeFinding, ceiling: int) -> dict:
    name = f.key.split("::", 1)[1]
    if f.kind == "worsened":
        msg = f"{name} ccn={f.value} > ratchet mark {f.mark} — worsened, decompose"
    else:
        msg = f"{name} ccn={f.value} > ceiling {ceiling} — new violation, decompose"
    return {"id": "CCN-OVER", "severity": "FAIL", "path": rel, "line": f.line, "message": msg}


def cmd_seed(args: argparse.Namespace) -> None:
    if lizard is None:
        print("ccn_gate: lizard not available (pip install lizard) — cannot seed", file=sys.stderr)
        sys.exit(2)
    root = resolve_root(args.root)
    marks = load_marks(root, RATCHET)
    count = 0
    for rel in git_files(root, ""):
        for key, entry in collect_over(root, rel, args.ceiling):
            marks[key] = Mark(
                max(entry.value, marks[key].value if key in marks else 0), entry.body_hash
            )
            count += 1
    save_marks(root, marks, RATCHET)
    print(f"seeded {count} over-ceiling mark(s) -> {root / RATCHET}", file=sys.stderr)


def cmd_judge(args: argparse.Namespace) -> None:
    root = resolve_root(args.root)
    if lizard is None:
        # No detector installed: degrade to a silent pass rather than block.
        json.dump([], sys.stdout)
        return
    ceiling = args.ceiling
    marks = load_marks(root, RATCHET)
    orig_marks = {k: Mark(v.value, v.body_hash) for k, v in marks.items()}

    files = list(args.changed or [])
    if args.stdin_files:
        for line in sys.stdin.read().splitlines():
            if line.startswith("===== FILE: ") and line.endswith(" ====="):
                files.append(line[len("===== FILE: ") : -len(" =====")])

    findings: list[dict] = []
    for rel in files:
        if not is_code(rel):
            continue
        current = dict(collect_over(root, rel, ceiling))
        batch_findings, marks = judge_batch(current, marks, ceiling, f"{rel}::")
        findings.extend(_finding(rel, f, ceiling) for f in batch_findings)

    if marks != orig_marks:
        save_marks(root, marks, RATCHET)
    json.dump(findings, sys.stdout)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    for name in ("seed", "judge"):
        p = sub.add_parser(name)
        p.add_argument("--root", default=".")
        p.add_argument("--ceiling", type=int, default=6)
        if name == "judge":
            p.add_argument("--stdin-files", action="store_true", help="parse gate mode:file payload")
            p.add_argument("--changed", nargs="*", default=[])

    args = parser.parse_args()
    {"seed": cmd_seed, "judge": cmd_judge}[args.cmd](args)


if __name__ == "__main__":
    main()
