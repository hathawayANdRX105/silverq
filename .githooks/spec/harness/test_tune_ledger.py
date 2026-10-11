#!/usr/bin/env python3
"""tune_ledger.py 回归测试(unittest + mock gh/post, 不碰网络/仓配置)。

钉住离线账本的三条生命线:
  - 账本字段最小集(ts/pr/rule_id/action/reason_sha/has_evidence), 不存 PR 原文;
  - collect 幂等: 同一 (pr, rule_id, action) 只记一次;
  - 聚合门槛(样本 ≥5 且 reject 占比 ≥0.5)与 sound 占比 ≥0.6 才出提案;
  - **零自动改配置**: propose 跑完 specs/ 下任何文件 mtime/size 不变,
    collect 只追加账本本身。
"""
from __future__ import annotations

import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import delivery_chain
import tune_ledger as tl

BODY = """\
## What
x

## Delivery record
- slop_comment: repair — decomposed the narration loop (#45)
- ccn: 驳回 — the branchy code is inherent, no split planned
- overall structure is fine, no action needed

## How to test
manual
"""


def gh_list(body):
    return json.dumps([{"number": 45, "title": "t", "body": body}])


def gh_view_45(body):
    return json.dumps({"body": body})


def make_ledger(tmp, rows):
    p = Path(tmp) / "ledger.jsonl"
    p.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
    return str(p)


class ClassifyClaimsTest(unittest.TestCase):
    """抽取语义: 只收引用规则名的处置条目, action 二分。"""

    def _classify(self, claims):
        stems = ["slop-comment", "ccn", "duplication"]
        with mock.patch.object(tl, "rule_stems", return_value=stems):
            return tl.classify_claims(claims)

    def test_action_and_rule_extraction(self):
        items = self._classify(delivery_chain.extract_claims(BODY))
        got = {(i["rule_id"], i["action"]) for i in items}
        self.assertEqual(got, {("slop-comment", "repair"), ("ccn", "reject")})
        # 无规则引用的段落不进账本
        self.assertEqual(len(items), 2)
        by_rule = {i["rule_id"]: i for i in items}
        self.assertIn("decomposed the narration loop", by_rule["slop-comment"]["reason"])
        self.assertFalse(tl.REJECT_PAT.search(by_rule["slop-comment"]["reason"]))

    def test_ck_fallback_without_catalog(self):
        items = self._classify(["CK-DUPLICATION: 驳回 — not copy-paste, two distinct helpers"])
        self.assertEqual(items[0]["rule_id"], "duplication")
        self.assertEqual(items[0]["action"], "reject")

    def test_evidence_flag(self):
        items = self._classify(["ccn: 驳回 — no split, verified by smoke test (#46)"])
        self.assertTrue(items[0]["has_evidence"] if "has_evidence" in items[0]
                        else bool(tl.EVIDENCE_PAT.search(items[0]["reason"])))


