#!/usr/bin/env python3
"""rule_selector.py 的判决语义回归测试(unittest + mock post, 不碰网络)。

钉住的是弃权式 advisory 的边界, 不是实现:
  - glob 已命中的文件不发问(确定性 glob 保持权威), 只对漏网文件提问;
  - p(governs) ≥ 0.85 才出 WARN(封顶), 低 p 无 finding, 每文件 ≤5 条建议;
  - 候选规则 >25 先粗筛, 单次调用题数 ≤12;
  - 无 jev 端点对 → 单条 INFO 降级, 永不阻断; 规则目录不可读 → 同样 INFO。
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import review_chain
import rule_selector as rs

ENV_KEYS = {"JEV_API_KEY": "k", "JEV_API_BASE_URL": "https://jev.example/v1"}

ALPHA = """\
# CK-ALPHA: alpha 规则 intent(测试用, 管 .py)
enabled: true
sla: l2
match:
  paths_include: ["**/*.py"]
"""
SLOP = """\
# CK-SLOP-COMMENT: AI slop 注释(测试用, 排除 tests)
enabled: true
sla: l2
match:
  paths_include: ["**/*.py"]
  paths_exclude: ["**/tests/**"]
"""
BETA = """\
# CK-BETA: beta 规则 intent(测试用, 管 .rs)
enabled: true
sla: l2
match:
  paths_include: ["**/*.rs"]
"""
CATCHALL = """\
# CK-GAMMA: 纯 catch-all 判决层规则(测试用, 应被排除出目录)
enabled: true
sla: l3
match:
  paths_include: ["**/*"]
"""
DISABLED = """\
# CK-DELTA: 停用规则(测试用, 应被排除出目录)
enabled: false
sla: l2
match:
  paths_include: ["**/*.py"]
