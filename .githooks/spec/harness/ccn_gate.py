#!/usr/bin/env python3
"""ccn gate — per-function cyclomatic ceiling with ratchet.

Mechanism reference: crapkit (CRAP / ccn ceiling / ratchet, marks may only fall).
Gate protocol: stdout = JSON array of {"id","severity","path","line","message"}.

Usage:
  ccn_gate.py seed  [--root R] [--ceiling 6]            # book whole-repo debt -> ratchet.tsv
  ccn_gate.py judge [--root R] [--ceiling 6] --stdin-files   # mode:file stdin (===== FILE: rel =====)
  ccn_gate.py judge [--root R] [--ceiling 6] --changed a.rs b.rs

Semantics:
  - function ccn > ceiling and NOT in ratchet  -> FAIL (new violation)
  - function ccn > ceiling, in ratchet, rose   -> FAIL (worsened)
  - function ccn > ceiling, in ratchet, same   -> tolerated debt (silent)
  - marked function fell below mark            -> mark auto-lowers / clears (marks may only fall)

Dependency: `lizard`. If not importable, this harness degrades to a silent skip
(prints [] / exits 0 for judge; exits non-zero for seed) so a repo that has not
installed lizard is not blocked — the FAIL severity only bites once lizard is present.
"""
import argparse
import json
import subprocess
import sys
from pathlib import Path

try:
    import lizard
except Exception:  # pragma: no cover - environment-dependent
    lizard = None

RATCHET = "ratchet.tsv"

CODE_EXTS = {
    ".rs", ".ts", ".tsx", ".js", ".py", ".go", ".java", ".c", ".cpp", ".h",
    ".hpp", ".zig", ".swift", ".sh", ".ps1", ".vue",
}


def is_code(rel: str) -> bool:
    return Path(rel).suffix.lower() in CODE_EXTS


def tracked_files(root: Path) -> list[str]:
    out = subprocess.run(["git", "ls-files"], cwd=root, capture_output=True, text=True)
    if out.returncode != 0:
        return []
    return [line for line in out.stdout.splitlines() if line.strip()]


def load_marks(root: Path) -> dict[str, int]:
    marks: dict[str, int] = {}
    path = root / RATCHET
    if path.is_file():
        for line in path.read_text().splitlines():
            if "\t" in line:
                key, value = line.rsplit("\t", 1)
                marks[key] = int(value)
    return marks


def save_marks(root: Path, marks: dict[str, int]) -> None:
    with (root / RATCHET).open("w") as fh:
        for key in sorted(marks):
            fh.write(f"{key}\t{marks[key]}\n")


def find_over(root: Path, ceiling: int, files: list[str]):
    """Yield (rel_path, function) for functions above the ceiling."""
    for rel in files:
        if not is_code(rel):
            continue
        path = root / rel
        if not path.is_file():
            continue
        try:
            result = lizard.analyze_file(str(path))
        except Exception:
            continue  # unparseable / unsupported: not a gate finding
        for fn in result.function_list:
            if fn.cyclomatic_complexity > ceiling:
                yield rel, fn


def cmd_seed(args: argparse.Namespace) -> None:
    if lizard is None:
        print("ccn_gate: lizard not available (pip install lizard) — cannot seed", file=sys.stderr)
        sys.exit(2)
    root = Path(args.root).resolve()
    marks = load_marks(root)
    count = 0
    for rel, fn in find_over(root, args.ceiling, tracked_files(root)):
        marks[f"{rel}::{fn.name}"] = max(int(fn.cyclomatic_complexity), marks.get(f"{rel}::{fn.name}", 0))
        count += 1
    save_marks(root, marks)
    print(f"seeded {count} over-ceiling mark(s) -> {root / RATCHET}", file=sys.stderr)


def cmd_judge(args: argparse.Namespace) -> None:
    root = Path(args.root).resolve()
    if lizard is None:
        # No detector installed: degrade to a silent pass rather than block.
        json.dump([], sys.stdout)
        return
    ceiling = args.ceiling
    marks = load_marks(root)

    files = list(args.changed or [])
    if args.stdin_files:
        for line in sys.stdin.read().splitlines():
            if line.startswith("===== FILE: ") and line.endswith(" ====="):
                files.append(line[len("===== FILE: ") : -len(" =====")])

    findings: list[dict] = []
    marks_changed = False
    for rel in files:
        path = root / rel
        if not path.is_file():
            continue
        try:
            result = lizard.analyze_file(str(path))
        except Exception:
            continue
        current_keys: set[str] = set()
        for fn in result.function_list:
            ccn = int(fn.cyclomatic_complexity)
            key = f"{rel}::{fn.name}"
            current_keys.add(key)
            if ccn > ceiling:
                if key in marks:
                    if ccn > marks[key]:
                        findings.append(
                            {
                                "id": "CCN-OVER",
                                "severity": "FAIL",
                                "path": rel,
                                "line": fn.start_line,
                                "message": f"{fn.name} ccn={ccn} > ratchet mark {marks[key]} — worsened, decompose",
                            }
                        )
                    # ccn <= mark: tolerated debt, silent
                else:
                    findings.append(
                        {
                            "id": "CCN-OVER",
                            "severity": "FAIL",
                            "path": rel,
                            "line": fn.start_line,
                            "message": f"{fn.name} ccn={ccn} > ceiling {ceiling} — new violation, decompose",
                        }
                    )
            # marks may only fall
            if key in marks and ccn < marks[key]:
                if ccn > ceiling:
                    marks[key] = ccn
                else:
                    del marks[key]
                marks_changed = True
        # function deleted/renamed ⇒ its debt mark dies with it
        stale = [k for k in marks if k.startswith(f"{rel}::") and k not in current_keys]
        for k in stale:
            del marks[k]
            marks_changed = True

    if marks_changed:
        save_marks(root, marks)
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
