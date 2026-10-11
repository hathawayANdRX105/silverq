"""Tests for ts_extract (tree-sitter fact cards + deterministic signals)."""
import json
import unittest
from unittest import mock

import ts_extract as ts

FACT_SRC = """/// Parse config, Err on failure
pub fn parse_config(s: &str) -> Result<Config, Err> {
    s.split(',').collect()
}

pub struct Config {
    pub name: String,
}

impl Render for Config {
    pub fn draw(&self) {}
}
"""

SIGNAL_SRC = """pub fn process(x: String) -> String {
    let y = x.clone();
    if a { } else if b { } else if c { } else { }
    for i in 0..x.len() { let _ = i; }
    unsafe { std::hint::black_box(y) }
}

/// SAFETY: caller upholds P
pub fn guarded(x: String) {
    unsafe { std::hint::black_box(x) }
}

pub fn copy_ok(n: u32) -> u32 { n.clone() }

pub fn borrowed(s: &str) -> String { s.to_string() }

pub fn nested_ifs(x: String) -> u32 {
    if x.len() > 1 {
        if x.len() > 2 {
            if x.len() > 3 { return 1; }
        }
    }
    0
}
"""


class FactCardTest(unittest.TestCase):
    def test_function_card(self):
        cards = ts.parse_items(FACT_SRC)
        fn = cards[0]
        self.assertEqual(fn["kind"], "fn")
        self.assertEqual(fn["name"], "parse_config")
        self.assertEqual(fn["vis"], "pub")
        self.assertEqual(fn["params"], "(s: &str)")
        self.assertEqual(fn["ret"], "Result<Config, Err>")
        self.assertEqual(fn["doc"], "Parse config, Err on failure")
        self.assertIn("split", fn["calls"])
        self.assertEqual(fn["unsafe"], 0)

    def test_struct_card(self):
        cards = ts.parse_items(FACT_SRC)
        st = cards[1]
        self.assertEqual(st["kind"], "struct")
        self.assertEqual(st["name"], "Config")
        self.assertEqual(st["fields"], ["pub name: String"])

    def test_impl_card(self):
        cards = ts.parse_items(FACT_SRC)
        impl = cards[2]
        self.assertEqual(impl["kind"], "impl")
        self.assertEqual(impl["trait"], "Render")
        self.assertEqual(impl["methods"], ["draw"])

    def test_fact_cards_text_renders(self):
        text = ts.fact_cards_text(ts.parse_items(FACT_SRC))
        self.assertIn(
            "[fn] parse_config (pub) (s: &str) -> Result<Config, Err>",
            text)
        self.assertIn("doc: Parse config, Err on failure", text)
        self.assertIn("[struct] Config (pub)", text)
        self.assertIn("impl of: Render", text)


class SignalTest(unittest.TestCase):
    def _all(self):
        return ts.signals("src/lib.rs", SIGNAL_SRC)

    def test_clone_owned_param(self):
        f = [x for x in self._all() if x["id"] == "TS-CLONE-OWNED"]
        self.assertEqual(len(f), 1)
        self.assertEqual(f[0]["severity"], "WARN")
        self.assertEqual(f[0]["line"], 2)
        self.assertIn("take &x", f[0]["suggestion"])

    def test_if_chain(self):
        lines = [x["line"] for x in self._all()
                 if x["id"] == "TS-IF-CHAIN"]
        self.assertIn(3, lines)

    def test_deep_if_nesting(self):
        lines = [x["line"] for x in self._all()
                 if x["id"] == "TS-IF-CHAIN"]
        self.assertIn(20, lines)

    def test_chain_not_double_reported(self):
        # 3 臂 else-if 链只在顶端报一次(内层是链成员,不算嵌套)
        lines = [x["line"] for x in self._all()
                 if x["id"] == "TS-IF-CHAIN"]
        self.assertEqual(lines.count(3), 1)

    def test_index_loop(self):
        f = [x for x in self._all() if x["id"] == "TS-INDEX-LOOP"]
        self.assertEqual(len(f), 1)
        self.assertEqual(f[0]["line"], 4)
        self.assertIn("enumerate", f[0]["suggestion"])

    def test_unsafe_without_invariant_fails(self):
        f = [x for x in self._all()
             if x["id"] == "TS-UNSAFE-NOINVARIANT"]
        self.assertEqual(len(f), 1)
        self.assertEqual(f[0]["severity"], "FAIL")
        self.assertEqual(f[0]["line"], 5)
        self.assertIn("SAFETY", f[0]["suggestion"])

    def test_unsafe_with_safety_doc_not_signaled(self):
        lines = [x["line"] for x in self._all()
                 if x["id"] == "TS-UNSAFE-NOINVARIANT"]
        self.assertNotIn(9, lines)

    def test_clone_on_copy_primitive_not_signaled(self):
        lines = [x["line"] for x in self._all()
                 if x["id"] == "TS-CLONE-OWNED"]
        self.assertNotIn(13, lines)

    def test_borrowed_param_not_signaled(self):
        lines = [x["line"] for x in self._all()
                 if x["id"] == "TS-CLONE-OWNED"]
        self.assertNotIn(15, lines)

    def test_short_chain_not_signaled(self):
        src = "pub fn f() { if a { } else if b { } }"
        self.assertEqual([x for x in ts.signals("f.rs", src)
                          if x["id"] == "TS-IF-CHAIN"], [])

    def test_category_and_suggestion_present(self):
        for x in self._all():
            self.assertIn("category", x)
            self.assertIn("suggestion", x)


class SoftDepTest(unittest.TestCase):
    def test_missing_grammar_skips_silently(self):
        with mock.patch.object(ts, "_PARSER", None):
            self.assertEqual(ts.signals("f.rs", "pub fn f() {}"), [])


class CliTest(unittest.TestCase):
    def test_main_parses_mode_file_stdin(self):
        payload = "===== FILE: src/lib.rs =====\n" + SIGNAL_SRC
        cap = []
        with mock.patch.object(ts.sys, "stdin",
                               mock.Mock(read=lambda: payload)), \
             mock.patch.object(ts.sys, "stdout",
                               mock.Mock(write=lambda s: cap.append(s))):
            ts.main()
        out = json.loads(cap[0])
        self.assertTrue(any(x["id"] == "TS-UNSAFE-NOINVARIANT"
                            for x in out))


if __name__ == "__main__":
    unittest.main()
