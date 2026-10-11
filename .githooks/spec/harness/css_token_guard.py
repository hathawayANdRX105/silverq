#!/usr/bin/env python3
"""css_token_guard.py — dioxus/css 语义 token 两支柱检查（l1 确定性，零 LLM）。

两个模式，**语料全靠扫描发现，不写死任何仓库布局**（gate 可移植标准：写死
`crates/*/…` 会让换仓时「扫到 0 文件」伪装成绿）：

  mode=rsx   rsx 的 `class: "…"` 字面量只许语义类（ui-* / role-* / 项目登记类 /
             data-* 标记 / 动态插值）。适用面判据 = 仓里存在含 `rsx!` 的跟踪 .rs。
  mode=css   手写 CSS 的规则体禁止 @apply 原始色板类与裸 #hex；色值必须走
             var(--*) 语义 token 或语义类。适用面判据 = 仓里存在非生成物的跟踪 CSS。

用法（checklist yaml 的 harness）:
  python3 .githooks/spec/harness/css_token_guard.py --mode rsx \
      --allow-file .githooks/spec/custom/css_token_allowlist.txt
  python3 .githooks/spec/harness/css_token_guard.py --mode css

输出: findings JSON 数组（stdout），退出码恒 0（严重度由 yaml fail_severity 定）。
一行统计走 stderr（语料数 / 判定项数），让「绿」与「查过」可区分；--stats
只输出该统计，供人核对口径确实覆盖到了文件。

豁免口径（命中即跳过，全部可审计）:
  1. 生成物 / 厂商快照整文件跳过：--skip-file（默认
     .githooks/spec/custom/css_guard_skip.txt）一行一个 fnmatch glob，匹配仓库
     相对路径，# 注释；.min. 与 .map 无条件跳过。名单是项目侧资产，
     canon-sync 不覆盖 custom/。
  2. 行级豁免：行内注释含 guard:allow 且其后写了理由。
  3. 自定义属性行永远豁免：--x: #23252a; 就是色值的住处，无论它住在 :root、
     `.dark, [data-theme=…]` 还是组件局部的 .theme-sample-x 块里；@theme 与
     @layer tokens 整块豁免。普通属性值里的裸 hex 照判——body { background:
     #fff; } 正是本规则要拦的东西。
  4. @utility 段（Tailwind v4 utility 定义）默认不判，沿用 v1 口径；
     --judge-utility 打开它，用于清账摸底。
  5. 合成语境不判：渐变（gradient）里的 hex 是多停靠点配色本身；带 alpha 的
     black/white（bg-black/60、border-white/15）是跨预设的叠加/压暗手段，不是主题槽。

语料为空时**不静默**：输出 *-NO-CORPUS 的 INFO finding，声明本仓不在该规则适用
面内——绿不等于查过。
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import re
import subprocess
import sys

# --------------------------------------------------------------- 形态判定
# Tailwind 已知 utility 语法（形式判定，非白名单）。刻意保守：只拦「确定是
# Tailwind utility」的形态，宁可漏报不可误报（gate 下界原则）。
TAILWIND_TOKEN = re.compile(
    r"^(?:(?:hover|focus|focus-visible|active|disabled|group-hover|peer-(?:checked|hover)|"
    r"dark|sm|md|lg|xl|2xl|first|last|odd|even|visited|checked|selection|marker|file|"
    r"placeholder|before|after|data-\[[^\]]+\]|aria-\[[^\]]+\]):)*"
    r"(?:[a-z][a-z0-9-]*(?:\[[^\]]+\]|/\d{1,3})?|-\[[^\]]+\])$"
)
KNOWN_UTILITY_ROOTS = re.compile(
    r"^(container|sr-only|not-sr-only|flex|inline-flex|grid|inline-grid|block|inline|hidden|"
    r"table|contents|isolation|absolute|relative|fixed|sticky|static|inset|top|right|bottom|left|"
    r"z|order|col|row|float|clear|object|overflow|overscroll|truncate|basis|grow|shrink|"
    r"border|rounded|shadow|ring|opacity|mix-blend|bg|from|via|to|filter|blur|brightness|"
    r"contrast|grayscale|hue-rotate|invert|saturate|sepia|backdrop|transition|duration|ease|"
    r"delay|animate|scale|rotate|translate|skew|origin|accent|appearance|cursor|caret|"
    r"resize|scroll|snap|touch|select|will-change|fill|stroke|outline|pointer-events|"
    r"visible|invisible|collapse|whitespace|break|text|font|tracking|leading|list|"
    r"decoration|underline|uppercase|lowercase|capitalize|normal-case|italic|not-italic|"
    r"antialiased|tabular-nums|align|justify|items|content|self|place|gap|space|divide|"
    r"p|px|py|pt|pb|pl|pr|m|mx|my|mt|mb|ml|mr|w|h|min-w|min-h|max-w|max-h|size|aspect|"
    r"columns|line-through)"
    r"(?:-|$)"
)
# 只留跨仓通用的语义前缀；项目自有物理类名一律走 --allow-file，不写死在这里。
GENERIC_SEMANTIC = re.compile(r"^(ui-|role-|cva-)")
STATE_ATTR = re.compile(r"^(data-|aria-|id$|for$|name$|type$|value$|href$|src$|alt$|style$|testid)")
RSX_CLASS_RE = re.compile(r'class:\s*"([^"]+)"')

HEX_COLOR_RE = re.compile(r"#[0-9a-fA-F]{3,8}\b")
PALETTE_ROOTS = (r"(?:bg|text|border|ring|fill|stroke|outline|decoration|shadow|divide|"
                 r"placeholder|accent|caret|from|via|to)")
PALETTE_APPLY_RE = re.compile(
    r"@apply[^;]*\b" + PALETTE_ROOTS +
    r"-(?:zinc|slate|gray|grey|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|"
    r"cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose)-\d{2,3}\b")
# 不带 alpha 的原始 white/black（border-white / bg-black）与带梯度的色板类同一毛病：
# 主题切换不传播（ferrite #101 的亮色「白底白框」即此形）。带 alpha 的
# `bg-black/60`、`border-white/15` 不判——那是跨预设都成立的叠加/压暗合成手段
# （scrim、shade、描边），不是主题槽；渐变语境同理。
RAW_NAMED_APPLY_RE = re.compile(r"@apply[^;]*\b" + PALETTE_ROOTS + r"-(?:white|black)\b")
NAMED_ALPHA_RE = re.compile(r"\b" + PALETTE_ROOTS + r"-(?:white|black)/\d")
CUSTOM_PROP_RE = re.compile(r"^\s*--[\w-]+\s*:")
GUARD_ALLOW_RE = re.compile(r"guard:allow\s+\S")


def git_ls_files(root: str, patterns: list[str]) -> list[str]:
    out: list[str] = []
    for pat in patterns:
        r = subprocess.run(["git", "ls-files", "--", pat], cwd=root,
                           capture_output=True, text=True, check=False)
        out.extend(line for line in r.stdout.splitlines() if line.strip())
    return sorted(set(out))


def read_text(path: str) -> str | None:
    try:
        with open(path, encoding="utf-8") as fh:
            return fh.read()
    except (OSError, UnicodeDecodeError):
        return None


def load_tokens(root: str, path: str | None) -> set[str]:
    src = read_text(f"{root}/{path}") if path else None
    if src is None:
        return set()
    return {line.strip() for line in src.splitlines()
            if line.strip() and not line.startswith("#")}


def is_skipped(rel: str, globs: list[str]) -> bool:
    if ".min." in rel or rel.endswith(".map"):
        return True
    return any(fnmatch.fnmatch(rel, pat) for pat in globs)


# ------------------------------------------------------------------ rsx 模式
def is_exempt_token(tok: str, allow: set[str]) -> bool:
    if tok in allow or STATE_ATTR.match(tok) or GENERIC_SEMANTIC.match(tok):
        return True
    if "{" in tok or "}" in tok:  # 动态插值
        return True
    m = TAILWIND_TOKEN.match(tok)
    if not m:
        return True  # 不是 Tailwind 形态（自定义物理类）→ 本规则不判
    bare = m.group(0).split(":")[-1].split("/")[0].split("[")[0]
    return not KNOWN_UTILITY_ROOTS.match(bare + "-")


def scan_rsx(root: str, findings: list[dict], allow: set[str], stats: dict) -> None:
    hits: list[tuple[str, str]] = []
    for rel in git_ls_files(root, ["*.rs"]):
        if rel.startswith(("target/", ".wt/")):
            continue
        src = read_text(f"{root}/{rel}")
        if src and "rsx!" in src:
            hits.append((rel, src))
    stats["corpus_files"] = len(hits)
    if not hits:
        findings.append({
            "id": "RSX-NO-CORPUS", "severity": "INFO", "path": ".", "line": 0,
            "message": "跟踪 .rs 里没有 `rsx!`：本仓不在 dioxus rsx 语义类契约的适用面内，"
                       "本规则这次绿不代表查过。",
        })
        return
    for rel, src in hits:
        for lineno, line in enumerate(src.splitlines(), 1):
            for m in RSX_CLASS_RE.finditer(line):
                stats["judged"] += 1
                for tok in m.group(1).split():
                    if is_exempt_token(tok, allow):
                        continue
                    findings.append({
                        "id": "RSX-RAW-UTILITY", "severity": "WARN", "path": rel, "line": lineno,
                        "message": f"rsx class 字面量引用裸 Tailwind utility「{tok}」：样式真身必须"
                                   f"住 kit 资产 CSS 的语义配方（ui-*），rsx 只发射语义类 + "
                                   f"data-* 状态。确认是文档化覆盖槽/误报时写进 --allow-file "
                                   f"名单并注明理由。",
                    })


# ------------------------------------------------------------------ css 模式
def push_context(pending: str, stack: list[str], judge_utility: bool) -> None:
    """选择器/前导文 → 一层语境。包装类 at-rule 用 pass 继承父级。"""
    sel = " ".join(pending.split())
    if sel.startswith(("@theme", "@layer tokens")):
        stack.append("token")
    elif sel.startswith(("@media", "@supports", "@container", "@scope", "@layer")):
        stack.append("pass")
    elif sel.startswith("@utility"):
        stack.append("judge" if judge_utility else "utility")
    else:
        stack.append("judge")


def effective_context(stack: list[str]) -> str:
    for ctx in reversed(stack):
        if ctx != "pass":
            return ctx
    return "outside"


def strip_block_comments(line: str, open_comment: bool) -> tuple[str, bool]:
    """跨行注释就地抹成空格（保留列位与行数），返回 (代码, 是否仍在注释内)。"""
    out, i, in_c = [], 0, open_comment
    while i < len(line):
        if in_c:
            end = line.find("*/", i)
            if end < 0:
                i = len(line)
            else:
                i = end + 2
                in_c = False
            out.append(" ")
            continue
        start = line.find("/*", i)
        if start < 0:
            out.append(line[i:])
            break
        out.append(line[i:start])
        out.append(" " * 2)
        i = start + 2
        in_c = True
    return "".join(out), in_c


def judge_declaration(ctx: str, code: str) -> bool:
    """该声明是否属于「色值必须走 token」的判定面。"""
    if ctx != "judge":
        return False
    # 自定义属性行永远豁免：`--border: #23252a;` 就是色值的住处，无论它住在 :root、
    # `.dark, [data-theme="dark"]` 还是组件局部的 `.theme-sample-x` 块里。
    return not CUSTOM_PROP_RE.match(code)


def emit_declaration(decl: str, lineno: int, allowed: bool, stack: list[str], rel: str,
                     findings: list[dict], stats: dict) -> None:
    """声明边界（`;` 或 `}`）到达时判定一条声明；语境取当前块栈。"""
    decl = " ".join(decl.split())
    # `@apply` 是规则体内唯一的 at-声明，必须判；其余 `@import` 之类语句不判。
    if not decl or (decl.startswith("@") and not decl.startswith("@apply")):
        return
    ctx = effective_context(stack)
    if ctx == "outside":
        return
    stats["judged"] += 1
    if allowed:
        stats["guard_allowed"] += 1
        return
    if not judge_declaration(ctx, decl):
        return
    # 渐变与叠加语境：多停靠点配色本身就是内容（海报压暗、头像底板渐变…），
    # 不是主题槽，和 hex 的渐变豁免同一口径。
    composite = "gradient" in decl
    raw_palette = bool(PALETTE_APPLY_RE.search(decl)) and not composite
    raw_named = (bool(RAW_NAMED_APPLY_RE.search(decl)) and not composite
                 and not NAMED_ALPHA_RE.search(decl))
    if raw_palette or raw_named:
        findings.append({
            "id": "CSS-RAW-PALETTE-APPLY", "severity": "WARN", "path": rel, "line": lineno,
            "message": "@apply 引用原始色板类（zinc/emerald/…-NNN 或不带 alpha 的 "
                       "white/black）：主题换色不会传播，必须换语义 token"
                       "（var(--*) / border-border-* / text-foreground …）。带 alpha 的 "
                       "black/white 叠加层与渐变语境本就不判；确需保留原样时在行尾加 "
                       "`/* guard:allow 理由 */`，整文件性质（生成物 / 样张）写进 "
                       ".githooks/spec/custom/css_guard_skip.txt。",
        })
    if HEX_COLOR_RE.search(decl) and "var(" not in decl and not composite:
        findings.append({
            "id": "CSS-HARDCODED-HEX", "severity": "WARN", "path": rel, "line": lineno,
            "message": "规则体普通属性值里裸 #hex：色值必须绑定 theme token（var(--*)）；"
                       "自定义属性行（--x: #hex）不受此限。确属生成物/样张的整文件例外"
                       "走 css_guard_skip.txt，个别行用 `/* guard:allow 理由 */`。",
        })


def scan_css(root: str, findings: list[dict], skip_globs: list[str], judge_utility: bool,
             stats: dict) -> None:
    corpus: list[tuple[str, str]] = []
    for rel in git_ls_files(root, ["*.css"]):
        if is_skipped(rel, skip_globs):
            stats["skipped_files"] += 1
            continue
        src = read_text(f"{root}/{rel}")
        if src is not None:
            corpus.append((rel, src))
    stats["corpus_files"] = len(corpus)
    if not corpus:
        findings.append({
            "id": "CSS-GUARD-NO-CORPUS", "severity": "INFO", "path": ".", "line": 0,
            "message": "跟踪 CSS（剔除生成物/跳行名单后）为空：本仓不在 CSS 语义 token "
                       "组合契约的适用面内，本规则这次绿不代表查过。",
        })
        return

    for rel, src in corpus:
        stack: list[str] = []
        pending = ""
        in_comment = False

        for lineno, raw in enumerate(src.splitlines(), 1):
            code, in_comment = strip_block_comments(raw, in_comment)
            allowed = bool(GUARD_ALLOW_RE.search(raw))
            buf = ""
            for ch in code:
                if ch == "{":
                    push_context(pending + buf, stack, judge_utility)
                    pending, buf = "", ""
                elif ch == "}":
                    emit_declaration(buf, lineno, allowed, stack, rel, findings, stats)
                    if stack:
                        stack.pop()
                    pending, buf = "", ""
                elif ch == ";":
                    emit_declaration(pending + buf, lineno, allowed, stack, rel,
                                     findings, stats)
                    pending, buf = "", ""
                else:
                    buf += ch
            pending += buf  # 选择器可跨行；未闭合声明留待下一行的边界


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["rsx", "css"], required=True)
    ap.add_argument("--allow-file", default=".githooks/spec/custom/css_token_allowlist.txt",
                    help="仓根相对路径：rsx 模式逐 token 豁免名单（# 注释）")
    ap.add_argument("--skip-file", default=".githooks/spec/custom/css_guard_skip.txt",
                    help="仓根相对路径：整文件跳过名单（生成物/厂商快照，一行一个 glob）")
    ap.add_argument("--judge-utility", action="store_true",
                    help="连 @utility 段一起判（默认沿用 v1 口径不判）")
    ap.add_argument("--stats", action="store_true", help="只输出语料/判定统计 JSON")
    args = ap.parse_args()

    root = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                          capture_output=True, text=True, check=False).stdout.strip()
    if not root:
        print("[]")
        return 0

    stats = {"mode": args.mode, "corpus_files": 0, "judged": 0,
             "guard_allowed": 0, "skipped_files": 0}
    findings: list[dict] = []
    if args.mode == "rsx":
        scan_rsx(root, findings, load_tokens(root, args.allow_file), stats)
    else:
        scan_css(root, findings, sorted(load_tokens(root, args.skip_file)),
                 args.judge_utility, stats)

    if args.stats:
        print(json.dumps(stats, ensure_ascii=False))
        return 0
    print(json.dumps(findings, ensure_ascii=False))
    print("css_token_guard stats: " + json.dumps(stats, ensure_ascii=False), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
