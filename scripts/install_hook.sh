#!/bin/sh
# 安装提交自动推送 hook（clone 后执行一次即可）。
# 用法: sh scripts/install_hook.sh
set -eu
HOOK_DIR="$(git rev-parse --git-dir)/hooks"
SRC="$(dirname "$0")/auto_push.sh"
cp "$SRC" "$HOOK_DIR/post-commit"
chmod +x "$HOOK_DIR/post-commit"
# 强制 LF，避免 CRLF 导致 sh 无法解析
python - "$HOOK_DIR/post-commit" <<'PY' 2>/dev/null || true
import io, sys
p = sys.argv[1]
data = io.open(p, "rb").read().replace(b"\r\n", b"\n")
io.open(p, "wb").write(data)
PY
git config auto-push.enabled true
echo "hook 已安装到 $HOOK_DIR/post-commit"
echo "自检："
sh "$HOOK_DIR/post-commit"
echo "日志：.git/auto-push.log"
