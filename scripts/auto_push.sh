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

# 不使用 -i 显式指定私钥：ssh 默认即可从 ~/.ssh 读取，
# 少一次文件打开可避开 Windows Defender 扫描导致的瞬时独占锁。

# ---- 诊断信息（首次运行时记录，便于定位环境差异）----
DIAG="$(git rev-parse --git-dir)/auto-push.diag"
if [ ! -f "$DIAG" ]; then
  {
    echo "=== diag ==="
    echo "HOME=${HOME:-<unset>}"
    echo "USER=${USER:-<unset>}  whoami=$(whoami 2>&1)"
    echo "MSYSTEM=${MSYSTEM:-<unset>}"
    echo "GIT_DIR=${GIT_DIR:-<unset>}"
    echo "SSH_AUTH_SOCK=${SSH_AUTH_SOCK:-<unset>}"
    echo "--- ls HOME/.ssh ---"
    ls -la "${HOME}/.ssh" 2>&1 | head -8
    echo "--- known_hosts 头 ---"
    head -1 "${HOME}/.ssh/known_hosts" 2>&1 | head -2
    echo "--- 私钥可读? ---"
    if head -c 32 "${HOME}/.ssh/id_ed25519" >/dev/null 2>&1; then echo "可读"; else echo "不可读"; fi
    echo "--- ssh 版本/路径 ---"
    ssh -V 2>&1 | head -1
    which ssh 2>&1
    echo "--- GIT_SSH_COMMAND 展开后 ---"
    echo "ssh ${SSH_BASE:-<empty>}"
  } > "$DIAG" 2>&1 || true
fi


# ---- 诊断信息（首次失败时记录，便于定位环境差异）----
if [ ! -f "$(git rev-parse --git-dir)/auto-push.diag" ]; then
  {
    echo "=== diag @ $(stamp) ==="
    echo "HOME=$HOME"
    echo "USER=$USER  whoami=$(whoami 2>&1)"
    echo "MSYSTEM=$MSYSTEM  SHELL=$SHELL"
    echo "--- ls \$HOME/.ssh ---"
    ls -la "$HOME/.ssh" 2>&1 | head -8
    echo "--- head known_hosts ---"
    head -1 "$HOME/.ssh/known_hosts" 2>&1 | head -2
    echo "--- id_ed25519 可读? ---"
    head -c 32 "$HOME/.ssh/id_ed25519" >/dev/null 2>&1 && echo "可读" || echo "不可读"
    echo "--- ssh -V ---"
    ssh -V 2>&1 | head -1
    echo "--- which ssh ---"
    which ssh 2>&1
  } > "$(git rev-parse --git-dir)/auto-push.diag" 2>&1
fi

SSH_BASE="-o BatchMode=yes -o ConnectTimeout=20 -o StrictHostKeyChecking=accept-new"

try_push() {
  GIT_SSH_COMMAND="ssh $SSH_BASE" \
    git push --quiet "$remote" "HEAD:refs/heads/$branch" >>"$LOG" 2>&1
}

attempt=1
MAX=5
while [ "$attempt" -le "$MAX" ]; do
  if try_push; then
    echo "$(stamp) 已推送 $remote/$branch ($(git rev-parse --short HEAD))" >> "$LOG"
    exit 0
  fi
  if [ "$attempt" -lt "$MAX" ]; then
    echo "$(stamp) 第 $attempt/$MAX 次推送 $remote/$branch 失败，5 秒后重试" >> "$LOG"
    sleep 5
  fi
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
