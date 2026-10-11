#!/usr/bin/env python3
"""verify_chain.py 域门(domain_gate)回归测试(unittest + mock post, 不碰网络)。

钉住:
  - 域门高 p(≥ 域门 fail 0.8)候选被丢弃且计入汇总 INFO
    `VERIFY-DOMAIN-GATE: dropped N/M`;
  - 域门 warn~fail 区间: 判决照常, finding extra 带 domain_gate 概率;
  - 未配 domain_gate 的题集(hardcoded_secret/rust_test_no_assert)行为逐字不变:
    无 VERIFY-DOMAIN-GATE 汇总、无 domain_gate extra 键;
  - 降级路径(无端点对)逐字不变: candidates-raw WARN, 不碰域门, 不发 post;
  - 域门声明在题集 json(键名/阈值/陈述式指令), 不在代码里。
"""
from __future__ import annotations

import io
import json
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import jev_env
import verify_chain as vc

HERE = Path(__file__).resolve().parent

SLOP_FILES = (
    "===== FILE: a.py =====\n# Step 1: parse config\nx = 1\n"
    "===== FILE: b.py =====\n# Step 2: write the result\ny = 2\n"
)
SECRET_FILES = (
    "===== FILE: c.py =====\napi_key = \"abcdef1234567890\"\nz = 3\n"
)


def noul(qid, value):
    return {qid: {"type": "noul", "noul": value}}


def per_path(post_map):
    """post 假实现: 按 state.path 回放录好的答案字典。"""
    def impl(url, headers, body, timeout):
        path = body["state"].get("path", "")
        return {"answers": {k: dict(v) for k, v in post_map[path].items()}}
    return impl


def run_main(rule, stdin_text, env, post_impl, argv=None):
    argv = argv if argv is not None else ["verify_chain.py", "--rule", rule]
    with mock.patch.object(vc, "post", side_effect=post_impl), \
         mock.patch.object(jev_env, "resolve",
                           return_value=("https://x.example", "k", "m") if env else (None, None, "m")), \
         mock.patch("sys.argv", argv), \
         mock.patch("sys.stdin", io.StringIO(stdin_text)), \
         mock.patch("sys.stdout", new=io.StringIO()) as out:
        vc.main()
    return json.loads(out.getvalue())


class GateConfigTest(unittest.TestCase):
    """域门是声明式配置: 键在题集 json, 不在代码。"""

    @classmethod
    def setUpClass(cls):
        with open(HERE / "jev_questions_verify.json", encoding="utf-8") as fh:
            cls.cfg = json.load(fh)

    def test_gated_sections_declare_domain_gate(self):
        for sec in ("slop_comment", "antislop", "duplication"):
            q = self.cfg[sec]["questions"]["domain_gate"]
            self.assertEqual(q["type"], "noul")
            self.assertEqual(q["fail"], 0.8)
            self.assertEqual(q["warn"], 0.6)
            self.assertIn("outside the rule's domain", q["instructions"])

    def test_ungated_sections_unchanged(self):
        for sec in ("hardcoded_secret", "rust_test_no_assert"):
            self.assertNotIn("domain_gate", self.cfg[sec]["questions"])


class GateVerdictTest(unittest.TestCase):
    """gate_verdict 分档(阈值从题集声明读)。"""

    def _cfg(self):
        return {"questions": {"domain_gate": {"fail": 0.8, "warn": 0.6}}}

    def test_bands(self):
        cfg = self._cfg()
        self.assertEqual(vc.gate_verdict(cfg, {"domain_gate": 0.9}), ("drop", 0.9))
        self.assertEqual(vc.gate_verdict(cfg, {"domain_gate": 0.7}), ("note", 0.7))
        self.assertEqual(vc.gate_verdict(cfg, {"domain_gate": 0.5}), (None, 0.5))
        self.assertEqual(vc.gate_verdict(cfg, {"domain_gate": 0.8}), ("drop", 0.8))

    def test_unconfigured_returns_unchanged(self):
        self.assertEqual(vc.gate_verdict({"questions": {}}, {"x": 0.9}), (None, None))
        self.assertEqual(vc.gate_verdict(self._cfg(), {"x": 0.9}), (None, None))


class DomainGateBehaviorTest(unittest.TestCase):
    def test_high_gate_drops_candidate_and_counts_info(self):
        post_map = {
            "a.py": noul("domain_gate", 0.9) | noul("is_slop", 0.9),
            "b.py": noul("domain_gate", 0.1) | noul("is_slop", 0.9),
        }
        out = run_main("slop_comment", SLOP_FILES, True, per_path(post_map))
        rule_findings = [f for f in out if f["id"] == "SLOP-COMMENT"]
        gate_info = [f for f in out if f["id"] == "VERIFY-DOMAIN-GATE"]
        # a.py 越域被丢(即使 is_slop 0.9), b.py 照常 FAIL
        self.assertEqual(len(rule_findings), 1)
        self.assertEqual(rule_findings[0]["severity"], "FAIL")
        self.assertEqual(rule_findings[0]["path"], "b.py")
        self.assertNotIn("domain_gate", rule_findings[0])
        self.assertEqual(len(gate_info), 1)
        self.assertEqual(gate_info[0]["severity"], "INFO")
        self.assertIn("dropped 1/2", gate_info[0]["message"])

    def test_mid_gate_keeps_finding_with_extra(self):
        post_map = {
            "a.py": noul("domain_gate", 0.7) | noul("is_slop", 0.9),
            "b.py": noul("domain_gate", 0.2) | noul("is_slop", 0.2),
        }
        out = run_main("slop_comment", SLOP_FILES, True, per_path(post_map))
        f = next(x for x in out if x["id"] == "SLOP-COMMENT")
        self.assertEqual(f["severity"], "FAIL")
        self.assertEqual(f.get("domain_gate"), "0.70")
        # 无人被丢 → 无汇总 INFO
        self.assertEqual([x for x in out if x["id"] == "VERIFY-DOMAIN-GATE"], [])

    def test_ungated_rule_behavior_verbatim(self):
        post_map = {"c.py": noul("real_secret", 0.9)}
        out = run_main("hardcoded_secret", SECRET_FILES, True, per_path(post_map))
        self.assertEqual(len(out), 1)
        f = out[0]
        self.assertEqual(f["id"], "HARDCODED-SECRET")
        self.assertEqual(f["severity"], "FAIL")
        self.assertEqual(f.get("real_secret"), "0.90")
        self.assertNotIn("domain_gate", f)
        self.assertNotIn("VERIFY-DOMAIN-GATE", [x["id"] for x in out])

    def test_degrade_path_verbatim_no_gate(self):
        captured = []

        def impl(url, headers, body, timeout):
            captured.append(url)

        out = run_main("slop_comment", SLOP_FILES, False, impl)
        self.assertEqual(captured, [])
        self.assertEqual(len(out), 2)
        for f in out:
            self.assertEqual(f["severity"], "WARN")
            self.assertEqual(f.get("tier"), "candidates-raw")
        self.assertNotIn("VERIFY-DOMAIN-GATE", [x["id"] for x in out])


if __name__ == "__main__":
    unittest.main(verbosity=2)
