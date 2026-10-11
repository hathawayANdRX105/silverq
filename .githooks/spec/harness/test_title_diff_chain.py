#!/usr/bin/env python3
"""title_diff_chain.py 的判定语义回归测试（unittest + fake urlopen，不碰网络/gh）。

钉住的形态：

  - 拿不到 PR 标题/正文（无 env 钩子且 gh 失败 / 钩子文件不可读）→ 单条 INFO
    跳过，且**不**发 jev 请求；
  - mismatch 题方向是陈述式负面命题（true=描述与 diff 矛盾/失配, HIGH=BAD）：
    高 p 出 finding，低 p 无 finding；
  - severity 封顶 WARN：p 过 fail 线 0.85 也只出 WARN（semantic advisory），
    绝不出 FAIL；
  - 无端点对（无 key）→ 单条 INFO「jev 不可用」降级, rc=0 不阻断。

进程形态与 test_delivery_chain 同：fake urlopen 换掉真网络，请求体落盘
供断言（state 里要带标题/正文/diff，题目要带陈述式措辞），回放录好的
noul 概率。gh 分支用「无钩子 + 本仓无 open PR」让 gh 自然失败，或单测
层 mock subprocess.run，不依赖机器上 gh 的在场与否。
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
from types import SimpleNamespace
from unittest import mock

HERE = Path(__file__).parent
HARNESS = HERE / "title_diff_chain.py"

PRELUDE = """\
import json, os, urllib.request


