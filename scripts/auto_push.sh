#!/bin/sh
# ---------------------------------------------------------------------------
#  每次提交后自动推送到远端。
#
#  设计原则
#  1. 推送失败**绝不阻塞提交**（post-commit 的退出码不影响 commit 成功）；
#  2. 失败自动重试；
#  3. 每次执行结果写入 .git/auto-push.log，便于事后排查；
#  4. 未绑定远端时静默跳过，不产生噪音。
#
#  开关：git config auto-push.enabled false   可临时关闭
#
#  关键经验（本机实测）
#  ---------------------------------------------------------------------------
#  * 不要用 GIT_SSH_COMMAND 覆盖 ssh 调用方式。
#    带 GIT_SSH_COMMAND 时，git 走 `sh -c` 二次解析，MSYS2 会在这一步把
#    ~/.ssh 访问路径改写，ssh 报 "Load key ...: Permission denied" +
#    "Failed to add the host to the list of known hosts"；
#    而**默认调用方式（不设 GIT_SSH_COMMAND）在同一台机器上手动 push 是成功的**。
#  * 必须把 HOME 规范成 POSIX 形式（/c/Users/...），Windows 形式（C:\Users\...）
#    会让 MSYS2 的 OpenSSH 拼出非法路径。
#  * 关闭 MSYS2 参数转换，避免子进程拿到被改写的路径。
# ---------------------------------------------------------------------------
set -u

LOG="$(git rev-parse --git-dir)/auto-push.log"
DIAG="$(git rev-parse --git-dir)/auto-push.diag"
stamp() { date '+%Y-%m-%d %H:%M:%S'; }

# ---- 开关检查 ----
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

# ---- HOME 规范化为 POSIX 形式 ----
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

# ---- 关闭 MSYS2 路径/参数转换，避免子进程拿到被改写的 ~/.ssh 路径 ----
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1
# 任何交互式提示都不允许（防止后台推送卡死）
export GIT_TERMINAL_PROMPT=0

# ---- 首次运行时记录一次 ssh 连通性诊断 ----
if [ ! -f "$DIAG" ]; then
  {
    echo "=== 诊断 @ $(stamp) ==="
    echo "HOME=$HOME"
    echo "USER=${USER:-<unset>}  whoami=$(whoami 2>&1)"
    echo "MSYSTEM=${MSYSTEM:-<unset>}"
    echo "--- ssh 直连测试 ---"
    ssh -o BatchMode=yes -T git@github.com 2>&1 | head -3
    echo "--- ssh 路径 ---"
    which ssh 2>&1
    ssh -V 2>&1 | head -1
  } > "$DIAG" 2>&1 || true
fi

# ---- 推送（使用 git 默认的 ssh 调用方式，不用 GIT_SSH_COMMAND）----
try_push() {
  git push --quiet "$remote" "HEAD:refs/heads/$branch" >>"$LOG" 2>&1
}

attempt=1
MAX=4
while [ "$attempt" -le "$MAX" ]; do
  if try_push; then
    echo "$(stamp) 已推送 $remote/$branch ($(git rev-parse --short HEAD))" >> "$LOG"
    exit 0
  fi
  if [ "$attempt" -lt "$MAX" ]; then
    echo "$(stamp) 第 $attempt/$MAX 次推送 $remote/$branch 失败，4 秒后重试" >> "$LOG"
    sleep 4
  fi
  attempt=$((attempt + 1))
done

echo "$(stamp) 推送 $remote/$branch 最终失败（提交本身已成功，不影响本地历史）" >> "$LOG"
exit 0
