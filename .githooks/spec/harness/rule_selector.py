#!/usr/bin/env python3
"""rule selector — 弃权式规则选择 advisory (mode:diff).

确定性 glob(paths_include)保持权威: 引擎照旧只跑 glob 命中的规则。
本 harness 只补「漏网」: 对没有任何域规则 glob 命中的变更文件, 问 jev
「哪些规则的 intent 语义上管辖这块改动」, 命中则 WARN 提示维护者把路径
补进该规则 match(或写书面驳回)。

为什么是弃权式: 全局 glob(`**/*`)与 l3 判决层规则(review_chain/
delivery_claims 等按 diff/PR 整体判决)对任何文件都「命中」, 但它们不是
域规则——把它们算进覆盖判定会让本工具对一切文件弃权, 失去意义。
故: 覆盖判定与候选清单都只含 l1/l2 且非纯 catch-all 的规则(见
_domain_scoped), 并跳过本规则自身。

判决方向: 每条候选规则一个 choice 题 rule_<id>, 标签穷尽
governs(该规则 intent 覆盖此改动)/ not_relevant(明显不归它管, 兜底),
fail_labels=["governs"], p(governs) ≥ 0.85 → 一条 WARN(封顶, yaml
fail_severity 同为 WARN——advisory 不拦截)。

降级契约: 无端点对 / 网络失败 / 目录不可读 → 单条 INFO, rc=0, 永不阻断。
凭据: 同 review_chain(JEV_API_* / TYPESAFE_API_*, 见 jev_env)。
手动跑法: git diff > /tmp/d && python3 specs/harness/rule_selector.py < /tmp/d
"""
from __future__ import annotations

import json
import os
import re
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jev_env
import jev_rule  # 复用引擎同款的宽松 glob 语义, 不自己重发明

try:
    import yaml
except ImportError:
    yaml = None

from review_chain import wrap_untrusted

HTTP_TIMEOUT = 40       # 每次调用; 4 文件 × 40s < yaml timeout 180s
MAX_FILES_PER_RUN = 4  # 未覆盖文件超过则只判前 4, 其余一条 INFO 说明
MAX_EXCERPT = 6000     # 每文件 diff 片段上限(字符)
MAX_SUGGEST = 5        # 每文件最多 5 条建议(按 p 降序)
MAX_QUESTIONS = 12     # 单次调用题数上限
COARSE_THRESHOLD = 25  # 候选规则 > 该数时按文件类型粗筛再入题
GOVERNS_P = 0.85       # p(governs) ≥ 0.85 才出 WARN
SELF_ID = "RULE-SELECTOR"
# 设计内未覆盖 = 不问: .githooks/ 是 canon-sync 生成镜像(正本 specs/ 才是输入);
# 散文文档与 ledger 数据按设计不被任何 code 规则 glob 管辖——引擎 paths_exclude
# 只管触发, 到不了这里, 必须在 harness 侧钉死, 否则 advisory 会对生成物反复误报。
SELF_EXCLUDE_PREFIX = (".githooks/", "specs/ledger/")
SELF_EXCLUDE_SUFFIX = (".md", ".mdx")

# 规则目录根: canon 正本仓是 specs/, 成员仓只有 canon-sync 分发的 .githooks/spec
# 镜像——两种布局都在; 正本存在时优先, 避免与镜像重复计目录。
CATALOG_ROOTS = ("specs", os.path.join(".githooks", "spec"))
CATALOG_SUBDIRS = ("quality", "dioxus", "code", "cleanup", "github", "workspace")
EXCLUDE_NAMES = {"severity_overrides.yaml", "dispatch.yaml"}
# 粗筛后保留集为空时的兜底: 语言无关的通用域规则(文件类型恰好没被任何
# 规则 glob 覆盖时, 这些通用规则仍可能被 jev 判「该管」——正是漏网场景)。
GENERAL_FALLBACK = ("CHECKLIST-SLOP-COMMENT", "CHECKLIST-ANTISLOP",
                    "CHECKLIST-HARDCODED-SECRET", "CHECKLIST-SEMANTIC-DEFAULT")


def _globs_of(rule: dict) -> tuple:
    """该规则 glob(match 段或顶层, 两套 yml 形状归一)。"""
    m = rule.get("match") if isinstance(rule.get("match"), dict) else rule
    inc = [str(p) for p in (m.get("paths_include") or [])]
    exc = [str(p) for p in (m.get("paths_exclude") or [])]
    return inc, exc


