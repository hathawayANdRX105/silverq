#!/usr/bin/env bash
# 下载 zashboard 官方构建产物到 silverq 的 UI 目录（MIT 许可，见上游仓库）。
# 用法: fetch-zashboard.sh [目标目录] [版本号]
set -euo pipefail
DEST="${1:-$HOME/.local/share/silverq/ui}"
VER="${2:-latest}"
BASE="https://github.com/Zephyruso/zashboard/releases"
if [ "$VER" = latest ]; then
  URL="$BASE/latest/download/dist.zip"
else
  URL="$BASE/download/v$VER/dist.zip"
fi
mkdir -p "$DEST"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
echo "下载 $URL ..."
curl -fL --progress-bar -o "$TMP/dist.zip" "$URL"
python3 - "$TMP/dist.zip" "$DEST" <<'PY'
import zipfile, sys, os
dest = sys.argv[2]
with zipfile.ZipFile(sys.argv[1]) as z:
    names = z.namelist()
    # dist.zip 内层可能是 dist/ 前缀
    prefix = os.path.commonprefix(names)
    if prefix and all(n.startswith(prefix) for n in names):
        for n in names:
            if n == prefix: continue
            z.extract(n, dest)
            os.rename(os.path.join(dest, n), os.path.join(dest, n[len(prefix):]))
        # 清掉空目录
        leftover = os.path.join(dest, prefix.rstrip('/'))
        if os.path.isdir(leftover):
            import shutil; shutil.rmtree(leftover, ignore_errors=True)
    else:
        z.extractall(dest)
PY
echo "已解压到 $DEST ($(du -sh "$DEST" | cut -f1))"
