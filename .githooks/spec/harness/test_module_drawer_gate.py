"""Tests for module_drawer_gate (misc-drawer aggregation heuristic)."""
import json
import unittest
from unittest import mock

import module_drawer_gate as mdg


def payload(*files):
    """Build mode:file stdin from (rel, content) pairs."""
    return "".join(f"===== FILE: {rel} =====\n{content}" for rel, content in files)


DRAWER_MIXED = """pub fn parse(x: &str) -> u32 { 0 }
pub fn format(x: u32) -> String { String::new() }
pub struct Config;
pub enum Mode;
pub trait Render;
"""


class ModuleDrawerGateTest(unittest.TestCase):
    def test_drawer_with_mixed_kinds_warns(self):
        f = mdg.judge("src/utils.rs", DRAWER_MIXED)
        self.assertEqual(f["id"], "MODULE-DRAWER")
        self.assertEqual(f["severity"], "WARN")
        self.assertEqual(f["line"], 1)
        self.assertIn("5 top-level pub items across 4 kinds", f["message"])

    def test_drawer_single_kind_no_finding(self):
        # 通用名但职责单一(全是 fn)——聚合信号不成立
        self.assertIsNone(mdg.judge("src/helpers.rs", "pub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn d() {}\npub fn e() {}\n"))

    def test_cohesive_name_no_finding(self):
        # 内聚名文件即使混合也不报(内聚模块不归本 gate)
        self.assertIsNone(mdg.judge("src/config.rs", DRAWER_MIXED))

    def test_two_kinds_below_threshold(self):
        self.assertIsNone(mdg.judge("src/common.rs", "pub fn a() {}\npub fn b() {}\npub struct S;\npub struct T;\npub struct U;\n"))

    def test_four_items_below_count(self):
        self.assertIsNone(mdg.judge("src/misc.rs", "pub fn a() {}\npub struct S;\npub enum E;\npub trait T;\n"))

    def test_nested_mod_items_not_counted(self):
        # items inside `mod x { }` belong to that module, not the file surface
        self.assertIsNone(mdg.judge("src/utils.rs", "pub fn a() {}\nmod inner {\n    pub struct S;\n    pub enum E;\n    pub trait T;\n    pub fn b() {}\n    pub fn c() {}\n}\n"))

    def test_pub_crate_counts(self):
        f = mdg.judge("src/utils.rs", "pub(crate) fn a() {}\npub(crate) fn b() {}\npub(crate) struct S;\npub(crate) enum E;\npub(crate) trait T;\n")
        self.assertIsNotNone(f)

    def test_cfg_attr_still_counts(self):
        # 属性行不影响下一行的 pub 项识别
        f = mdg.judge("src/utils.rs", "#[cfg(test)]\npub fn a() {}\npub fn b() {}\npub struct S;\npub enum E;\npub trait T;\n")
        self.assertIsNotNone(f)

    def test_main_parses_stdin_blocks(self):
        stdin_payload = payload(("src/utils.rs", DRAWER_MIXED), ("src/main.rs", "pub fn main() {}\n"))
        with mock.patch.object(mdg.sys, "stdin", mock.Mock(read=lambda: stdin_payload)), \
             mock.patch.object(mdg.sys, "stdout", mock.Mock(write=lambda s: captured.append(s))):
            captured = []
            mdg.main()
        out = json.loads(captured[0])
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["path"], "src/utils.rs")


if __name__ == "__main__":
    unittest.main()