def _specific_globs(rule: dict) -> list:
    """域 glob: 纯 catch-all(`**/*`/`**`)不算——判决层规则对任何文件都命中,
    计入覆盖判定会让本工具全弃权(模块 docstring)。"""
    inc, _ = _globs_of(rule)
    return [p for p in inc if p not in ("**/*", "**")]


def _domain_scoped(rule: dict) -> bool:
    """域规则 = l1/l2 且至少有一条域 glob(纯 `**/*` 的判决层规则不入围)。"""
    if str(rule.get("sla", "l2")) not in ("l1", "l2"):
        return False
    return bool(_specific_globs(rule))


def _rule_hit(rule: dict, rel: str) -> bool:
    """该规则 glob 是否命中此文件(覆盖判定只认域 glob, 排除项照旧生效)。"""
    _, exc = _globs_of(rule)
    for e in exc:
        if jev_rule.loose_match(rel, e):
            return False
    inc = _specific_globs(rule)
    return bool(inc) and any(jev_rule.loose_match(rel, p) for p in inc)


def _intent_of(text: str, fallback: str) -> str:
    """intent 条文 = 头注释里的 CK-* 行(规则描述); 取不到退回首条非空 # 行,
    再取不到用文件名。[jev-verified] 之类的过程注释不是规则条文。"""
    first_comment = ""
    for ln in text.splitlines():
        if not ln.lstrip().startswith("#"):
            break
        body = ln.lstrip("# ").strip()
        if not body:
            continue
        if not first_comment:
            first_comment = body
        if re.match(r"CK-[A-Z0-9-]+:", body):
            return body[:200]
    return first_comment[:200] or fallback


def load_catalog(root: str) -> list:
    """读规则 yaml: 正本仓 specs/, 成员仓 .githooks/spec 镜像, 两种布局都认;
    两个根都不存在 = 基建失败, 抛错(降级为可见 INFO), 禁止静默空目录假绿。
    返回域规则 [{id, intent, sla, paths_include, ...}]。"""
    if yaml is None:
        raise RuntimeError("pyyaml unavailable")
    bases = [os.path.join(root, c) for c in CATALOG_ROOTS
             if os.path.isdir(os.path.join(root, c))]
    if not bases:
        raise RuntimeError(f"no rule catalog under {'/'.join(CATALOG_ROOTS)} of {root}")
    rules = []
    for base in bases[:1]:
        for sub in CATALOG_SUBDIRS:
            dpath = os.path.join(base, sub)
            if not os.path.isdir(dpath):
                continue
            for name in sorted(os.listdir(dpath)):
                if not name.endswith(".yaml") or name in EXCLUDE_NAMES:
                    continue
                path = os.path.join(dpath, name)
                try:
                    with open(path, encoding="utf-8") as fh:
                        text = fh.read()
                    doc = yaml.safe_load(text) or {}
                except Exception as e:  # noqa: BLE001 - 读不了的规则 yaml 按跳过降级
                    print(f"rule_selector: skipping {name} ({e})", file=sys.stderr)
                    continue
                if not isinstance(doc, dict) or doc.get("enabled") is False:
                    continue
                rid = re.sub(r"[^a-z0-9]", "-", os.path.splitext(name)[0]).upper()
                if rid == SELF_ID:
                    continue
                m = doc.get("match") if isinstance(doc.get("match"), dict) else doc
                rule = {
                    "id": rid,
                    "intent": _intent_of(text, name),
                    "sla": str(doc.get("sla", "l2")),
                    "paths_include": [str(p) for p in (m.get("paths_include") or [])],
                    "paths_exclude": [str(p) for p in (m.get("paths_exclude") or [])],
                }
                if _domain_scoped(rule):
                    rules.append(rule)
    return rules


def parse_diff(payload: str) -> dict:
    """统一 diff → {rel_path: hunk_text}, 保序。"""
    files: dict = {}
    cur = None
    for ln in payload.splitlines():
        m = re.match(r"^diff --git a/(\S+) b/(\S+)", ln)
        if m:
            cur = m.group(2)
            files.setdefault(cur, "")
            continue
        if ln.startswith("+++ b/"):
            cur = ln[6:].strip() or cur
            files.setdefault(cur, "")
        elif cur is not None and not ln.startswith("--- "):
            files[cur] += ln + "\n"
    return files