def fake_urlopen(req, timeout=None):
    with open(os.environ["TDC_CAPTURE"], "a", encoding="utf-8") as fh:
        fh.write(req.data.decode() + "\\n")
    prob = json.loads(os.environ["TDC_PROB"])
    payload = {"answers": {"mismatch": {"type": "noul", "noul": prob}}}

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
import os, sys
sys.path.insert(0, {tmp!r})
import fakelocal  # noqa: F401
sys.path.insert(0, {workdir!r})
sys.stdin = open(os.environ["TDC_DIFF"], encoding="utf-8")
sys.argv = ["title_diff_chain.py"]
import title_diff_chain
sys.exit(title_diff_chain.main())
"""


class TitleDiffChainTest(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("title_diff_chain", str(HARNESS))
        self.mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.mod)

    def _run(self, prob: float, title: str = "fix: 修内存泄漏", body: str = "body",
             diff: str = "diff --git a/x b/x\n+x", hooks: bool = True,
             title_hook: bool = True, body_hook: bool = True) -> tuple:
        """Run the harness in a real process; return (findings, last_request_or_None)."""
        with tempfile.TemporaryDirectory(prefix="tdc-") as tmp:
            env = dict(os.environ)
            for k in ("TYPESAFE_API_KEY", "TYPESAFE_API_BASE", "JEV_API_KEY",
                      "JEV_API_BASE_URL", "JEV_MODEL", "GATE_PR_TITLE_FILE", "GATE_PR_BODY_FILE"):
                env.pop(k, None)
            env["TYPESAFE_API_KEY"] = "test-key"
            if hooks:
                if title_hook:
                    tf = Path(tmp, "title.txt")
                    tf.write_text(title, encoding="utf-8")
                    env["GATE_PR_TITLE_FILE"] = str(tf)
                if body_hook:
                    bf = Path(tmp, "body.md")
                    bf.write_text(body, encoding="utf-8")
                    env["GATE_PR_BODY_FILE"] = str(bf)
            else:
                # gh 分支隔离:本仓可能有 open PR(如开发中的 PR),
                # 会让 gh pr view 真成功而破坏「无 PR」断言。
                # 把 PATH 指向空目录,gh 查不到 → OSError → INFO。
                env["PATH"] = str(Path(tmp, "nopath"))
            df = Path(tmp, "diff.txt")
            df.write_text(diff, encoding="utf-8")
            Path(tmp, "fakelocal.py").write_text(PRELUDE, encoding="utf-8")
            env.update({
                "TDC_DIFF": str(df),
                "TDC_CAPTURE": str(Path(tmp, "capture.jsonl")),
                "TDC_PROB": json.dumps(prob),
            })
            code = RUNNER.format(tmp=tmp, workdir=str(HERE))
            p = subprocess.run(
                [sys.executable, "-c", code],
                capture_output=True, text=True, env=env, cwd=str(HERE), check=False,
            )
            self.assertEqual(p.returncode, 0, f"rc/stderr:\n{p.stderr}")
            cap = Path(env["TDC_CAPTURE"])
            req = json.loads(cap.read_text(encoding="utf-8").splitlines()[-1]) if cap.exists() else None
            return json.loads(p.stdout), req

    # ---------- 降级路径 ----------

    def test_no_title_body_single_info_no_post(self):
        # 无钩子 + 本仓无 open PR：gh 分支自然失败 → 单条 INFO，且不发请求。
        out, req = self._run(0.9, hooks=False)
        self.assertEqual([(f["id"], f["severity"]) for f in out], [("TITLE-DIFF", "INFO")])
        self.assertIn("无 PR 标题/正文", out[0]["message"])
        self.assertIsNone(req)

    def test_unreadable_title_hook_degrades(self):
        with tempfile.TemporaryDirectory(prefix="tdc-") as tmp:
            env = dict(os.environ)
            for k in ("GATE_PR_TITLE_FILE", "GATE_PR_BODY_FILE"):
                env.pop(k, None)
            env["GATE_PR_TITLE_FILE"] = str(Path(tmp, "missing.txt"))
            old = dict(os.environ)
            os.environ.clear()
            os.environ.update(env)
            try:
                title, body, source = self.mod.fetch_title_body()
            finally:
                os.environ.clear()
                os.environ.update(old)
            self.assertEqual((title, body), ("", ""))
            self.assertIn("unreadable", source)

    def test_gh_provides_title_body_when_no_hooks(self):
        env = dict(os.environ)
        for k in ("GATE_PR_TITLE_FILE", "GATE_PR_BODY_FILE"):
            env.pop(k, None)
        old = dict(os.environ)
        os.environ.clear()
        os.environ.update(env)
        with mock.patch.object(
            self.mod.subprocess, "run", side_effect=[
                SimpleNamespace(stdout="fix: 修内存泄漏\n"),
                SimpleNamespace(stdout="改了分配器"),
            ],
        ):
            try:
                title, body, source = self.mod.fetch_title_body()
            finally:
                os.environ.clear()
                os.environ.update(old)
        self.assertEqual((title, body, source), ("fix: 修内存泄漏", "改了分配器", "gh"))

    def test_no_endpoint_pair_single_info(self):
        with tempfile.TemporaryDirectory(prefix="tdc-") as tmp:
            tf = Path(tmp, "title.txt")
            tf.write_text("fix: x", encoding="utf-8")
            df = Path(tmp, "diff.txt")
            df.write_text("diff\n+x", encoding="utf-8")
            Path(tmp, "fakelocal.py").write_text(PRELUDE, encoding="utf-8")
            env = dict(os.environ)
            for k in ("TYPESAFE_API_KEY", "JEV_API_KEY", "JEV_API_BASE_URL"):
                env.pop(k, None)
            env["GATE_PR_TITLE_FILE"] = str(tf)
            env.update({
                "TDC_DIFF": str(df),
                "TDC_CAPTURE": str(Path(tmp, "capture.jsonl")),
                "TDC_PROB": json.dumps(0.9),
            })
            code = RUNNER.format(tmp=tmp, workdir=str(HERE))
            p = subprocess.run(
                [sys.executable, "-c", code],
                capture_output=True, text=True, env=env, cwd=str(HERE), check=False,
            )
            self.assertEqual(p.returncode, 0, f"rc/stderr:\n{p.stderr}")
            out = json.loads(p.stdout)
            self.assertEqual([(f["id"], f["severity"]) for f in out], [("TITLE-DIFF", "INFO")])
            self.assertIn("jev 不可用", out[0]["message"])

    # ---------- 判定层 ----------

    def test_high_p_is_warn_not_fail(self):
        out, req = self._run(0.95)
        self.assertEqual(len(out), 1)
        f = out[0]
        self.assertEqual(f["id"], "TITLE-DIFF-MISMATCH")
        self.assertEqual(f["severity"], "WARN")  # 封顶: 0.95 >= fail 0.85 也只出 WARN
        self.assertNotIn("BLOCK", f["message"])  # 封顶后不该再写「fix or split before push」
        self.assertIn("semantic advisory", f["message"])
        self.assertEqual(f["tier"], "jev")
        self.assertEqual(f["confidence"], "0.95")
        self.assertEqual(req["questions"]["mismatch"]["fail"], 0.85)
        self.assertEqual(req["questions"]["mismatch"]["warn"], 0.6)

    def test_low_p_no_findings(self):
        out, _ = self._run(0.2)
        self.assertEqual(out, [])

    def test_state_carries_title_body_and_diff(self):
        _, req = self._run(0.9, title="fix: 修内存泄漏", body="改了分配器",
                            diff="diff --git a/x b/x\n+realloc fix")
        state = req["state"]
        self.assertIn("修内存泄漏", state)
        self.assertIn("改了分配器", state)
        self.assertIn("realloc fix", state)

    def test_question_direction_is_declarative(self):
        _, req = self._run(0.9)
        q = req["questions"]["mismatch"]
        self.assertIn("misdescribe or contradict", q["instructions"])
        self.assertNotIn("Does", q["instructions"])  # 疑问句会让 true 顺问句读反, 判定翻转
        self.assertIn("contradicts the diff", q["criteria"]["true"])

    def test_title_only_via_empty_body_hook_still_judges(self):
        # title 钩子在场、body 文件为空：body 缺失不算「拿不到」, 仍正常判决。
        out, _ = self._run(0.9, body_hook=False)
        self.assertEqual([(f["id"], f["severity"]) for f in out], [("TITLE-DIFF-MISMATCH", "WARN")])


if __name__ == "__main__":
    unittest.main(verbosity=2)