class CollectTest(unittest.TestCase):
    def test_idempotent_and_minimal_fields(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = make_ledger(tmp, [])
            with mock.patch.object(tl, "run_gh", side_effect=lambda a, r: gh_list(BODY)):
                self.assertEqual(tl.cmd_collect(30, None, ledger), 0)
            with open(ledger, encoding="utf-8") as fh:
                rows1 = [json.loads(ln) for ln in fh]
            self.assertEqual(len(rows1), 2)
            for r in rows1:
                # 字段最小集: 账本进仓, 不存 PR 原文
                self.assertEqual(set(r), {"ts", "pr", "rule_id", "action",
                                          "reason_sha", "has_evidence"})
                self.assertNotIn(BODY, json.dumps(r))
                self.assertRegex(r["reason_sha"], r"^[0-9a-f]{12}$")
            # 幂等: 再跑一次零新增
            with mock.patch.object(tl, "run_gh", side_effect=lambda a, r: gh_list(BODY)), \
                 redirect_stdout(io.StringIO()) as out:
                self.assertEqual(tl.cmd_collect(30, None, ledger), 0)
            self.assertIn("0 new", out.getvalue())
            with open(ledger, encoding="utf-8") as fh:
                self.assertEqual(len([json.loads(x) for x in fh]), 2)


class ProposeTest(unittest.TestCase):
    REJECTS = 5

    def _rows(self, rule="ccn", n_reject=REJECTS, n_repair=0):
        import hashlib
        rows = []
        for i in range(n_repair):
            rows.append({"ts": "t", "pr": 45, "rule_id": rule, "action": "repair",
                         "reason_sha": hashlib.sha256(f"r{i}".encode()).hexdigest()[:12],
                         "has_evidence": True})
        for i in range(n_reject):
            rows.append({"ts": "t", "pr": 45, "rule_id": rule, "action": "reject",
                         "reason_sha": hashlib.sha256(f"rej{i}".encode()).hexdigest()[:12],
                         "has_evidence": False})
        return rows

    def _propose(self, ledger, env_key=False):
        env = {"JEV_API_KEY": "k", "JEV_API_BASE_URL": "https://jev.example/v1"} \
            if env_key else {k: "" for k in ("JEV_API_KEY", "TYPESAFE_API_KEY")}
        with mock.patch.dict(os.environ, env, clear=False), \
             redirect_stdout(io.StringIO()) as out:
            tl.cmd_propose(ledger, None)
        return out.getvalue()

    def test_sample_below_threshold_no_proposal(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = make_ledger(tmp, self._rows(n_reject=3, n_repair=1))
            text = self._propose(ledger)
            self.assertIn("无可提案", text)

    def test_reject_share_below_threshold_no_proposal(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = make_ledger(tmp, self._rows(n_reject=2, n_repair=5))
            text = self._propose(ledger)
            self.assertIn("无可提案", text)

    def test_no_jev_degrades_to_stats_with_label(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = make_ledger(tmp, self._rows())
            text = self._propose(ledger)
            self.assertIn("ccn — 提案", text)
            self.assertIn("no-jev: quality unclassified", text)
            self.assertIn("须维护者批准", text)

    def test_sound_share_gate(self):
        import hashlib
        reason = "the branchy code is inherent, no split planned"
        body = ("## What\nx\n\n## Delivery record\n"
                f"- ccn: 驳回 — {reason}\n- ccn: 驳回 — {reason}\n"
                f"- ccn: 驳回 — {reason}\n- ccn: 驳回 — {reason}\n"
                f"- ccn: 驳回 — {reason}\n")
        rows = []
        for i in range(5):
            rows.append({"ts": "t", "pr": 45, "rule_id": "ccn", "action": "reject",
                         "reason_sha": hashlib.sha256(reason.encode()).hexdigest()[:12],
                         "has_evidence": False})
        with tempfile.TemporaryDirectory() as tmp:
            ledger = make_ledger(tmp, rows)

            def fake_run_gh(args, repo):
                if args[0] == "pr" and args[1] == "view":
                    return gh_view_45(body)
                raise AssertionError(f"unexpected gh args {args}")

            def fake_post(url, headers, body_b, timeout):
                answers = {}
                probs = [0.9, 0.9, 0.1, 0.1, 0.9]
                for k, p in enumerate(probs):
                    answers[f"rej_{k}"] = {"type": "choice",
                                            "probabilities": {"sound": p, "rubber_stamp": 1 - p}}
                return {"answers": answers}

            env = {"JEV_API_KEY": "k", "JEV_API_BASE_URL": "https://jev.example/v1"}
            with mock.patch.dict(os.environ, env, clear=False), \
                 mock.patch.object(tl, "run_gh", side_effect=fake_run_gh), \
                 mock.patch.object(tl, "post", side_effect=fake_post), \
                 redirect_stdout(io.StringIO()) as out:
                tl.cmd_propose(ledger, None)
            # 5 条驳回中 3 条 sound(p≥0.5) → 0.6 恰好过线 → 出提案
            self.assertIn("ccn — 提案", out.getvalue())
            with mock.patch.dict(os.environ, env, clear=False), \
                 mock.patch.object(tl, "run_gh", side_effect=fake_run_gh), \
                 mock.patch.object(tl, "post",
                                   side_effect=lambda u, h, b, t: {"answers": {
                                       f"rej_{k}": {"type": "choice", "probabilities":
                                                    {"sound": 0.1, "rubber_stamp": 0.9}}
                                       for k in range(5)}}), \
                 redirect_stdout(io.StringIO()) as out2:
                tl.cmd_propose(ledger, None)
            self.assertIn("暂不提案", out2.getvalue())


class ZeroWriteTest(unittest.TestCase):
    """零自动改配置: 工具跑完, specs/ 下(除账本外)任何文件不变。"""

    def _snapshot(self, root):
        snap = {}
        for p in Path(root).rglob("*"):
            if p.is_file():
                st = p.stat()
                snap[str(p)] = (st.st_mtime, st.st_size)
        return snap

    def test_propose_writes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            rows = []
            for i in range(5):
                rows.append({"ts": "t", "pr": 45, "rule_id": "ccn", "action": "reject",
                             "reason_sha": "a" * 12, "has_evidence": False})
            ledger = make_ledger(tmp, rows)
            fake_specs = Path(tmp) / "specs"
            (fake_specs / "quality").mkdir(parents=True)
            (fake_specs / "quality" / "checklist_x.yaml").write_text("enabled: true\n")
            env = {k: "" for k in ("JEV_API_KEY", "TYPESAFE_API_KEY")}
            snap = self._snapshot(fake_specs)
            with mock.patch.dict(os.environ, env, clear=False), \
                 redirect_stdout(io.StringIO()):
                tl.cmd_propose(ledger, None)
            self.assertEqual(snap, self._snapshot(fake_specs))

    def test_collect_only_touches_ledger(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = make_ledger(tmp, [])
            with mock.patch.object(tl, "run_gh", side_effect=lambda a, r: gh_list(BODY)):
                tl.cmd_collect(30, None, ledger)
            with open(ledger, encoding="utf-8") as fh:
                rows = [json.loads(x) for x in fh]
            self.assertEqual(len(rows), 2)
            # 除账本外无其它新文件
            others = [p for p in Path(tmp).glob("*") if p.name != "ledger.jsonl"]
            self.assertEqual(others, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