def coarse_filter(rules: list, rel: str) -> list:
    """候选 > COARSE_THRESHOLD 时按文件类型粗筛: glob 含该扩展名, 或
    intent/id 提及该语言; 保留集过稀(<3)时补通用规则, 题数硬上限
    MAX_QUESTIONS。"""
    ext = rel.rsplit(".", 1)[-1].lower() if "." in rel else ""
    lang = ext or rel.rsplit("/", 1)[-1].lower()
    kept = [r for r in rules
            if any("." + ext in p for p in r.get("paths_include", []))
            or lang in (r.get("intent", "") + " " + r["id"]).lower()]
    if len(kept) < 3:
        kept += [r for r in rules if r["id"] in GENERAL_FALLBACK and r not in kept]
    return kept[:MAX_QUESTIONS]


def build_questions(rules: list, rel: str) -> dict:
    qs = {}
    for r in rules:
        qs[f"rule_{r['id'].lower()}"] = {
            "type": "choice",
            "instructions": (
                f"Rule {r['id']} (intent: {r['intent'][:160]}). "
                f"Does that intent govern the change to {rel} shown in state?"
            ),
            "labels": ["governs", "not_relevant"],
            "fail_labels": ["governs"],
            "criteria": {
                "governs": "the rule intent covers this file's change "
                           "(e.g. a slop-comment rule for this python edit)",
                "not_relevant": "clearly outside the rule's intent (catch-all label)",
            },
        }
    return qs


def post(url: str, headers: dict, body: dict, timeout: int):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json", **headers})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


def _info(message: str) -> dict:
    return {"id": SELF_ID, "severity": "INFO", "path": "", "line": 0,
            "message": message}


def _wrap(state: dict) -> str:
    """diff 是不可信输入: state 整体先截断再定界包裹。"""
    text = json.dumps(state, ensure_ascii=False)[:24000]
    return wrap_untrusted(text)


def run(root: str, diff: str) -> list:
    out: list = []
    base, key, model = jev_env.resolve()
    if not base or not key:
        out.append(_info("rule_selector: no jev endpoint pair configured "
                         "(JEV_API_* / TYPESAFE_API_*); 弃权式 advisory 跳过"))
        return out
    files = {p: d for p, d in parse_diff(diff).items()
             if not (p.startswith(SELF_EXCLUDE_PREFIX)
                     or p.endswith(SELF_EXCLUDE_SUFFIX))}
    if not files:
        return out
    try:
        catalog = load_catalog(root)
    except Exception as e:  # noqa: BLE001 - 目录不可读按基建失败降级
        out.append(_info(f"rule_selector: rule catalog unreadable ({e}); advisory 跳过"))
        return out
    open_files = [p for p in files if not any(_rule_hit(r, p) for r in catalog)]
    judged = open_files[:MAX_FILES_PER_RUN]
    skipped = open_files[MAX_FILES_PER_RUN:]
    if skipped:
        out.append(_info(f"{len(skipped)} more uncovered files not judged this run: "
                         f"{', '.join(skipped[:5])}"))
    for p in judged:
        cands = [r for r in catalog if not _rule_hit(r, p)]
        if len(cands) > COARSE_THRESHOLD:
            cands = coarse_filter(cands, p)
        if not cands:
            continue
        state = {
            "file": p,
            "diff_excerpt": files[p][:MAX_EXCERPT],
            "candidates": [{"id": r["id"], "intent": r["intent"][:160]} for r in cands],
        }
        qs = build_questions(cands, p)
        try:
            payload = post(f"{base}/v1/systemone",
                           {"Authorization": f"Bearer {key}", "x-api-key": key},
                           {"state": _wrap(state), "model": model, "questions": qs},
                           HTTP_TIMEOUT)
        except Exception as e:  # noqa: BLE001 - 单文件调用失败不拖垮整批
            out.append(_info(f"rule_selector: jev call failed for {p} ({e})"))
            continue
        hits = []
        for qid, ans in (payload.get("answers") or {}).items():
            probs = (ans or {}).get("probabilities", {}) or {}
            p_g = min(sum(v for k, v in probs.items() if k == "governs"), 1.0)
            if p_g >= GOVERNS_P:
                hits.append((p_g, qid.removeprefix("rule_").upper()))
        for p_g, rid in sorted(hits, reverse=True)[:MAX_SUGGEST]:
            out.append({
                "id": f"{SELF_ID}-{rid}",
                "severity": "WARN",
                "path": p,
                "line": 0,
                "message": (f"glob 未命中但 jev 判定 {rid} 应管辖此文件, "
                            f"建议把路径补进该规则 match 或写书面驳回 "
                            f"(p(governs)={p_g:.2f})"),
                "confidence": f"{p_g:.2f}",
            })
    return out


def main() -> int:
    root = os.environ.get("GATE_ROOT", ".")
    diff = sys.stdin.read()
    if not diff.strip():
        print("[]")
        return 0
    print(json.dumps(run(root, diff), ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
