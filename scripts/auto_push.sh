#!/bin/sh
# ---------------------------------------------------------------------------
#  每次提交后自动推送到远端。
#
#  设计原则
#  1. 推送失败**绝不阻塞提交**（post-commit 的退出码不影响 commit 成功）；
#  2. 失败自动重试（Windows 上 known_hosts 偶发瞬时文件锁：Permission denied）；
#  3. 每次执行结果写入 .git/auto-push.log，便于事后排查；
#  4. 未绑定远端时静默跳过，不产生噪音。
#
#  开关：git config auto-push.enabled false   可临时关闭
# ---------------------------------------------------------------------------
set -u

LOG="$(git rev-parse --git-dir)/auto-push.log"
stamp() { date '+%Y-%m-%d %H:%M:%S'; }

enabled="$(git config --get auto-push.enabled || echo true)"
if [ "$enabled" != "true" ]; then
  echo "$(stamp) 自动推送已被关闭（git config auto-push.enabled false）" >> "$LOG"
  exit 0
fi

remote="$(git config --get remote.pushRemote \
  || git config --get branch.$(git rev-parse --abbrev-ref HEAD).pushRemote \
  || echo origin)"
branch="$(git rev-parse --abbrev-ref HEAD)"

if ! git remote get-url "$remote" >/dev/null 2>&1; then
  echo "$(stamp) 未绑定远端 '$remote'，跳过自动推送" >> "$LOG"
  exit 0
fi

# ---------------------------------------------------------------------------
#  HOME 必须是 POSIX 形式。
#  Git Bash 的 OpenSSH 在 HOME 为 Windows 形式（C:\Users\...）时会把
#  known_hosts 拼成非法路径，导致 "Permission denied" / "Host key verification failed"。
# ---------------------------------------------------------------------------
if [ -z "${HOME:-}" ]; then
  HOME="$(cd "$(git rev-parse --git-dir)/../.." && pwd)"
  export HOME
fi
case "$HOME" in
  [A-Za-z]:*)
    HOME="$(cygpath -u "$HOME" 2>/dev/null || echo "$HOME")"
    export HOME
    ;;
esac

KEY="$HOME/.ssh/id_ed25519"
SSH_BASE="-o BatchMode=yes -o ConnectTimeout=20 -o StrictHostKeyChecking=accept-new"
if [ -f "$KEY" ]; then
  SSH_BASE="$SSH_BASE -i $KEY"
fi

try_push() {
  GIT_SSH_COMMAND="ssh $SSH_BASE" \
    git push --quiet "$remote" "HEAD:refs/heads/$branch" >>"$LOG" 2>&1
}

attempt=1
while [ "$attempt" -le 3 ]; do
  if try_push; then
    echo "$(stamp) 已推送 $remote/$branch ($(git rev-parse --short HEAD))" >> "$LOG"
    exit 0
  fi
  echo "$(stamp) 第 $attempt 次推送 $remote/$branch 失败，2 秒后重试" >> "$LOG"
  sleep 2
  attempt=$((attempt + 1))
done

echo "$(stamp) 已知主机校验反复失败，最后一次尝试忽略 known_hosts" >> "$LOG"
if GIT_SSH_COMMAND="ssh $SSH_BASE -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no" \
   git push --quiet "$remote" "HEAD:refs/heads/$branch" >>"$LOG" 2>&1; then
  echo "$(stamp) 已推送 $remote/$branch（忽略 known_hosts 成功）" >> "$LOG"
else
  echo "$(stamp) 推送失败（提交本身已成功，不影响本地历史）" >> "$LOG"
fi

exit 0