"""


def make_catalog(rules: dict) -> Path:
    root = Path(tempfile.mkdtemp(prefix="rs_cat_"))
    q = root / "specs" / "quality"
    q.mkdir(parents=True)
    for name, text in rules.items():
        (q / name).write_text(text, encoding="utf-8")
    return root


def answers(question_ids, probs):
    """造 systemone 响应: 每个问题一个 choice 答案。"""
    return {"answers": {
        q: {"type": "choice", "probabilities": dict(zip(question_ids, probs))}
        for q in question_ids}}


def base_diff():
    return (
        "diff --git a/a.py b/a.py\n"
        "--- a/a.py\n+++ b/a.py\n@@ -1 +1 @@\n-x\n+y\n"
        "diff --git a/b.rs b/b.rs\n"
        "--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-x\n+y\n"
        "diff --git a/deploy.env b/deploy.env\n"
        "--- /dev/null\n+++ b/deploy.env\n@@ -0,0 +1 @@\n+# step 1: parse config\n"
    )


class RuleSelectorTest(unittest.TestCase):
    def _run(self, root, diff, post_impl):
        with mock.patch.object(rs, "post", side_effect=post_impl), \
             mock.patch.dict(os.environ, ENV_KEYS, clear=False):
            return rs.run(str(root), diff)

    def _post(self, captured):
        def impl(url, headers, body, timeout):
            captured.append(json.loads(json.dumps(body)))
            return answers(list(body["questions"]), [0.0, 0.0])
        return impl

    def test_catalog_only_domain_rules(self):
        root = make_catalog({
            "checklist_alpha.yaml": ALPHA,
            "checklist_slop_comment.yaml": SLOP,
            "checklist_beta.yaml": BETA,
            "checklist_gamma.yaml": CATCHALL,
            "checklist_delta.yaml": DISABLED,
        })
        cat = rs.load_catalog(str(root))
        ids = [r["id"] for r in cat]
        # catch-all(l3)与停用规则不进目录
        self.assertEqual(ids, ["CHECKLIST-ALPHA", "CHECKLIST-BETA", "CHECKLIST-SLOP-COMMENT"])

    def test_member_repo_mirror_layout_loads(self):
        # 成员仓没有 specs/, 规则只在 .githooks/spec 镜像——读不到镜像
        # 就会把整个成员仓判成「无规则可建议」的假绿。
        root = Path(tempfile.mkdtemp(prefix="rs_mir_"))
        q = root / ".githooks" / "spec" / "quality"
        q.mkdir(parents=True)
        (q / "checklist_alpha.yaml").write_text(ALPHA, encoding="utf-8")
        cat = rs.load_catalog(str(root))
        self.assertEqual([r["id"] for r in cat], ["CHECKLIST-ALPHA"])

    def test_missing_catalog_root_is_loud(self):
        # 两个布局根都不存在 = 基建失败, 必须出 INFO, 禁止静默 [] 假绿。
        root = Path(tempfile.mkdtemp(prefix="rs_empty_"))
        out = self._run(root, base_diff(), self._post([]))
        self.assertEqual([f["severity"] for f in out], ["INFO"])
        self.assertIn("unreadable", out[0]["message"])

    def test_globs_covered_files_not_asked(self):
        root = make_catalog({
            "checklist_alpha.yaml": ALPHA,
            "checklist_slop_comment.yaml": SLOP,
            "checklist_beta.yaml": BETA,
        })
        captured = []
        out = self._run(root, base_diff(), self._post(captured))
        # a.py / b.rs 被 glob 命中 → 只有 deploy.env 发问(一次调用)
        self.assertEqual(len(captured), 1)
        # state 经 wrap_untrusted 定界包裹,解包后断言内层 JSON
        inner = review_chain.unwrap_untrusted(captured[0]["state"])
        self.assertEqual(json.loads(inner)["file"], "deploy.env")
        self.assertEqual(out, [])

    def test_low_probability_no_finding(self):
        root = make_catalog({
            "checklist_slop_comment.yaml": SLOP,
            "checklist_alpha.yaml": ALPHA,
        })
        captured = []

        def low_post(url, headers, body, timeout):
            captured.append(body)
            return answers(list(body["questions"]), [0.4, 0.0])  # p(governs)=0.4
        self.assertEqual(self._run(root, base_diff(), low_post), [])

    def test_high_probability_warn_capped(self):
        root = make_catalog({
            "checklist_slop_comment.yaml": SLOP,
            "checklist_alpha.yaml": ALPHA,
            "checklist_beta.yaml": BETA,
        })
        captured = []
        out = self._run(root, base_diff(), self._post_high(captured, {"rule_checklist-slop-comment": 0.9}))
        warns = [f for f in out if f["severity"] == "WARN"]
        self.assertEqual(len(warns), 1)
        self.assertEqual(warns[0]["id"], "RULE-SELECTOR-CHECKLIST-SLOP-COMMENT")
        self.assertEqual(warns[0]["path"], "deploy.env")
        self.assertIn("书面驳回", warns[0]["message"])

    def test_generated_mirror_and_docs_never_asked(self):
        # .githooks/ 生成镜像与 .md 散文按设计不被 code 规则 glob 管辖;
        # 若被问, advisory 会对每次 spec-sync 的镜像变更反复误报。
        root = make_catalog({
            "checklist_slop_comment.yaml": SLOP,
            "checklist_alpha.yaml": ALPHA,
        })
        diff = base_diff() + (
            "\ndiff --git a/.githooks/spec/docs/x.md b/.githooks/spec/docs/x.md\n"
            "--- a/.githooks/spec/docs/x.md\n+++ b/.githooks/spec/docs/x.md\n@@\n+hi\n"
            "diff --git a/specs/docs/SPEC.md b/specs/docs/SPEC.md\n"
            "--- a/specs/docs/SPEC.md\n+++ b/specs/docs/SPEC.md\n@@\n+hi\n"
            "diff --git a/specs/ledger/findings.jsonl b/specs/ledger/findings.jsonl\n"
            "--- a/specs/ledger/findings.jsonl\n+++ b/specs/ledger/findings.jsonl\n@@\n+{}\n")
        captured = []
        out = self._run(root, diff, self._post_high(captured, {}))
        asked = [json.loads(review_chain.unwrap_untrusted(b["state"]))["file"] for b in captured]
        for banned in (".githooks/spec/docs/x.md", "specs/docs/SPEC.md",
                       "specs/ledger/findings.jsonl"):
            self.assertNotIn(banned, asked)
        self.assertEqual([f for f in out if f["severity"] == "WARN"], [])

    @staticmethod
    def _post_high(captured, per_q):
        def impl(url, headers, body, timeout):
            captured.append(body)
            answers = {}
            for q, probs in per_q.items():
                answers[q] = {"type": "choice",
                              "probabilities": {"governs": probs, "not_relevant": 1.0 - probs}}
            return {"answers": answers}
        return impl

    def test_suggestions_capped_at_five(self):
        rules = {}
        for i in range(12):
            rules[f"checklist_pad{i:02d}.yaml"] = (
                f"# CK-PAD-{i:02d}: padding rule {i} (测试用)\n"
                "enabled: true\nsla: l2\nmatch:\n  paths_include: [\"**/*.py\"]\n"
            )
        rules["checklist_slop_comment.yaml"] = SLOP
        root = make_catalog(rules)
        captured = []
        high = {f"rule_checklist-pad{i:02d}": 0.9 for i in range(12)}
        out = self._run(root, "diff --git a/x.env b/x.env\n+++ b/x.env\n@@ +1 @@\n+# step 1\n",
                        self._post_high(captured, high))
        warns = [f for f in out if f["severity"] == "WARN"]
        self.assertEqual(len(warns), 5)  # 每文件封顶 5 条建议

    def test_batching_caps_questions_at_twelve(self):
        rules = {}
        for i in range(26):
            rules[f"checklist_many{i:02d}.yaml"] = (
                f"# CK-MANY-{i:02d}: python rule {i} (测试用)\n"
                f"enabled: true\nsla: l2\nmatch:\n  paths_include: [\"**/*.zz{i}\"]\n"
            )
        root = make_catalog(rules)
        captured = []
        self._run(root, "diff --git a/a.py b/a.py\n+++ b/a.py\n@@ +1 @@\n+# step 1\n",
                   self._post(captured))
        self.assertEqual(len(captured), 1)
        self.assertLessEqual(len(captured[0]["questions"]), rs.MAX_QUESTIONS)

    def test_no_key_single_info_no_post(self):
        root = make_catalog({"checklist_slop_comment.yaml": SLOP})
        captured = []
        env = {k: "" for k in ENV_KEYS}
        env["TYPESAFE_API_KEY"] = ""
        env["TYPESAFE_API_BASE"] = ""
        with mock.patch.object(rs, "post", side_effect=self._post(captured)), \
             mock.patch.dict(os.environ, env, clear=False):
            out = rs.run(str(root), base_diff())
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["severity"], "INFO")
        self.assertIn("no jev endpoint pair", out[0]["message"])
        self.assertEqual(captured, [])

    def test_catalog_unreadable_single_info(self):
        root = make_catalog({"checklist_slop_comment.yaml": SLOP})
        with mock.patch.object(rs, "load_catalog", side_effect=RuntimeError("boom")), \
             mock.patch.dict(os.environ, ENV_KEYS, clear=False):
            out = rs.run(str(root), base_diff())
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["severity"], "INFO")
        self.assertIn("unreadable", out[0]["message"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
