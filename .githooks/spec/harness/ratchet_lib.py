#!/usr/bin/env python3
"""ratchet_lib — shared ratchet-debt ledger for the spec gates (ccn, file_size, …).

A ratchet ledger books *pre-existing* debt so a gate only bites on new or
rising debt, and stays silent on debt it already knows about (no re-shaming the
same over-threshold file / over-ceiling function at every merge).

The ledger is a TSV of `key\tvalue\tbody_hash` (3 columns). Legacy 2-column rows
(`key\tvalue`) read fine; a missing/empty hash simply means "no rename
inheritance" for that mark. Semantics: marks may only fall.

Judgement (classify / settle_mark / judge_batch) is detector-agnostic and pure,
so it is unit-testable and reusable by gates with different detectors. The
rename-aware batch judge (judge_batch + Entry) is what the ccn gate uses to
avoid punishing a zero-complexity rename: the mark follows the function to its
new key instead of being dropped and re-reported as "new".
"""
from __future__ import annotations

import hashlib
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

RATCHET = "ratchet.tsv"


# ---------------------------------------------------------------------------
# git io helpers (worktree-safe: ledger + paths resolve via the git toplevel)
# ---------------------------------------------------------------------------


def git_toplevel() -> Path | None:
    """The enclosing git worktree's toplevel from cwd, or None outside a repo."""
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False
    )
    tl = out.stdout.strip()
    return Path(tl) if out.returncode == 0 and tl else None


def resolve_root(arg: str = ".") -> Path:
    """Where the ledger and file reads live.

    An explicit non-default argument is honoured as-is (resolved). The default
    '.' resolves to the enclosing git worktree toplevel so the ledger is found
    from anywhere inside the repo; falls back to cwd if there is no repo.
    """
    if arg not in (".", ""):
        return Path(arg).resolve()
    top = git_toplevel()
    return top if top is not None else Path.cwd()


def git_files(root: Path, pattern: str = "*.rs") -> list[str]:
    """Tracked paths under root matching the glob ('' = every tracked file);
    gitignore/worktree aware."""
    cmd = ["git", "-C", str(root), "ls-files"]
    if pattern:
        cmd += ["--", pattern]
    out = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if out.returncode != 0:
        return []
    return [line for line in out.stdout.splitlines() if line.strip()]


# ---------------------------------------------------------------------------
# Ledger persistence
# ---------------------------------------------------------------------------


@dataclass
class Mark:
    """One ledger row: the value debt was booked at, plus an optional body hash."""

    value: int
    body_hash: str | None = None


def load_marks(root: Path, name: str = RATCHET) -> dict[str, Mark]:
    """Read the ledger at root/name. Missing file = empty ledger.

    Tolerant of 2-column legacy rows (value only) and 3-column rows (value +
    optional body_hash); a blank hash column yields Mark(value, None).
    """
    marks: dict[str, Mark] = {}
    path = Path(root) / name
    if not path.is_file():
        return marks
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        parts = line.split("\t")
        key = parts[0]
        value = int(parts[1].strip()) if len(parts) > 1 and parts[1].strip() else 0
        body_hash = parts[2].strip() if len(parts) > 2 and parts[2].strip() else None
        marks[key] = Mark(value, body_hash)
    return marks


def save_marks(root: Path, marks: dict[str, Mark], name: str = RATCHET) -> None:
    """Write the ledger as sorted 3-column rows (blank hash column when unknown)."""
    with (Path(root) / name).open("w", encoding="utf-8") as fh:
        for key in sorted(marks):
            m = marks[key]
            fh.write(f"{key}\t{m.value}\t{m.body_hash or ''}\n")


# ---------------------------------------------------------------------------
# Pure judgement core (no detector / no IO)
# ---------------------------------------------------------------------------


def classify(value: int, mark: Mark | None, ceiling: int) -> str:
    """Judge one measurement against its debt mark and a ceiling.

    Returns one of:
      'ok'        value within ceiling (no debt either way)
      'new'       over ceiling but never booked
      'worsened'  over ceiling and above the booked mark (debt grew)
      'tolerated' over ceiling but at/below the booked mark (known debt, silent)
    """
    if value <= ceiling:
        return "ok"
    if mark is None:
        return "new"
    if value > mark.value:
        return "worsened"
    return "tolerated"


def settle_mark(value: int, mark_value: int | None, ceiling: int) -> int | None:
    """The ledger value to store after measuring `value` (marks may only fall).

    Within the ceiling the mark clears (None); over the ceiling a marked value
    drops to min(mark, value). An unmarked over-ceiling measurement is *not*
    auto-booked here — only seed does that.
    """
    if value <= ceiling:
        return None
    if mark_value is None:
        return None
    return min(mark_value, value)


