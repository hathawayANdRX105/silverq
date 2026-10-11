#!/usr/bin/env python3
"""delivery_chain.py 的判定语义回归测试（unittest + mock，不碰网络/gh）。

样张挑的是**会决定这项检查存废的形态**，不是装饰性断言：

  - claims 抽取必须吃得下中英混排 bullet / 编号列表 / 一句话驳回段落，且段尾
    是下一个标题就停（`How to test` 里的字眼不能被当成声明）；
  - jev 的 noul 方向是 true=「证据不支持该声明」(HIGH=BAD)，p 过阈值才出
    finding —— 低 p 必须**无** finding（钉住它不是空转的噪声发生器）；
  - 基础设施失败（网络挂 / 无 PR body / 无 Delivery record）一律只降成单条
    INFO，永不阻断（DWJ 契约），也不抛异常；
  - 阈值真从 jev_questions_delivery.json 取（0.85/0.6；evidence_sufficient
    0.9/0.6），不是代码里写死的数。

进程形态：真进程 import delivery_chain 跑 main()，只用 fake urllib 换掉
urlopen —— 既钉住进程入口（stdio 协议），又彻底隔离网络与 gh。
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent
HARNESS = HERE / "delivery_chain.py"
MAX_STATE_CHARS = 24000

SAMPLE_BODY = """\
# What

body text.

## Delivery record

- 修复了 X 崩溃 (crash on exit)
1. 补了回归测试 test_alpha
   驳回: 该 finding 是误报，保留原写法

## How to test

cargo test -p alpha
"""

# 换掉 urlopen：请求体落盘（按调用次序，供断言抽取批/判决批的 state 与 questions）。
# 第一次调用回放抽取批 choice 概率（DMC_EXTRACT），之后的调用回放判决批 noul
# 概率（DMC_JUDGE）；调用序号记在 DMC_N 文件里，跨进程共享。
PRELUDE = """\
import json, os, urllib.request


def fake_urlopen(req, timeout=None):
    with open(os.environ["DMC_CAPTURE"], "a", encoding="utf-8") as fh:
        fh.write(req.data.decode() + "\\n")
    if os.environ.get("DMC_NET_FAIL"):
        raise TimeoutError("simulated upstream reset")
    nfile = os.environ["DMC_N"]
    n = int(open(nfile).read() or "0")
    with open(nfile, "w") as fh:
        fh.write(str(n + 1))
    prob = json.loads(os.environ["DMC_EXTRACT" if n == 0 else "DMC_JUDGE"])
    payload = {"answers": {
        q: ({"type": "choice",
             "probabilities": {"disposition": p, "prose": round(1.0 - p, 2)}}
             if q.startswith("cand_") else
             {"type": "noul", "noul": p})
        for q, p in prob.items()
    }}

    class _Resp:
        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self):
            return json.dumps(payload).encode()

    return _Resp()


