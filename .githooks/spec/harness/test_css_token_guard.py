#!/usr/bin/env python3
"""css_token_guard.py 的判定语义回归测试（无依赖，CI 直接 `python3` 跑）。

样张挑的是**真实会误报/漏报的形态**，不是装饰性断言：

  - @apply 是规则体内唯一的 at-声明，曾被「at-rule 语句不判」一并吞掉（漏报整类）；
  - `.dark, [data-theme="dark"]` 这类多选择器 / 组件局部块里的自定义属性，选择器
    形态判不住，只能按「自定义属性行永远豁免」处理（误报 34 处 theme token）；
  - 带 alpha 的 black/white 叠加层与渐变是配色本身，不是主题槽（误报会把 kit 的
    scrim/shade 全点红，规则随即被当噪声忽略）。

不依赖被测仓布局：在 tmpdir 里 `git init` 一个样张仓，用真进程跑两种模式。
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

HARNESS = Path(__file__).with_name("css_token_guard.py")

# 每条 (文件, 内容)。断言按 (id, 文件, 行号) 集合对账，行号即样张里的物理行。
CSS_FILES = {
    "assets/kit.css": """\
@import "reset.css";
@theme {
  --color-a: #ff0000;
}
@layer tokens {
  :root { --y: #000000; }
}
:root { --x: #23252a; }
.a { @apply border border-white; }
.b { @apply text-zinc-500; }
.c { @apply border-border-subtle; }
.d,
[data-theme="dark"] {
  --tok: #123456;
  color: #111111;
}
.e { background: #ffffff; }
.f { @apply bg-black/60; }
.g { @apply bg-gradient-to-b from-black/85 to-zinc-900; }
.h { background: linear-gradient(#ffffff, #000000); }
.i { color: #ffffff; /* guard:allow 三方快照原样 */ }
@utility u-x {
  @apply bg-red-500;
}
@layer components {
  .j {
    @apply bg-black;
  }
}
@media (min-width: 40rem) {
  .k { color: #333333; }
}
""",
    # 名单命中 → 整文件不判
    "assets/samples.css": ".s { color: #123456; }\n",
    # .min. 无条件跳过
    "assets/vendor.min.css": ".m { color: #123456; }\n",
}
CSS_FLAGS = {
    ("CSS-RAW-PALETTE-APPLY", "assets/kit.css", 9),    # 不带 alpha 的 border-white
    ("CSS-RAW-PALETTE-APPLY", "assets/kit.css", 10),   # text-zinc-500
    ("CSS-HARDCODED-HEX", "assets/kit.css", 15),       # 多选择器块的普通属性值
    ("CSS-HARDCODED-HEX", "assets/kit.css", 17),       # 规则体裸 hex
    ("CSS-RAW-PALETTE-APPLY", "assets/kit.css", 27),   # @layer components 继承外层
    ("CSS-HARDCODED-HEX", "assets/kit.css", 31),       # @media 继承外层
}
SKIP_LIST = "assets/samples.css\n"

RSX_FILES = {
    "src/lib.rs": """\
use dioxus::prelude::*;

#[component]
pub fn Card() -> Element {
    rsx! {
        div { class: "ui-card role-label",
            span { class: "ui-card-title", "t" }
            div { class: "ui-body bg-zinc-800", "b" }
            div { class: "row-text", "r" }
            div { class: "{dynamic}", "d" }
            div { class: "data-[state=open]:opacity-100", "s" }
        }
    }
}
""",
}
RSX_ALLOW = "row-text\n"


def make_repo(files: dict[str, str], extra: dict[str, str] | None = None) -> str:
    tmp = tempfile.mkdtemp(prefix="css-guard-")
    repo = Path(tmp)
    for rel, body in {**files, **(extra or {})}.items():
        p = repo / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body, encoding="utf-8")
    for cmd in (["init", "-q"], ["-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"],
                ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "fixture"]):
        subprocess.run(["git", "-C", tmp, *cmd], check=True, capture_output=True)
    return tmp


def run(repo: str, mode: str, extra: list[str] | None = None) -> tuple[list[dict], dict]:
    proc = subprocess.run(
        [sys.executable, str(HARNESS), "--mode", mode, *(extra or [])],
        cwd=repo, capture_output=True, text=True, check=True)
    assert "stats: " in proc.stderr, \
        f"语料/判定项计数必须走 stderr，让「绿」与「查过」可区分：{proc.stderr!r}"
    stats = json.loads(proc.stderr.split("stats: ", 1)[1].strip())
    assert stats["corpus_files"] >= 0
    return json.loads(proc.stdout), stats


def key(findings: list[dict]) -> set[tuple[str, str, int]]:
    return {(f["id"], f["path"], f["line"]) for f in findings}


def check(label: str, got: object, want: object) -> int:
    if got == want:
        print(f"  ok  {label}")
        return 0
    print(f"FAIL  {label}\n  缺少（该报没报）: {sorted(want - got)}\n"
          f"  多余（不该报）: {sorted(got - want)}")
    return 1


def main() -> int:
    failures = 0

    css_repo = make_repo(CSS_FILES, {".githooks/spec/custom/css_guard_skip.txt": SKIP_LIST})
    findings, stats = run(css_repo, "css")
    failures += check("css: 命中集 == 样张该报的形态", key(findings), CSS_FLAGS)
    guarded = {(f["path"], f["line"]) for f in findings
               if "guard:allow" in (Path(css_repo) / f["path"]).read_text(encoding="utf-8")
               .splitlines()[f["line"] - 1]}
    failures += check("css: guard:allow 行不出现在结果里", guarded, set())
    # 上一条是「没有」型断言，靠这条钉住它不是空转：样张那行确实被看到了并计了豁免。
    failures += check("css: 豁免行确实被识别并计数", stats["guard_allowed"], 1)
    skipped = {f["path"] for f in findings}
    failures += check("css: skip 名单与 .min. 不得出现在结果里",
                      {p for p in skipped if "samples" in p or "vendor" in p}, set())

    # 名单缺失 = 一个都不跳过（宁可多报，绝不静默放行）
    no_skip = make_repo(CSS_FILES)
    hits, _ = run(no_skip, "css")
    failures += check("css: 缺 skip 名单时样张文件也要判",
                      {"assets/samples.css"} & {f["path"] for f in hits},
                      {"assets/samples.css"})

    # 无 CSS 语料 → 显式 NO-CORPUS，而不是静默 []
    empty = make_repo({"README.md": "no css here\n"})
    failures += check("css: 语料为空必须自报 NO-CORPUS",
                      key(run(empty, "css")[0]),
                      {("CSS-GUARD-NO-CORPUS", ".", 0)})

    rsx_repo = make_repo(RSX_FILES, {".githooks/spec/custom/css_token_allowlist.txt": RSX_ALLOW})
    failures += check("rsx: 只拦裸 Tailwind utility",
                      {(f["id"], f["path"], f["line"]) for f in run(rsx_repo, "rsx")[0]},
                      {("RSX-RAW-UTILITY", "src/lib.rs", 8)})
    failures += check("rsx: 无 rsx! 必须自报 NO-CORPUS",
                      key(run(empty, "rsx")[0]), {("RSX-NO-CORPUS", ".", 0)})

    print("PASS" if failures == 0 else f"{failures} 组失败")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
