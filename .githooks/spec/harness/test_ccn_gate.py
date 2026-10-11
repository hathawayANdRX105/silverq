#!/usr/bin/env python3
"""ccn gate 抗改名（棘轮账本继承）回归测试。

钉住审计缺陷 P2：函数改名 / 移动文件时，零复杂度变化的重构不应被当成
「新违规」FAIL。机制 = 账本 3 列 key\tccn\tbody_hash：函数体归一化 hash +
ccn 相同则把账本继承到新 key（视同持平）；hash 或 ccn 变了才照报；孤儿账无人
认领才删。核心判定在 ratchet_lib.judge_batch（纯函数，无需 lizard）。

  - 改名持平（同名换名、体不变）      -> 不报，账本跟随到 key
  - 改名又变复杂（体变了 hash 变）    -> 仍报（new）
  - 2 列旧账无 hash                   -> 不继承，按 new 照报
  - 孤儿账（函数删了 / 无人认领）     -> 清理，不报
  - 下降 -> 改写账本；升 -> 恶化 FAIL；天花板内 -> 销账

末尾附一个真跑 ccn_gate.py 子进程的用例（改名持平端到端），仅当环境装了
lizard 才执行，否则跳过——判定语义上面已用纯函数覆盖。
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ratchet_lib as rl

CEIL = 6
CEIL_PREFIX = "f::"


def judge(current, marks, ceiling=CEIL, prefix=CEIL_PREFIX):
    """Return ([(kind, key)], new_marks) for a batch."""
    findings, new_marks = rl.judge_batch(current, marks, ceiling, prefix)
    return [(f.kind, f.key) for f in findings], new_marks


class CcnRenameTest(unittest.TestCase):
    def test_rename_flat_is_silent(self):
        # 函数体不变、仅改名：旧账 (8, H) 的孤儿被新 key (8, H) 认领。
        cur = {"f::new": rl.Entry(8, "H", 5)}
        marks = {"f::old": rl.Mark(8, "H")}
        findings, new_marks = judge(cur, marks)
        self.assertEqual(findings, [])
        self.assertEqual(new_marks.get("f::new"), rl.Mark(8, "H"))
        self.assertNotIn("f::old", new_marks)

    def test_rename_more_complex_still_reports(self):
        # 改名 + 变复杂：体变了 hash 变 (H->H2) 且 ccn 升，孤儿认不进去。
        cur = {"f::new": rl.Entry(12, "H2", 5)}
        marks = {"f::old": rl.Mark(8, "H")}
        findings, _ = judge(cur, marks)
        self.assertEqual(findings, [("new", "f::new")])

    def test_legacy_no_hash_does_not_inherit(self):
        # 2 列旧行没有 hash：改名认不进账本，按新违规照报（无 hash 则不继承）。
        cur = {"f::new": rl.Entry(8, "H", 5)}
        marks = {"f::old": rl.Mark(8, None)}
        findings, new_marks = judge(cur, marks)
        self.assertEqual(findings, [("new", "f::new")])
        self.assertNotIn("f::old", new_marks)

    def test_new_violation_when_never_booked(self):
        findings, _ = judge({"f::a": rl.Entry(9, "H", 3)}, {})
        self.assertEqual(findings, [("new", "f::a")])

    def test_worsened_when_mark_present_and_rose(self):
        findings, _ = judge({"f::a": rl.Entry(12, "H", 3)}, {"f::a": rl.Mark(8, "H")})
        self.assertEqual(findings, [("worsened", "f::a")])

    def test_fall_lowers_the_mark(self):
        # 下降：账本改写（只降不升），静默。
        cur = {"f::a": rl.Entry(10, "H", 3)}
        marks = {"f::a": rl.Mark(15, "H")}
        findings, new_marks = judge(cur, marks)
        self.assertEqual(findings, [])
        self.assertEqual(new_marks.get("f::a"), rl.Mark(10, "H"))

    def test_orphan_is_cleaned_when_function_gone(self):
        # 函数删了 / 没人认领 → 孤儿账清理，不报。
        findings, new_marks = judge({}, {"f::dead": rl.Mark(8, "H")})
        self.assertEqual(findings, [])
        self.assertEqual(new_marks, {})


def _lizard_available() -> bool:
    import importlib.util

    return importlib.util.find_spec("lizard") is not None


class CcnGateSubprocessTest(unittest.TestCase):
    """End-to-end through the real ccn_gate.py subprocess (needs lizard)."""

    CCN = Path(__file__).resolve().parent / "ccn_gate.py"
    RUST = """\
fn old_name(x: i32) -> i32 {
    if x > 0 { return 1; }
    if x < 0 { return -1; }
    if x == 1 { return 2; }
    if x == 2 { return 3; }
    if x == 3 { return 4; }
    0
}
"""

    @unittest.skipUnless(_lizard_available(), "lizard not installed")
    def test_pure_rename_is_silent_end_to_end(self):
        tmp = Path(tempfile.mkdtemp(prefix="ccn-subprocess-"))
        subprocess.run(["git", "init", "-q"], cwd=tmp, check=True, capture_output=True)
        (tmp / "src.rs").write_text(self.RUST)
        # commit only after src.rs exists: an empty-repo commit exits 1
        for cmd in (["git", "-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"],
                    ["git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "f"]):
            subprocess.run(cmd, cwd=tmp, check=True, capture_output=True)

        # seed books old_name; then rename it and judge — a pure rename must stay silent.
        subprocess.run([sys.executable, str(self.CCN), "seed", "--root", str(tmp),
                        "--ceiling", "6"], check=True, capture_output=True, text=True)
        self.assertTrue((tmp / "ratchet.tsv").is_file())
        (tmp / "src.rs").write_text(self.RUST.replace("old_name", "renamed_name"))
        out = subprocess.run(
            [sys.executable, str(self.CCN), "judge", "--root", str(tmp),
             "--ceiling", "6", "--changed", "src.rs"],
            check=True, capture_output=True, text=True)
        findings = json.loads(out.stdout)
        self.assertEqual(findings, [], f"pure rename must be silent, got {findings}")


if __name__ == "__main__":
    unittest.main()