_WS = re.compile(r"\s+")


def normalize_body(src: str) -> str:
    """Deterministic whitespace-collapsed form of a source snippet.

    Runs of any whitespace collapse to a single space; leading/trailing
    whitespace is trimmed. Cosmetic-independent: a re-indent or line-wrap does
    not change the result.
    """
    return _WS.sub(" ", src.strip())


def body_hash(src: str) -> str:
    """Stable fingerprint of a source snippet = sha256 of its normalized form."""
    return hashlib.sha256(normalize_body(src).encode("utf-8")).hexdigest()


def function_body_hash(lines: list[str], name: str, start: int, end: int) -> str:
    """Name-independent hash of a function's source block (start/end are 1-based,
    inclusive line numbers over `lines`).

    The function-name token is blanked out before hashing so a pure rename keeps
    the same fingerprint; a real body change does not. Whitespace is collapsed
    (see normalize_body) so re-wrapping/re-indenting does not break inheritance.
    """
    s = max(0, start - 1)
    e = min(len(lines), end)
    block = "\n".join(lines[s:e])
    if name:
        block = re.sub(rf"\b{re.escape(name)}\b", " ", block)
    return body_hash(block)


@dataclass
class Entry:
    """One measured function inside a scanned unit (value + name-free hash)."""

    value: int
    body_hash: str | None
    line: int


@dataclass
class JudgeFinding:
    """A debt the gate should surface: 'new' (unbooked) or 'worsened' (grew)."""

    key: str
    kind: str
    value: int
    mark: int | None
    line: int


def judge_batch(
    current: dict[str, Entry],
    marks: dict[str, Mark],
    ceiling: int,
    rel_prefix: str,
) -> tuple[list[JudgeFinding], dict[str, Mark]]:
    """Rename-aware ratchet judgement for one scanned unit.

    `current`   maps key -> measured Entry for this unit (functions still over
                the ceiling; healed/deleted ones are simply absent).
    `marks`     the ledger, key -> Mark.
    `ceiling`   the value threshold; over it = debt.
    `rel_prefix` key prefix of this unit (e.g. f"{rel}::"), scoping which ledger
                rows belong here.

    Returns (findings, new_marks):
      findings   the 'new'/'worsened' items to surface (all else is silent).
      new_marks  the updated ledger: a function renamed to a new key that carries
                 an identical body + value inherits its debt mark (treated as
                 flat); unclaimed orphan marks are dropped; healed marks are
                 removed and falling marks are lowered.

    A pure rename (same body, same value) is therefore silent — the debt mark
    follows the function instead of being deleted and re-reported as new.
    """
    new_marks = {k: Mark(v.value, v.body_hash) for k, v in marks.items()}
    findings: list[JudgeFinding] = []

    orphan_keys = [k for k in marks if k.startswith(rel_prefix) and k not in current]
    # Orphan rows that carry a body hash are claimable; index by (value, hash) so
    # an inherited debt must match both the booked value and the body fingerprint.
    orphan_index: dict[tuple[int, str], list[str]] = {}
    for k in orphan_keys:
        m = marks[k]
        if m.body_hash is not None:
            orphan_index.setdefault((m.value, m.body_hash), []).append(k)
    claimed: set[str] = set()

    def claim(value: int, bh: str | None) -> str | None:
        if bh is None:  # a 2-column legacy mark has no hash -> no inheritance
            return None
        for k in orphan_index.get((value, bh), []):
            if k not in claimed:
                claimed.add(k)
                return k
        return None

    for key, e in current.items():
        verdict = classify(e.value, marks.get(key), ceiling)
        if verdict == "worsened":
            findings.append(JudgeFinding(key, "worsened", e.value, marks[key].value, e.line))
        elif verdict == "new":
            src = claim(e.value, e.body_hash)
            if src is not None:
                # rename inheritance: identical body + value == same debt, 视同持平.
                new_marks[key] = Mark(e.value, e.body_hash)
                new_marks.pop(src, None)
            else:
                findings.append(JudgeFinding(key, "new", e.value, None, e.line))
        # 'tolerated' / 'ok' are silent; the settling below still applies.

        # marks may only fall: lower an existing mark, or clear it once healed.
        if key in marks:
            settled = settle_mark(e.value, marks[key].value, ceiling)
            if settled is None:
                new_marks.pop(key, None)
            elif settled != marks[key].value:
                new_marks[key] = Mark(settled, marks[key].body_hash or e.body_hash)

    # Orphan rows nobody claimed die with the function they were tracking.
    for k in orphan_keys:
        if k not in claimed:
            new_marks.pop(k, None)

    return findings, new_marks