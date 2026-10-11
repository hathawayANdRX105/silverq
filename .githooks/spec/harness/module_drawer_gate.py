#!/usr/bin/env python3
"""module drawer gate — misc-drawer aggregation heuristic (deterministic).

A "misc drawer" is a file named util/helper/common/misc/... that aggregates
unrelated top-level pub items. File length is the file_size gate's business;
module cohesion is semantic_rust's module_structure category. This gate catches
only the deterministic signal both miss: a generic filename whose top-level
public surface spans many item kinds.

Gate protocol: stdout = JSON array of
{"id","severity","path","line","message"} (see ccn_gate for the convention).

stdin (mode:file): "===== FILE: rel =====\\n<content>" blocks.
Trigger: file stem in DRAWER_NAMES AND >= MIN_KINDS distinct top-level pub
item kinds AND >= MIN_PUB_ITEMS top-level pub items -> one WARN per file.
Only indent-0 items count (items inside `mod x { }` belong to that module).
"""
import json
import re
import sys

DRAWER_NAMES = {
    "util", "utils", "helper", "helpers", "common", "common_utils",
    "misc", "misc_utils", "general", "shared", "tools",
}
MIN_KINDS = 3
MIN_PUB_ITEMS = 5

# Top-level (indent-0) pub item declarations, by kind.
KIND_PATTERNS = (
    ("fn", re.compile(r"^pub(?:\(\w+\))? fn ")),
    ("struct", re.compile(r"^pub(?:\(\w+\))? struct ")),
    ("enum", re.compile(r"^pub(?:\(\w+\))? enum ")),
    ("trait", re.compile(r"^pub(?:\(\w+\))? trait ")),
    ("mod", re.compile(r"^pub(?:\(\w+\))? mod ")),
    ("const", re.compile(r"^pub(?:\(\w+\))? (?:const|static) ")),
    ("type", re.compile(r"^pub(?:\(\w+\))? type ")),
)
FILE_MARKER = re.compile(r"^===== FILE: (.+) =====$")


def parse_files(payload: str) -> dict:
    """mode:file stdin -> {rel_path: content} (order-preserving)."""
    files: dict = {}
    rel = None
    chunks: list = []
    for line in payload.splitlines():
        m = FILE_MARKER.match(line)
        if m:
            if rel is not None:
                files[rel] = "\n".join(chunks)
            rel = m.group(1)
            chunks = []
        elif rel is not None:
            chunks.append(line)
    if rel is not None:
        files[rel] = "\n".join(chunks)
    return files


def judge(rel: str, content: str) -> dict | None:
    """Return a drawer finding for one file, or None."""
    stem = rel.rsplit("/", 1)[-1]
    stem = stem.rsplit(".", 1)[0] if "." in stem else stem
    if stem not in DRAWER_NAMES:
        return None
    kinds: set = set()
    count = 0
    first_line = 0
    for i, line in enumerate(content.splitlines(), start=1):
        for kind, pat in KIND_PATTERNS:
            if pat.match(line):
                if not count:
                    first_line = i
                kinds.add(kind)
                count += 1
                break
    if len(kinds) < MIN_KINDS or count < MIN_PUB_ITEMS:
        return None
    return {
        "id": "MODULE-DRAWER",
        "severity": "WARN",
        "path": rel,
        "line": first_line,
        "message": (
            f"misc-drawer module: {count} top-level pub items across "
            f"{len(kinds)} kinds ({', '.join(sorted(kinds))}) aggregated in "
            f"a generic-named file; split into cohesive modules"
        ),
    }


def main() -> int:
    findings = []
    for rel, content in parse_files(sys.stdin.read()).items():
        f = judge(rel, content)
        if f:
            findings.append(f)
    print(json.dumps(findings, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
