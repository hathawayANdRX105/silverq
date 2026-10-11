#!/usr/bin/env python3
"""`dioxus_web.py` nesting 模式（CK-WEB-RSX-NESTING / R1）的回归测试。

钉住三件失效形态——它们的共同点是**闸门自己坏掉时照样输出合法 JSON**，
「绿」不代表「查过」：

  1. **阈值口径**：`--nesting-limit 1` = 允许 1 层嵌套。检查清单里写死了
     「peak 元素深度 >= 3 才报（limit+2 偏移）」，脚本若退回 `>= limit`，
     顶层元素的正常混排（`div{class,span,button}`）会被整片报成违规。
  2. **块边界**：`rsx!` 块的收尾花括号必须把栈弹平。栈不弹平的失效形态是
     一个块一路吃到文件尾（第 25-502 行那种），于是每行都被算进同一个块。
  3. **范围**：`demo/` 是视觉回归 fixture，不进 R1 纪律（jev 裁定 12/12 误报）。
     排除失效 = demo 页面的存量债混进生产代码的热路径。

脚本本体无扩展名依赖，用 importlib 直接加载真身，不复制逻辑；范围那条走 CLI
（排除逻辑在主循环里，单元级测不到）。
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
from dataclasses import asdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("dioxus_web", HERE / "dioxus_web.py")
dioxus_web = importlib.util.module_from_spec(spec)
sys.modules["dioxus_web"] = dioxus_web
spec.loader.exec_module(dioxus_web)

# 块 1 = section>div>h2，真 2 层嵌套（peak=3）；块 2 只有一个顶层 p（peak=1）。
# 行号在断言里写死，改 fixture 必须一起改——块边界正是本测试要钉的东西。
TWO_BLOCKS = '''pub fn a() -> Element {
    rsx! {
        section {
            div { class: "x",
                h2 { "t" }
            }
        }
    }
}

pub fn b() -> Element {
    rsx! {
        p { "solo" }
    }
}
'''

# 顶层 div 里混排属性与两个叶子元素 = 1 层嵌套（peak=2），R1 允许。
ONE_LEVEL = '''rsx! {
    div { class: "a",
        span { "1" }
        button { "2" }
    }
}
'''

# slot 片段：`footer: rsx! {` 自身的开括号不是元素，里面的 span 才占一层。
SLOT_FRAGMENT = '''rsx! {
    div { class: "a",
        footer: rsx! {
            span { "x" }
        }
    }
}
'''


def check(label: str, got: object, want: object) -> int:
    if got == want:
        print(f"  ok  {label}")
        return 0
    print(f"FAIL  {label}\n  got : {got}\n  want: {want}")
    return 1


def git_repo(tmp: Path, files: dict[str, str]) -> str:
    for rel, body in files.items():
        p = tmp / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body, encoding="utf-8")
    for cmd in (["init", "-q"], ["-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"],
                ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "f"]):
        subprocess.run(["git", "-C", str(tmp), *cmd], check=True, capture_output=True)
    return str(tmp)


def nest(rel: str, src: str, limit: int = 1) -> list[dict]:
    return [asdict(f) for f in dioxus_web.scan_nesting(rel, src, limit)]


def main() -> int:
    failures = 0

    # 1. 峰值与块边界：两个块各自成段，第二个块不许并进第一个。
    failures += check(
        "逐块峰值与收尾行（栈弹平，不吃到文件尾）",
        dioxus_web.peak_nesting(TWO_BLOCKS),
        [(3, 2, 8), (1, 12, 14)],
    )

    # 2. 阈值：limit=1 只报 >1 层嵌套。
    failures += check("2 层嵌套必报", len(nest("src/a.rs", TWO_BLOCKS)), 1)
    failures += check("报的是超限那块而不是全文件",
                      [f["line"] for f in nest("src/a.rs", TWO_BLOCKS)], [2])
    failures += check("1 层嵌套（顶层混排）不报", nest("src/a.rs", ONE_LEVEL), [])
    failures += check("slot 片段的开括号不计入元素深度",
                      nest("src/a.rs", SLOT_FRAGMENT), [])

    # 3. demo/ 不在纪律范围；同内容的生产路径必须照报（防排除条件写宽成整体失效）。
    root = git_repo(Path(tempfile.mkdtemp(prefix="dioxus-web-")) / "repo",
                    {"demo/preview.rs": TWO_BLOCKS, "src/chat.rs": TWO_BLOCKS})
    r = subprocess.run([sys.executable, str(HERE / "dioxus_web.py"),
                        "--only", "nesting", "--scope", "repo",
                        "--nesting-limit", "1", "--max", "0", "--root", root],
                       check=True, capture_output=True, text=True)
    paths = sorted({f["path"] for f in json.loads(r.stdout)})
    failures += check("demo/ 排除、生产路径照报", paths, ["src/chat.rs"])

    print("PASS" if not failures else f"{failures} FAILED")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
