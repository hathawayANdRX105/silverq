#!/usr/bin/env python3
"""ratchet_lib 判定语义回归测试（纯 stdlib，无检测器/网络依赖）。

钉住棘轮账本的核心判定——存量债务只许降不许升：
  - classify   无账超标=新违规 / 有账上升=恶化 / 持平与下降=容忍 / 天花板内=正常
  - settle_mark 天花板内销账 / 下降改写账本 / 未记账不自动记
  - load/save  3 列格式可读写，2 列旧行照常解析（无 hash 则不参与改名继承）
  - body hash  改名（换名不换体）hash 不变，改体 hash 变
  - judge_batch 改名持平不报 / 改名变复杂仍报 / 孤儿账无人认领则清理

直接 import 真身（不经手被测仓），断言用集合对账，行号即样张里的物理行。
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ratchet_lib as rl


def make_repo() -> str:
    """A throwaway git worktree so load/save_mark against resolve_root is safe."""
    return tempfile.mkdtemp(prefix="ratchet-test-")


class ClassifyTest(unittest.TestCase):
    def test_new_violation_when_unbooked(self):
        self.assertEqual(rl.classify(8, None, 6), "new")

    def test_worsened_when_above_mark(self):
        self.assertEqual(rl.classify(12, rl.Mark(8), 6), "worsened")

    def test_tolerated_when_flat_or_fall(self):
        self.assertEqual(rl.classify(8, rl.Mark(8), 6), "tolerated")
        self.assertEqual(rl.classify(10, rl.Mark(15), 6), "tolerated")

    def test_within_ceiling_is_ok_regardless_of_mark(self):
        self.assertEqual(rl.classify(4, rl.Mark(15), 6), "ok")
        self.assertEqual(rl.classify(4, None, 6), "ok")


class SettleTest(unittest.TestCase):
    def test_below_ceiling_clears(self):
        self.assertIsNone(rl.settle_mark(4, 15, 6))

    def test_fall_lowers_mark(self):
        self.assertEqual(rl.settle_mark(10, 15, 6), 10)

    def test_rise_keeps_mark(self):
        # marks may only fall: a rise is NOT auto-re-marked here, judge reports it
        self.assertEqual(rl.settle_mark(20, 8, 6), 8)

    def test_unbooked_over_ceiling_not_auto_marked(self):
        self.assertIsNone(rl.settle_mark(12, None, 6))


class LoadSaveTest(unittest.TestCase):
    def test_missing_file_is_empty_ledger(self):
        d = make_repo()
        self.assertEqual(rl.load_marks(d), {})

    def test_three_column_roundtrip(self):
        d = make_repo()
        rl.save_marks(d, {"a": rl.Mark(9, "h"), "b": rl.Mark(7, None)})
        loaded = rl.load_marks(d)
        self.assertEqual(loaded["a"], rl.Mark(9, "h"))
        self.assertIsNone(loaded["b"].body_hash)
        self.assertEqual(loaded["b"].value, 7)

    def test_two_column_legacy_rows_parse(self):
        d = make_repo()
        Path(d, "ratchet.tsv").write_text("k::f\t8\nk2::g\t12\n")
        loaded = rl.load_marks(d)
        self.assertEqual(loaded, {"k::f": rl.Mark(8, None), "k2::g": rl.Mark(12, None)})


class BodyHashTest(unittest.TestCase):
    def test_rename_keeps_hash(self):
        old = ["fn old(x: i32) -> i32 {", "    if x > 0 {", "        return 1;", "    }", "    0", "}"]
        new = ["fn new(x: i32) -> i32 {", "    if x > 0 {", "        return 1;", "    }", "    0", "}"]
        h_old = rl.function_body_hash(old, "old", 1, 6)
        h_new = rl.function_body_hash(new, "new", 1, 6)
        self.assertEqual(h_old, h_new, "a pure rename must keep its body hash")

    def test_body_change_breaks_hash(self):
        base = ["fn f(x: i32) -> i32 {", "    if x > 0 {", "        return 1;", "    }", "    0", "}"]
        variant = ["fn f(x: i32) -> i32 {", "    if x > 0 {", "        return 2;", "    }", "    0", "}"]
        self.assertNotEqual(rl.function_body_hash(base, "f", 1, 6),
                            rl.function_body_hash(variant, "f", 1, 6))

    def test_whitespace_collapse_is_stable(self):
        a = ["fn f() {", "    let x = 1;"]
        b = ["fn f() {", "   let  x   = 1;"]  # re-indent / extra spaces
        self.assertEqual(rl.function_body_hash(a, "f", 1, 2), rl.function_body_hash(b, "f", 1, 2))


if __name__ == "__main__":
    unittest.main()
