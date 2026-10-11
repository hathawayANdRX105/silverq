#!/usr/bin/env python3
"""file_size gate — single-file size ceiling with a rename-free ratchet.

Why: files in the 1500-line / 35KB range cannot be reviewed or tested
reliably; the gate warns to split by responsibility. The threshold is a CLI
arg (passed from the checklist yaml), not a hardcoded shell heredoc.

Ratchet: instead of re-warn-shaming every over-threshold file at every merge
(the old grep-only gate did this — and `| head -30` silently truncated the
list), an over-threshold file is booked into a debt ledger `debt.tsv`
(`FILE-TOO-BIG::<path>` -> line count). The judge stays grep-mode (it self-scans
the whole repo), but only *rises* and *new* over-threshold files are WARN:
  - file over-threshold, in debt.tsv, lines flat   -> silent (known debt)
  - file over-threshold, in debt.tsv, lines fell    -> silent, ledger lowered
  - file over-threshold, in debt.tsv, lines rose    -> WARN (debt grew)
  - file over-threshold, NOT in debt.tsv            -> WARN + "run seed" hint
  - debt.tsv missing entirely                        -> all over-threshold files
                                                       WARN + "run seed" hint
  - file no longer over-threshold                    -> its debt mark is cleared

Ledger lives at <repo root>/debt.tsv (3-column `key\tlines\thash`; the hash
column is unused here — the key is path-based — so it stays empty). Judge never
creates a ledger; only `seed` does. Judge only *lowers/clears* marks, so an
empty/all-paid-off ledger is removed rather than left as an empty file.

Gate protocol: stdout = JSON array of {"id","severity","path","line","message"}.
Findings are capped at --max-report (default 50); beyond that a single INFO
finding reports the count of the remaining entries, so nothing is ever silently
truncated (unlike `| head -30`).

Usage:
  file_size_gate.py seed  [--root R] [--max-lines 1500] [--max-bytes 35840]
  file_size_gate.py judge [--root R] [--max-lines 1500] [--max-bytes 35840] [--max-report 50]
"""
import argparse
import json
import sys
from pathlib import Path

from ratchet_lib import Mark, git_files, load_marks, resolve_root, save_marks

DEBT = "debt.tsv"
KEY_PREFIX = "FILE-TOO-BIG::"
SEED_HINT = "；debt.tsv 缺失，先跑 seed 生成账本再判升降"


def _key(rel: str) -> str:
    return f"{KEY_PREFIX}{rel}"


def measure(path: Path) -> tuple[int, int]:
    """(lines, bytes) of a file; lines is the newline count, matching `wc -l`."""
    data = path.read_bytes()
    return data.count(b"\n"), len(data)


def over_threshold(root: Path, max_lines: int, max_bytes: int) -> list[tuple[str, int, int]]:
    """All tracked .rs files whose size is over at least one threshold."""
    out: list[tuple[str, int, int]] = []
    for rel in git_files(root, "*.rs"):
        p = root / rel
        if not p.is_file():
            continue
        lines, nbytes = measure(p)
        if lines > max_lines or nbytes > max_bytes:
            out.append((rel, lines, nbytes))
    return out


def seed(root: Path, max_lines: int, max_bytes: int) -> int:
    """Book every over-threshold file into the debt ledger and persist it."""
    marks = load_marks(root, DEBT)
    count = 0
    for rel, lines, _ in over_threshold(root, max_lines, max_bytes):
        key = _key(rel)
        # keep the higher value so re-seeding a shrunken file never lowers a ratchet
        marks[key] = Mark(max(lines, marks[key].value if key in marks else 0), None)
        count += 1
    save_marks(root, marks, DEBT)
    return count


def judge(root: Path, max_lines: int, max_bytes: int, max_report: int) -> list[dict]:
    """Judge the whole repo against the debt ledger; return findings to emit."""
    marks = load_marks(root, DEBT)
    had_ledger = (root / DEBT).is_file()
    over = over_threshold(root, max_lines, max_bytes)
    over_keys = {_key(rel) for rel, _, _ in over}

    # a debt whose file is no longer over-threshold is paid off: drop the mark.
    for key in list(marks):
        if key.startswith(KEY_PREFIX) and key not in over_keys:
            del marks[key]

    findings: list[dict] = []
    for rel, lines, nbytes in over:
        key = _key(rel)
        mark = marks.get(key)
        if mark is None:
            hint = "" if had_ledger else SEED_HINT
            findings.append({
                "id": "FILE-TOO-BIG",
                "severity": "WARN",
                "path": rel,
                "line": 0,
                "message": (f"{rel} 超标（{lines} 行 / {nbytes} B）— 新超标，"
                            f"考虑按单一职责拆分{hint}"),
            })
        elif lines > mark.value:
            findings.append({
                "id": "FILE-TOO-BIG",
                "severity": "WARN",
                "path": rel,
                "line": 0,
                "message": (f"{rel} 超标（{lines} 行 / {nbytes} B）— "
                            f"高于棘轮记账 {mark.value} 行，债务恶化，按单一职责拆分"),
            })
        elif lines < mark.value:
            # a fall is silent but lowers the mark (marks may only fall).
            marks[key] = Mark(lines, None)

    if marks:
        save_marks(root, marks, DEBT)
    elif had_ledger:
        # every debt paid off — drop the now-empty ledger rather than leave it stale.
        (root / DEBT).unlink(missing_ok=True)

    if len(findings) > max_report:
        dropped = len(findings) - max_report
        findings = findings[:max_report]
        findings.append({
            "id": "FILE-TOO-BIG-TRUNCATED",
            "severity": "INFO",
            "path": "",
            "line": 0,
            "message": f"... and {dropped} more over-threshold file(s) (truncated)",
        })
    return findings


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("seed", "judge"):
        p = sub.add_parser(name)
        p.add_argument("--root", default=".")
        p.add_argument("--max-lines", type=int, default=1500)
        p.add_argument("--max-bytes", type=int, default=35840)
        if name == "judge":
            p.add_argument("--max-report", type=int, default=50)

    args = parser.parse_args()
    root = resolve_root(args.root)
    if args.cmd == "seed":
        n = seed(root, args.max_lines, args.max_bytes)
        print(f"seeded {n} over-threshold mark(s) -> {root / DEBT}", file=sys.stderr)
    else:
        json.dump(judge(root, args.max_lines, args.max_bytes, args.max_report), sys.stdout)


if __name__ == "__main__":
    main()