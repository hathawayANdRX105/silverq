#!/usr/bin/env python3
"""file_size_gate.py 判定语义回归测试（无检测器/网络依赖）。

钉住审计缺陷 P4/P5：门禁自扫全仓，存量超标记进 debt.tsv，只判升降——
  - 持平静默（已在账本、行数未变）           -> 不报
  - 上升 WARN（高于记账）                    -> 报
  - 下降静默并改写账本（只降不升）           -> 不报，debt.tsv 里的行数下调
  - 无账本的新超标                           -> WARN，message 带「先跑 seed」提示
  - 天花板内销账                             -> 该文件的账被清掉；全清完 debt.tsv 移除
  - judge 永不建空账本                       -> 首次 judge 不产生 debt.tsv
  - 截断：最多报 50 条，超出补 1 条 INFO      -> "... and N more (truncated)"

用 tmpdir git 仓 fixture + 真进程跑 CLI（stdout 解析成 JSON），不依赖被测仓。
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

GATE = Path(__file__).resolve().parent / "file_size_gate.py"


def rs(n: int) -> str:
    """Exactly n newlines, so the gate's `wc -l`-equivalent reports n lines."""
    return "x\n" * n


def make_repo(files: dict[str, str]) -> Path:
    d = Path(tempfile.mkdtemp(prefix="fs-gate-"))
    subprocess.run(["git", "init", "-q"], cwd=d, check=True, capture_output=True)
    for rel, body in files.items():
        p = d / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body, encoding="utf-8")
    subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"],
                   cwd=d, check=True, capture_output=True)
    subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "f"],
                   cwd=d, check=True, capture_output=True)
    return d


def run_judge(repo: Path, max_report: int = 50) -> list[dict]:
    p = subprocess.run(
        [sys.executable, str(GATE), "judge", "--root", str(repo),
         "--max-lines", "1500", "--max-bytes", "35840", "--max-report", str(max_report)],
        capture_output=True, text=True, check=True)
    return json.loads(p.stdout)


def run_seed(repo: Path) -> None:
    subprocess.run(
        [sys.executable, str(GATE), "seed", "--root", str(repo),
         "--max-lines", "1500", "--max-bytes", "35840"],
        capture_output=True, text=True, check=True)


def ids(findings: list[dict]) -> set[str]:
    return {f["id"] for f in findings}


class FileSizeGateTest(unittest.TestCase):
    def test_flat_is_silent(self):
        repo = make_repo({"big.rs": rs(1600)})
        run_seed(repo)
        self.assertEqual(run_judge(repo), [], "flat known debt must be silent")

    def test_rise_is_warn(self):
        repo = make_repo({"big.rs": rs(1600)})
        run_seed(repo)
        (repo / "big.rs").write_text(rs(1800))
        findings = run_judge(repo)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]["id"], "FILE-TOO-BIG")
        self.assertEqual(findings[0]["severity"], "WARN")
        self.assertIn("1800", findings[0]["message"])

    def test_fall_is_silent_and_lowest(self):
        repo = make_repo({"big.rs": rs(1600)})
        run_seed(repo)
        (repo / "big.rs").write_text(rs(1520))  # still over 1500, but below the 1600 mark
        self.assertEqual(run_judge(repo), [], "a fall is silent known debt")
        debt = (repo / "debt.tsv").read_text(encoding="utf-8")
        self.assertIn("1520", debt, "the ledger row must have been lowered")

    def test_new_without_ledger_warns_and_hints_seed(self):
        repo = make_repo({"big.rs": rs(1600)})
        self.assertFalse((repo / "debt.tsv").exists(), "judge must not create the ledger")
        findings = run_judge(repo)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]["id"], "FILE-TOO-BIG")
        self.assertEqual(findings[0]["severity"], "WARN")
        self.assertIn("seed", findings[0]["message"], "a missing ledger must hint at seed")
        self.assertFalse((repo / "debt.tsv").exists(), "still no ledger after a no-ledger judge")

    def test_heal_below_threshold_pays_off(self):
        repo = make_repo({"a.rs": rs(1600), "b.rs": rs(1600)})
        run_seed(repo)
        self.assertTrue((repo / "debt.tsv").is_file())
        # heal a.rs below the ceiling; b.rs stays a known debt
        (repo / "a.rs").write_text(rs(100))
        self.assertEqual(run_judge(repo), [])
        debt = (repo / "debt.tsv").read_text(encoding="utf-8")
        self.assertNotIn("a.rs", debt, "healed file's mark must be cleared")
        self.assertIn("b.rs", debt)
        # heal b.rs too -> whole ledger empties out and the file is removed
        (repo / "b.rs").write_text(rs(100))
        self.assertEqual(run_judge(repo), [])
        self.assertFalse((repo / "debt.tsv").exists(), "all-paid-off ledger must be removed")

    def test_truncation_caps_at_max_report_with_info(self):
        repo = make_repo({f"f{i:02d}.rs": rs(1600) for i in range(55)})
        findings = run_judge(repo, max_report=50)
        self.assertEqual(len(findings), 51, "50 kept + 1 INFO truncation note")
        self.assertEqual(ids(findings), {"FILE-TOO-BIG", "FILE-TOO-BIG-TRUNCATED"})
        trunc = next(f for f in findings if f["id"] == "FILE-TOO-BIG-TRUNCATED")
        self.assertEqual(trunc["severity"], "INFO")
        self.assertIn("5 more", trunc["message"], f"got {trunc['message']!r}")

    def test_under_threshold_is_not_flagged(self):
        repo = make_repo({"small.rs": rs(100)})
        self.assertEqual(run_judge(repo), [], "a small file is not debt")


if __name__ == "__main__":
    unittest.main()
