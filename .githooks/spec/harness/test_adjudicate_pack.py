#!/usr/bin/env python3
"""adjudicate_pack.py 回归测试(unittest + mock post/gh, 不碰网络)。

钉住:
  - 待裁决定位: Delivery record 里 待裁决/pending/规则缺陷 标记条目 +
    findings 里 WARN 且无处置匹配(FAIL/INFO 与已处置的 WARN 不收);
  - 四选项概率表穷尽(标签集固定, 概率来自同一次调用);
  - 零写副作用: -o 缺省时只打 stdout, 不落任何文件; 有 -o 只写简报本身;
  - 无 key / 单条调用失败 → 纯证据包降级(degraded), rc=0 不阻断。
"""
from __future__ import annotations

import io
import json
import sys
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import adjudicate_pack as ap

PR_CTX = {"title": "t", "body": "", "files": [], "items": []}

BODY_WITH_MARKER = """\
## What
x

## Delivery record
- 评审发现(烟跑): repo-wide 声明漏报边界 → 处置: 接受并书面记录 [待裁决]
- slop_comment: repair — decomposed the loop (#45)

## How to test
manual
"""

BODY_WITH_REJECTION = BODY_WITH_MARKER.replace("[待裁决]", "[pending adjudication]")


def run_main(argv, stdin="{}", post_impl=None, ctx_impl=None, env_key=False):
    with mock.patch.object(ap, "post", side_effect=post_impl or (lambda *a: {})), \
         mock.patch.object(ap, "run_gh",
                           side_effect=(ctx_impl or (lambda a: json.dumps(PR_CTX)))), \
         mock.patch.object(ap.jev_env, "resolve",
                           return_value=("https://x", "k", "m") if env_key else (None, None, "m")), \
         mock.patch("sys.argv", ["prog"] + argv), \
         mock.patch("sys.stdin", io.StringIO(stdin)), \
         mock.patch("sys.stdout", new=io.StringIO()) as out:
        rc = ap.main()
    return rc, out.getvalue()


def verdict_answers(**overrides):
    probs = {"accept_reject": 0.5, "demand_fix": 0.3, "revise_rule": 0.1,
             "insufficient_evidence": 0.1}
    probs.update(overrides)
    return {"answers": {"verdict": {"type": "choice", "probabilities": probs},
                       "confidence": {"type": "noul", "noul": 0.7}}}


class LocateTest(unittest.TestCase):
    def test_marker_items_from_delivery_record(self):
        # 待裁决标记形态: 直接验证 MARKER_PAT 抽取命中
        import delivery_chain
        got = [c for c in delivery_chain.extract_claims(BODY_WITH_MARKER)
               if ap.MARKER_PAT.search(c)]
        self.assertEqual(len(got), 1)
        self.assertIn("待裁决", got[0])
        self.assertNotIn("slop_comment: repair", " ".join(got))

    def test_unhandled_findings_match_by_rule_name(self):
        findings = [
            {"id": "SLOP-COMMENT", "severity": "WARN", "path": "a.py", "line": 1},
            {"id": "CCN", "severity": "WARN", "path": "b.py", "line": 2},
            {"id": "CCN", "severity": "FAIL", "path": "b.py", "line": 3},
            {"id": "DUPLICATION", "severity": "INFO", "path": "c.py", "line": 4},
        ]
        # body 里 slop_comment 有处置 → SLOP-COMMENT 的 WARN 被排除; CCN 无处置 → 收
        body = "## Delivery record\n- slop_comment: repair — decomposed\n"
        out = ap.unhandled_findings(findings, body)
        self.assertEqual([f["id"] for f in out], ["CCN"])

    def test_no_marker_no_unhandled_is_empty(self):
        rc, out = run_main(["--pr", "45"], ctx_impl=lambda a: json.dumps(
            dict(PR_CTX, body="## Delivery record\n- all good\n")))
        self.assertEqual(rc, 0)
        self.assertIn("无待裁决项", out)


class ReportTest(unittest.TestCase):
    def test_four_options_exhaustive(self):
        findings = json.dumps([{"id": "CCN", "severity": "WARN", "path": "b.py",
                                 "line": 2, "message": "ccn high"}])
        rc, out = run_main(["--stdin"], stdin=findings, post_impl=lambda *a: verdict_answers(),
                           ctx_impl=lambda a: json.dumps(dict(
                               PR_CTX, body="## Delivery record\n- all good\n")), env_key=True)
        self.assertEqual(rc, 0)
        for k in ap.VERDICT_LABELS:
            self.assertIn(f"| {k} |", out)
        self.assertIn("证据充分度 confidence = 0.70", out)
        self.assertIn("维护者裁决, 本工具不代批", out)

    def test_no_key_degrades_to_evidence_only(self):
        called = []

        def post_impl(url, headers, body, timeout):
            called.append(url)

        findings = json.dumps([{"id": "CCN", "severity": "WARN", "path": "b.py"}])
        rc, out = run_main(["--stdin"], stdin=findings, post_impl=post_impl,
                           ctx_impl=lambda a: json.dumps(dict(
                               PR_CTX, body="## Delivery record\n- all good\n")))
        self.assertEqual(rc, 0)
        self.assertEqual(called, [])
        self.assertIn("_(degraded: 无 jev 端点/调用失败", out)

    def test_single_item_failure_degrades_that_item_only(self):
        calls = []

        def post_impl(url, headers, body, timeout):
            calls.append(len(calls))
            if len(calls) == 1:
                raise RuntimeError("boom")
            return verdict_answers()

        findings = json.dumps([
            {"id": "CCN", "severity": "WARN", "path": "b.py"},
            {"id": "DUPLICATION", "severity": "WARN", "path": "c.py"},
        ])
        rc, out = run_main(["--stdin"], stdin=findings, post_impl=post_impl,
                           ctx_impl=lambda a: json.dumps(dict(
                               PR_CTX, body="## Delivery record\n- all good\n")), env_key=True)
        self.assertEqual(rc, 0)
        self.assertEqual(len(calls), 2)
        self.assertIn("待裁决 1", out)
        self.assertIn("待裁决 2", out)
        self.assertIn("_(degraded: 本条调用失败", out)
        self.assertIn("insufficient_evidence", out)  # 第二条仍有概率表

    def test_no_write_side_effect_default_stdout(self):
        findings = json.dumps([{"id": "CCN", "severity": "WARN", "path": "b.py"}])
        rc, out = run_main(["--stdin"], stdin=findings,
                           ctx_impl=lambda a: json.dumps(dict(
                               PR_CTX, body="## Delivery record\n- all good\n")))
        self.assertEqual(rc, 0)
        self.assertIn("# 裁决简报", out)  # 简报在 stdout, 未落文件

    def test_usage_requires_input(self):
        with redirect_stderr(io.StringIO()) as err, \
             mock.patch("sys.argv", ["prog"]), \
             mock.patch("sys.stdin", io.StringIO("[]")), \
             mock.patch("sys.stdout", new=io.StringIO()):
            rc = ap.main()
        self.assertEqual(rc, 2)
        self.assertIn("usage", err.getvalue())


if __name__ == "__main__":
    unittest.main(verbosity=2)
