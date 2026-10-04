#!/usr/bin/env python3
"""Dioxus/rsx 结构与样式扫描器，被多条 checklist 复用。

一条命令输出三类 finding，调用方用 `--only` 挑：

    nesting    rsx! 块内「元素套元素」的峰值深度
    style      内联 class: 过长 / 硬编码颜色 / 原始色板类
    layout     views 目录里定义 #[component]、component 目录缺失

关于 nesting 的口径——这是全部规则的命门：
`prop: rsx! { ... }`（传 slot）、`if cond { rsx! {} }`（条件渲染）、
`.map(|x| rsx! {})`（列表渲染）都是 Dioxus 惯用法，**不算**元素嵌套。
把它们算进去，规则会天天误报惯用法，agent 学一次就学会无视这条规则。
所以这里用栈逐个花括号判类别，而不是正则数 `Name {` 的行数。

峰值必须在遍历过程中取：块结束时栈已弹平，那时再数永远是 0。
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import asdict, dataclass

# ── 语法识别 ──────────────────────────────────────────────────────
RSX_START = re.compile(r'\brsx!\s*\{')
CONTROL = re.compile(r'^\s*(if|else\s+if|else|for|while|match|loop)\b')
IDENT_TAIL = re.compile(r'([A-Za-z_][A-Za-z0-9_]*)\s*$')

NOT_ELEMENT = {
    'rsx', 'move', 'else', 'if', 'for', 'while', 'match', 'loop',
    'mut', 'ref', 'async', 'unsafe', 'impl', 'fn', 'let', 'use', 'in',
}
HTMLISH = {
    'div', 'span', 'section', 'article', 'header', 'footer', 'nav', 'main',
    'aside', 'ul', 'ol', 'li', 'table', 'form', 'button', 'a', 'p', 'h1',
    'h2', 'h3', 'h4', 'h5', 'h6', 'label', 'img', 'svg', 'details',
    'summary', 'dialog', 'pre', 'code', 'strong', 'em', 'small', 'hr',
    'input', 'select', 'option', 'textarea', 'fieldset', 'tr', 'td', 'th',
    'thead', 'tbody', 'caption', 'iframe', 'video', 'audio', 'canvas',
}


# H3 中文文案（web-spec §B3.4 的判定口径：字符串字面量里出现 CJK）
CJK = re.compile(r'[\u4e00-\u9fff]')
STRING_LIT = re.compile(r'"((?:[^"\\]|\\.)*)"')


def classify_brace(line: str) -> str:
    """判定某个 `{` 是否为「元素开括号」。"""
    if RSX_START.search(line) or CONTROL.match(line):
        return 'other'
    m = IDENT_TAIL.search(line)
    if not m:
        return 'other'
    name = m.group(1)
    if name in NOT_ELEMENT:
        return 'other'
    return 'element' if (name[0].isupper() or name in HTMLISH) else 'other'


@dataclass
class Finding:
    id: str
    severity: str
    path: str
    line: int
    message: str


def peak_nesting(src: str) -> list[tuple[int, int, int]]:
    """每个 rsx! 块的 (峰值元素深度, 起始行, 结束行)。"""
    out = []
    for m in RSX_START.finditer(src):
        start = src.count('\n', 0, m.start()) + 1
        stack = ['root']
        peak = 0
        j = m.end()                      # 已越过 rsx! 的开括号
        while j < len(src) and stack:
            c = src[j]
            if c == '{':
                ls = src.rfind('\n', 0, j) + 1
                stack.append(classify_brace(src[ls:j]))
                peak = max(peak, stack.count('element'))   # 必须在遍历中取
            elif c == '}' and len(stack) > 1:
                stack.pop()
            j += 1
        if peak:
            out.append((peak, start, src.count('\n', 0, j) + 1))
    return out


def scan_nesting(path: str, src: str, limit: int) -> list[Finding]:
    return [
        Finding(
            id="RSX-NESTED-ELEMENT",
            severity="WARN",
            path=path,
            line=start,
            message=(
                f"rsx 块内元素嵌套 {depth} 层（第 {start}-{end} 行）。"
                "Dioxus 里 slot 传片段用 `prop: rsx!{}` 是对的，这里说的是 `div{{div{{div{{}}}}}}` 这种裸元素套元素。"
                "重构：把最内层结构抽成 `#[component] pub fn X() -> Element` 放同文件的 component 区域或 "
                "crate 的 components/ 下，父层只留一次调用 + 传 props。"
            ),
        )
        for depth, start, end in peak_nesting(src)
        if depth >= limit
    ]

# ── style 模式：内联 class 散落 / 硬编码颜色（ui-component-principles §4.1）──
# 三类 finding：
#   DIOXUS-INLINE-CLASS   内联 class 串 ≥ --class-limit 字符 → 样式该抽成常量或用 ui-kit 常量
#   DIOXUS-HARDCODED-COLOR  类串/样式串里 #hex 硬编码颜色
#   DIOXUS-RAW-PALETTE  原始色板类（bg-zinc-800 / text-white / text-red-400 …）绕开语义 token
# 重要取舍：不要求「所有 class 都得是常量」——单个原子类（flex / gap-2）提常量是
# 过度抽象，比散落更难维护。只在成组出现（串过长）时抽。
# 色板类判的是「有没有语义等价物」：text-muted-foreground / bg-card / border-border /
# text-destructive 这类语义类不报；text-zinc-N、text-red-N 等原始色板报。
# 阈值口径（来自 ferrite 实测）：存量仓全仓 1971 条（RAW-PALETTE 1401 集中在正在
# 迁移的 ui-components 副本里）→ 规则是「新代码不许新增散落」（--scope changed），
# 存量债走 merge 全仓审计 + 书面驳回，不在热路径里报。

# 布局/原子 utility 前缀（出现 2 个以上才认为是 class 串，避免误伤散文字符串）
_UTIL_PREFIX = re.compile(
    r'^(?:flex|grid|hidden|block|inline|table|relative|absolute|fixed|sticky|float|'
    r'w|h|size|p|px|py|ps|pe|pt|pb|pl|pr|m|mx|my|ms|me|mt|mb|ml|mr|gap|space|'
    r'rounded|ring|border|shadow|bg|text|font|leading|tracking|uppercase|lowercase|'
    r'items|justify|self|content|place|object|overflow|transition|duration|delay|ease|'
    r'z|top|right|bottom|left|inset|opacity|cursor|outline|select|appearance|touch|'
    r'scale|rotate|translate|skew|origin|min|max|aspect|line-clamp|col|row|order)'
)

# 原始色板类：修饰语 - 色族（-shade）(/opacity)。语义色（-foreground/-muted-…）不在此列。
_RAW_PALETTE = re.compile(
    r'\b(?:text|bg|border|ring|from|to|via|fill|stroke|outline|decoration|divide|'
    r'accent|caret|placeholder|selection|shadow)'
    r'-(?:zinc|slate|neutral|stone|gray|red|orange|amber|yellow|lime|green|emerald|'
    r'teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose|white|black)'
    r'(?:-\d{1,3})?(/\d{1,3})?(?![a-z])'
)
_HEX = re.compile(r'#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})\b')


def _looks_like_class_list(value: str) -> bool:
    toks = value.split()
    hits = sum(1 for t in toks if _UTIL_PREFIX.match(t))
    return len(toks) >= 2 and hits >= 2


def scan_style(path: str, src: str, class_limit: int) -> list[Finding]:
    out: list[Finding] = []
    for lineno, line in enumerate(src.split('\n'), 1):
        stripped = line.lstrip()
        if stripped.startswith(('//', '*')):
            continue                      # 注释/文档里的字符串不算
        for value in STRING_LIT.findall(line):
            if not _looks_like_class_list(value):
                continue
            if len(value) >= class_limit:
                out.append(Finding(
                    id="DIOXUS-INLINE-CLASS",
                    severity="WARN",
                    path=path, line=lineno,
                    message=(
                        f"内联 class 串 {len(value)} 字符（≥{class_limit}）：成组样式散落在 rsx 里，"
                        "换主题要逐处改。重构：抽成命名常量（或引用 ui-kit styles.rs 的语义常量），"
                        "rsx 里只留一次引用。单个原子类（flex / gap-2）不要提常量——过度抽象。"
                    ),
                ))
            for m in _RAW_PALETTE.finditer(value):
                out.append(Finding(
                    id="DIOXUS-RAW-PALETTE",
                    severity="WARN",
                    path=path, line=lineno,
                    message=(
                        f"原始色板类 {m.group(0)} 绕开语义 token（ui-component-principles §4.1"
                        " token 唯一来源）。重构：换成语义类（text-muted-foreground / bg-card / "
                        "border-border / text-destructive …），色板值只住在 token 层（theme.css）。"
                    ),
                ))
            for m in _HEX.finditer(value):
                out.append(Finding(
                    id="DIOXUS-HARDCODED-COLOR",
                    severity="WARN",
                    path=path, line=lineno,
                    message=(
                        f"硬编码颜色 {m.group(0)}：未走设计 token，换主题会失效。"
                        "重构：值进 token 层（theme.css 变量），类上引用语义类。"
                    ),
                ))
    return out


def scan_spec(path: str, src: str) -> list[Finding]:
    """按 web-spec **原文阈值**判，不用自造数字。

    来源：ferrite `todo/web-refract/web-spec.md`
      R1.3 §B2.4  一个 #[component] 函数体至多 1 个内容 rsx（槽位接线除外）
      H3   §B3    rsx 元素体内中文为 0，范围 = tab-page/ + components/
    色值（B8 semantic_style / H1 零漂移）**不在这里判**：规范明写「抽组件时不改
    色值」「把 border-zinc-800 顺手改 border-border 违反 H1」，色值迁移必须单独
    开 PR —— 那是 web_refactor 的 semantic_style 问句的职责，不该由 l1 越权指挥。
    """
    norm = path.replace('\\', '/')
    if '/tab-page/' not in norm and '/components/' not in norm:
        return []

    findings: list[Finding] = []

    n_rsx, n_comp = src.count('rsx!'), src.count('#[component]')
    if n_rsx > n_comp:
        findings.append(Finding(
            id="WEB-SINGLE-RSX",
            severity="WARN",
            path=path, line=1,
            message=(
                f"rsx! 出现 {n_rsx} 次但 #[component] 只有 {n_comp} 个 —— 违反 R1.3 单 rsx 纪律"
                "（web-spec §B2.4：一个 #[component] 函数体至多 1 个内容 rsx）。"
                "重构：把 `let x = rsx!{…}` 这类第二个内容 rsx 抽成子组件函数再组合；"
                "只有 `Option<Element>` 槽值位置的 rsx 属框架管线例外，不计入。"
            ),
        ))

    for lineno, line in enumerate(src.split('\n'), 1):
        if line.lstrip().startswith('//'):
            continue                       # §B3 明写：注释里的中文不算
        if 'data-testid' in line:
            continue                       # §B3.3 例外：testid 值不抽
        for value in STRING_LIT.findall(line):
            if not CJK.search(value):
                continue
            findings.append(Finding(
                id="WEB-I18N-CONSTANT",
                severity="WARN",
                path=path, line=lineno,
                message=(
                    f"rsx 元素体里出现中文串：\"{value[:40]}\" —— 违反 H3（web-spec §B3 文案常量化）。"
                    "重构：提成带前缀的常量（BTN_/LBL_/SEC_/MSG_/FIELD_/OPT_）放 crate 根级 shared.rs "
                    "或组件文件头；**常量值一个字符都不许改**（H1 零行为漂移）。"
                ),
            ))
    return findings

def scan_layering(path: str, src: str) -> list[Finding]:
    """层级约束的可判定信号是 **import 方向**，不是「组件放在哪个目录」。

    实测（kymido）：`views/config.rs` 里放 6 个 `#[component]` 是他们刻意的做法
    ——「一文件一 pub 组件 + 私有子组件同文件」。按目录一刀切会误报 6 处。
    反过来 `components/` 不得 `use crate::views::` 在现状是 0 违例，是干净硬约束。
    """
    norm = path.replace('\\', '/')
    if not re.search(r'/(components|layouts|utils)/', norm):
        return []
    for lineno, line in enumerate(src.split('\n'), 1):
        if re.search(r'\buse\s+(crate|super)::(views)::', line):
            return [Finding(
                id="WEB-LAYERING-DEPENDENCY",
                severity="FAIL",
                path=path, line=lineno,
                message=(
                    f"{norm.rsplit('/', 2)[-2]}/ 反向依赖了 views/。"
                    "层级是单向的：views → components/layouts/utils，反向不允许。"
                    "重构：把它需要的页面级状态改成 props 传入（`#[component] pub fn X(#[prop(into)] ...)`），"
                    "或把真正共用的部分下沉到 components/ 再由两边各自引用。"
                ),
            )]
    return []


def tracked_rs_files(root: str) -> list[str]:
    """用 git ls-files 取扫描集：gitignore 感知，不会扫进 target/ 与 .wt/。"""
    out = subprocess.run(
        ['git', 'ls-files', '-z', '--', '*.rs'],
        cwd=root, capture_output=True, check=False,
    )
    if not out.returncode:
        return [p for p in out.stdout.decode('utf-8', 'replace').split('\0') if p]
    import pathlib
    return [str(p) for p in pathlib.Path(root).rglob('*.rs')
            if 'target' not in p.parts and '.wt' not in p.parts]


def changed_rs_files(root: str, base: str) -> list[str]:
    """基线以来改动过的 .rs。

    存量仓（ferrite 2222 条）必须靠这个收窄到「你这次动过的文件」，
    否则一条规则一次报几千条 WARN，agent 的唯一理性反应就是忽略它——
    等于规则不存在，还多了一个假绿来源。
    """
    for spec in (f'{base}...HEAD', f'{base}..HEAD', base):
        out = subprocess.run(
            ['git', 'diff', '--name-only', '-z', spec, '--', '*.rs'],
            cwd=root, capture_output=True, check=False,
        )
        if out.returncode == 0:
            return [p for p in out.stdout.decode('utf-8', 'replace').split('\0') if p]
    return []


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--max', type=int, default=40,
                    help='单次最多输出多少条 finding（防止刷屏）；统计真实量级时传 0')
    ap.add_argument('--only', choices=['nesting', 'spec', 'layering', 'style'], default=None)
    ap.add_argument('--class-limit', type=int, default=72,
                    help='style: 内联 class 串超过该字符数报 DIOXUS-INLINE-CLASS')
    ap.add_argument('--nesting-limit', type=int, default=2,
                    help='R1 要求 tab-page/ 元素嵌套 ≤1 层；默认 2 = 报「超过 1 层」')
    ap.add_argument('--scope', choices=['repo', 'changed'], default='repo',
                    help='repo=全仓审计(CI/merge 用)；changed=只看基线以来的改动(hook 热路径用)')
    ap.add_argument('--base', default=None,
                    help='changed 范围的基线 rev；缺省取 GATE_BASE，再缺省 origin/main')
    ap.add_argument('--root', default=None)
    args = ap.parse_args()

    root = args.root or subprocess.run(
        ['git', 'rev-parse', '--show-toplevel'],
        capture_output=True, text=True, check=False,
    ).stdout.strip() or '.'

    if args.scope == 'changed':
        base = args.base or os.environ.get('GATE_BASE') or 'origin/main'
        targets = changed_rs_files(root, base)
    else:
        targets = tracked_rs_files(root)

    findings: list[Finding] = []
    for rel in targets:
        try:
            with open(f'{root}/{rel}', encoding='utf-8', errors='replace') as fh:
                src = fh.read()
        except OSError:
            continue
        # 判据是**内容**不是路径名：按目录名过滤会让 web crate 一改名就静默失效，
        # 表现为「规则没报=通过」的假绿。带 rsx! 的文件才是 web UI 代码。
        if 'rsx!' not in src and '/views/' not in rel.replace('\\', '/'):
            continue
        if args.only in (None, 'nesting'):
            findings += scan_nesting(rel, src, args.nesting_limit)
        if args.only in (None, 'spec'):
            findings += scan_spec(rel, src)
        if args.only in (None, 'layering'):
            findings += scan_layering(rel, src)
        if args.only in (None, 'style'):
            findings += scan_style(rel, src, args.class_limit)

    findings.sort(key=lambda f: (f.path, f.line))
    total = len(findings)
    if args.max:
        findings = findings[:args.max]
    print(json.dumps([asdict(f) for f in findings], ensure_ascii=False))
    if not args.max:
        print(f'total={total}', file=sys.stderr)
    return 0

if __name__ == '__main__':
    sys.exit(main())