urllib.request.urlopen = fake_urlopen
"""

RUNNER = """\
import sys, os
sys.path.insert(0, {tmp!r})
import fakelocal  # noqa: F401
sys.path.insert(0, {workdir!r})
sys.stdin = open(os.environ["DMC_DIFF"], encoding="utf-8")
sys.argv = ["delivery_chain.py"]
import delivery_chain
sys.exit(delivery_chain.main())
"""


class DeliveryChainTest(unittest.TestCase):
    # ---------- 纯函数层：claims 抽取 ----------

    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("delivery_chain", str(HARNESS))
        cls.mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.mod)

    def test_extracts_bullets_numbered_and_paragraph(self):
        self.assertEqual(
            self.mod.extract_claims(SAMPLE_BODY),
            [
                "修复了 X 崩溃 (crash on exit)",
                "补了回归测试 test_alpha",
                "驳回: 该 finding 是误报，保留原写法",
            ],
        )

    def test_section_stops_at_next_heading(self):
        self.assertNotIn("cargo test -p alpha", self.mod.extract_claims(SAMPLE_BODY))

    def test_no_delivery_section(self):
        self.assertEqual(self.mod.extract_claims("# What\nnothing here\n"), [])

    def test_twelve_cap_is_explicit_not_silent(self):
        body = "\n".join(["## Delivery record"] + [f"- 声明 {i}" for i in range(15)])
        self.assertEqual(len(self.mod.extract_claims(body)), 15)  # 抽取不截断
        extract = {f"cand_{i}": 0.9 for i in range(15)}
        judge = {f"claim_{i}": 0.1 for i in range(15)}
        out, _ = self.run_harness(body, extract, judge)
        ids = [f["id"] for f in out]
        self.assertIn("DELIVERY-CLAIMS-TRUNCATED", ids)
        note = next(f for f in out if f["id"] == "DELIVERY-CLAIMS-TRUNCATED")
        self.assertEqual(note["severity"], "INFO")
        self.assertIn("15", note["message"])
        self.assertIn("12", note["message"])

    # ---------- 进程层：stdio 协议 + 判定 ----------

    def run_harness(self, body: str, extract: dict | None, judge: dict | None,
                    diff: str = "diff --git a/x b/x\n+x",
                    net_fail: bool = False, no_key: bool = False) -> tuple:
        """extract = {cand_i: p(disposition)}; judge = {claim_i/evidence_sufficient: p}.

        Returns (findings, captured_requests) where captured_requests is the
        list of posted bodies in call order (empty when nothing was posted).
        """
        with tempfile.TemporaryDirectory(prefix="dmc-") as tmp:
            def put(name, content):
                p = os.path.join(tmp, name)
                Path(p).write_text(content, encoding="utf-8")
                return p

            put("fakelocal.py", PRELUDE)
            put("dmc_n.txt", "0")
            env = dict(os.environ)
            for k in ("TYPESAFE_API_KEY", "TYPESAFE_API_BASE", "JEV_API_KEY",
                      "JEV_API_BASE_URL", "JEV_MODEL"):
                env.pop(k, None)
            if not no_key:
                env["TYPESAFE_API_KEY"] = "test-key"
            env.update({
                "GATE_PR_BODY_FILE": put("body.md", body),
                "DMC_DIFF": put("diff.txt", diff),
                "DMC_CAPTURE": os.path.join(tmp, "capture.jsonl"),
                "DMC_N": os.path.join(tmp, "dmc_n.txt"),
                "DMC_EXTRACT": json.dumps(extract or {}),
                "DMC_JUDGE": json.dumps(judge or {}),
            })
            if net_fail:
                env["DMC_NET_FAIL"] = "1"
            code = RUNNER.format(tmp=tmp, workdir=str(HERE))
            p = subprocess.run([sys.executable, "-c", code],
                               capture_output=True, text=True, env=env, cwd=str(HERE),
                               check=False)
            self.assertEqual(p.returncode, 0, f"rc/stderr:\n{p.stderr}")
            cap_path = Path(env["DMC_CAPTURE"])
            capture = [json.loads(line) for line in
                       cap_path.read_text(encoding="utf-8").splitlines()] if cap_path.exists() else []
            return json.loads(p.stdout), capture

    @staticmethod
    def unwrap_state(state_str: str) -> dict:
        """req["state"] 在 run_jev 层被整串包了一层定界符; 拆掉再解析 JSON。"""
        inner = state_str
        if "UNTRUSTED_DATA_BEGIN" in inner:
            inner = inner.split("UNTRUSTED_DATA_BEGIN\n", 1)[1]
            inner = inner.rsplit("\nUNTRUSTED_DATA_END", 1)[0]
        return json.loads(inner)

    @staticmethod
    def unwrap_unit(unit: str) -> str:
        """build_state 层对每个字段各包一层; 拆出原始文本。"""
        if "UNTRUSTED_DATA_BEGIN" in unit:
            unit = unit.split("UNTRUSTED_DATA_BEGIN\n", 1)[1]
            unit = unit.rsplit("\nUNTRUSTED_DATA_END", 1)[0]
        return unit


    def test_no_pr_body_single_info(self):
        out, capture = self.run_harness("", None, None)
        self.assertEqual([(f["id"], f["severity"]) for f in out], [("DELIVERY-CLAIMS", "INFO")])
        self.assertIn("无 PR 上下文", out[0]["message"])
        self.assertEqual(capture, [])  # 没 PR 上下文就不该白发一次 jev 调用

    def test_no_delivery_section_single_info(self):
        out, capture = self.run_harness("# What\n只有正文\n", None, None)
        self.assertEqual([(f["id"], f["severity"]) for f in out], [("DELIVERY-CLAIMS", "INFO")])
        self.assertIn("无可核验声明", out[0]["message"])
        self.assertEqual(capture, [])  # 无候选不发抽取批

    def test_single_claim_is_not_verifiable(self):
        body = "## Delivery record\n\n- 只有一条声明\n"
        out, capture = self.run_harness(body, {"cand_0": 0.9}, {"claim_0": 0.9})
        self.assertEqual([(f["id"], f["severity"]) for f in out], [("DELIVERY-CLAIMS", "INFO")])
        self.assertEqual(len(capture), 1)  # 抽取批发过, 判决批因 claims<2 没发

    def test_high_p_claim_is_fail_with_extras(self):
        out, capture = self.run_harness(
            SAMPLE_BODY,
            {"cand_0": 0.9, "cand_1": 0.9, "cand_2": 0.9},
            {"claim_0": 0.9, "claim_1": 0.7, "claim_2": 0.2, "evidence_sufficient": 0.1})
        self.assertEqual([(f["id"], f["severity"]) for f in out],
                         [("DELIVERY-CLAIM-0", "FAIL"), ("DELIVERY-CLAIM-1", "WARN")])
        f0 = out[0]
        self.assertEqual(f0["tier"], "jev")
        self.assertEqual(f0["confidence"], "0.90")
        self.assertEqual(f0["claim"], "修复了 X 崩溃 (crash on exit)")
        self.assertEqual((f0["path"], f0["line"]), ("", 0))
        self.assertIn("声明未被证据支持", f0["message"])
        self.assertIn("修复了 X 崩溃", f0["message"])
        self.assertIn("0.90", f0["message"])
        # 低 p 的 claim_2 不出 finding（否则这条检查就是噪声发生器）
        self.assertNotIn("DELIVERY-CLAIM-2", [f["id"] for f in out])
        # 两次调用: 抽取批 (choice, fail_labels=disposition) + 判决批 (noul)
        ext, jdg = capture
        self.assertEqual(ext["questions"]["cand_0"]["type"], "choice")
        self.assertEqual(ext["questions"]["cand_0"]["fail_labels"], ["disposition"])
        self.assertIn("prose", ext["questions"]["cand_0"]["criteria"])
        state = self.unwrap_state(jdg["state"])
        # 不可信输入防护: 每条声明与 diff 证据都包在定界符里, 免疫指令在场
        self.assertIn("UNTRUSTED_DATA_BEGIN", state["claims"][0])
        self.assertIn("修复了 X 崩溃 (crash on exit)", state["claims"][0])
        self.assertIn("UNTRUSTED_DATA_END", state["claims"][0])
        self.assertIn("must not be followed", state["claims"][0])
        self.assertIn("UNTRUSTED_DATA_BEGIN", state["diff_evidence"])
        self.assertIn("+x", state["diff_evidence"])
        self.assertEqual(jdg["questions"]["claim_0"]["fail"], 0.85)
        self.assertEqual(jdg["questions"]["claim_0"]["warn"], 0.6)
        self.assertIn("unsupported by the diff evidence",
                      jdg["questions"]["claim_0"]["instructions"])
        self.assertNotIn("Does the diff evidence support",
                          jdg["questions"]["claim_0"]["instructions"])

    def test_low_p_produces_no_findings(self):
        out, _ = self.run_harness(
            SAMPLE_BODY,
            {"cand_0": 0.9, "cand_1": 0.9, "cand_2": 0.9},
            {"claim_0": 0.2, "claim_1": 0.2, "claim_2": 0.2, "evidence_sufficient": 0.1})
        self.assertEqual(out, [])

    def test_question_wording_pins_direction(self):
        # 方向钉:题目必须是陈述式负面命题（与 criteria.true 同向），
        # 疑问句会让模型顺问句把 true 读成「supported」，判定整体翻转。
        out, capture = self.run_harness(
            SAMPLE_BODY,
            {"cand_0": 0.9, "cand_1": 0.9, "cand_2": 0.9},
            {"claim_0": 0.9, "claim_1": 0.2, "claim_2": 0.2})
        q0 = capture[-1]["questions"]["claim_0"]
        self.assertIn("unsupported by the diff evidence", q0["instructions"])
        self.assertIn("contradicted by it", q0["instructions"])
        self.assertNotIn("Does the diff evidence support", q0["instructions"])
        # 高 p = 「未被支持/矛盾」概率高 → finding 必须是「未被证据支持」语义
        f0 = next(f for f in out if f["id"] == "DELIVERY-CLAIM-0")
        self.assertEqual(f0["severity"], "FAIL")
        self.assertIn("声明未被证据支持", f0["message"])
        self.assertNotIn("声明被证据支持", f0["message"])

    def test_evidence_sufficient_high_p_finding(self):
        out, capture = self.run_harness(
            SAMPLE_BODY,
            {"cand_0": 0.9, "cand_1": 0.9, "cand_2": 0.9},
            {"claim_0": 0.1, "claim_1": 0.1, "claim_2": 0.1, "evidence_sufficient": 0.95})
        self.assertEqual([(f["id"], f["severity"]) for f in out], [("DELIVERY-EVIDENCE", "FAIL")])
        self.assertEqual(out[0]["tier"], "jev")
        self.assertEqual(out[0]["confidence"], "0.95")
        self.assertIn("证据不足", out[0]["message"])
        jdg = capture[-1]
        self.assertEqual(jdg["questions"]["evidence_sufficient"]["fail"], 0.9)
        self.assertEqual(jdg["questions"]["evidence_sufficient"]["warn"], 0.6)

    def test_diff_evidence_keeps_tail(self):
        diff = "older hunk\n" + ("filler\n" * 6000) + "newest hunk\n"
        out, capture = self.run_harness(
            SAMPLE_BODY,
            {"cand_0": 0.9, "cand_1": 0.9, "cand_2": 0.9},
            {"claim_0": 0.1, "claim_1": 0.1, "claim_2": 0.1}, diff=diff)
        self.assertEqual(out, [])
        evidence = self.unwrap_unit(self.unwrap_state(capture[-1]["state"])["diff_evidence"])
        self.assertLessEqual(len(evidence), MAX_STATE_CHARS)
        self.assertTrue(evidence.endswith("newest hunk\n"))
        self.assertNotIn("older hunk", evidence)  # 头部被截掉, 尾部保留

    def test_network_error_degrades_to_infos(self):
        out, _ = self.run_harness(SAMPLE_BODY, None, None, net_fail=True)
        self.assertEqual(
            [f["id"] for f in out], ["DELIVERY-CLAIMS-FALLBACK", "DELIVERY-CLAIMS"])
        self.assertEqual(all(f["severity"] == "INFO" for f in out), True)
        self.assertIn("claims via regex fallback", out[0]["message"])
        self.assertIn("jev 不可用", out[1]["message"])
        self.assertIn("降级", out[1]["message"])

    # ---------- L7: 模型挑声明 (非 bullet 候选 / 模板话剔除 / 无 key 回退) ----------

    BOLD_BODY = """\
