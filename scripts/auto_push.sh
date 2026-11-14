#!/bin/sh
# ---------------------------------------------------------------------------
#  每次提交后自动推送到远端。
#  设计原则：推送失败**绝不阻塞提交**（post-commit 的退出码不影响 commit 成功），
#  失败原因写入 .git/auto-push.log，便于事后排查。
#  开关：git config auto-push.enabled false  可临时关闭
# ---------------------------------------------------------------------------
set -u

LOG="$(git rev-parse --git-dir)/auto-push.log"
stamp() { date '+%Y-%m-%d %H:%M:%S'; }

# 开关检查
enabled="$(git config --get auto-push.enabled || echo true)"
if [ "$enabled" != "true" ]; then
  echo "$(stamp) auto-push 已被关闭（git config auto-push.enabled false）" >> "$LOG"
  exit 0
fi

remote="$(git config --get remote.pushRemote || git config --get branch.$(git rev-parse --abbrev-ref HEAD).pushRemote || echo origin)"
branch="$(git rev-parse --abbrev-ref HEAD)"

# 未绑定远端时静默跳过，不产生噪音
if ! git remote get-url "$remote" >/dev/null 2>&1; then
  echo "$(stamp) 未绑定远端 '$remote'，跳过自动推送" >> "$LOG"
  exit 0
fi

if git push --quiet "$remote" "HEAD:refs/heads/$branch" >>"$LOG" 2>&1; then
  echo "$(stamp) 已推送 $remote/$branch ($(git rev-parse --short HEAD))" >> "$LOG"
else
  rc=$?
  echo "$(stamp) 推送 $remote/$branch 失败（提交本身已成功，不影响本地历史）" >> "$LOG"
fi

exit 0