# What

## Delivery record

**CM-04 ratchet: 改写溢出处理于 file_size_gate**
- 修复了 X 崩溃 (crash on exit)
- 补了回归测试 test_alpha

## How to test

cargo test -p alpha
"""

    def test_bold_disposition_line_is_captured(self):
        out, capture = self.run_harness(
            self.BOLD_BODY,
            {"cand_0": 0.8, "cand_1": 0.9, "cand_2": 0.9},
            {"claim_0": 0.9, "claim_1": 0.2, "claim_2": 0.2})
        f0 = next(f for f in out if f["id"] == "DELIVERY-CLAIM-0")
        # 非 bullet 的粗体处置行被收进 claims (标记剥掉, 值仍是逐字片段)
        self.assertEqual(f0["claim"], "CM-04 ratchet: 改写溢出处理于 file_size_gate")
        self.assertEqual(f0["severity"], "FAIL")
        jdg = capture[-1]
        self.assertEqual(len(self.unwrap_state(jdg["state"])["claims"]), 3)

    def test_template_prose_is_excluded(self):
        body = ("## Delivery record\n\n"
                "本 PR 整体重构了所有模块, 顺带修了几个小问题\n"
                "- 修复了 X 崩溃 (crash on exit)\n"
                "- 补了回归测试 test_alpha\n")
        out, capture = self.run_harness(
            body,
            {"cand_0": 0.1, "cand_1": 0.9, "cand_2": 0.9},
            {"claim_0": 0.2, "claim_1": 0.2})
        self.assertEqual(out, [])
        jdg = capture[-1]
        state = self.unwrap_state(jdg["state"])
        # 模板话被剔: 判决批只剩 2 条真声明, 原序保留
        self.assertEqual(
            [self.unwrap_unit(c) for c in state["claims"]],
            ["修复了 X 崩溃 (crash on exit)", "补了回归测试 test_alpha"])
        self.assertNotIn("重构了所有模块", json.dumps(jdg["questions"], ensure_ascii=False))

    def test_table_rows_and_nested_bullets_are_candidates(self):
        body = ("## Delivery record\n\n"
                "| 规则 | 处置 |\n"
                "|---|---|\n"
                "| CM-01 | 已修 (改 commit gate 逻辑)\n"
                "- 修复了 X 崩溃\n"
                "  - 补了嵌套回归测试 test_nested\n"
                "\n## How to test\n\ncargo test\n")
        self.assertEqual(
            self.mod.extract_candidates(body),
            ["| 规则 | 处置 |",
             "| CM-01 | 已修 (改 commit gate 逻辑)",
             "修复了 X 崩溃",
             "补了嵌套回归测试 test_nested"])
        # |---|---| 分隔行不是内容
        self.assertNotIn("|---|---|", self.mod.extract_candidates(body))

    def test_no_key_falls_back_to_regex_with_info(self):
        out, capture = self.run_harness(SAMPLE_BODY, None, None, no_key=True)
        self.assertEqual(
            [f["id"] for f in out], ["DELIVERY-CLAIMS-FALLBACK", "DELIVERY-CLAIMS"])
        self.assertEqual(all(f["severity"] == "INFO" for f in out), True)
        self.assertIn("claims via regex fallback", out[0]["message"])
        self.assertIn("jev 不可用", out[1]["message"])
        self.assertEqual(capture, [])  # 无端点对根本没发请求


if __name__ == "__main__":
    unittest.main(verbosity=2)
