#!/usr/bin/env bash
# 统一开发环境（source 使用）：
#   source scripts/env.sh
#
# 解决三个本机特有问题：
#  1) 系统代理 127.0.0.1:64796 不可用 -> 清空并设置 NO_PROXY=*
#  2) crates 走 rsproxy 稀疏索引（已写入 ~/.cargo/config.toml）
#  3) 构建产物放系统临时目录，避免工作区内 .exe 被回收
export http_proxy= https_proxy= HTTP_PROXY= HTTPS_PROXY= ALL_PROXY= all_proxy=
export no_proxy='*' NO_PROXY='*'
export PATH="/c/Users/sz-ws016/.cargo/bin:/d/mingw64x2/mingw64/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/c/Users/sz-ws016/AppData/Local/Temp/rs-target}"

# 常用命令：
#   cargo build --release -p remote-screen-host-win
#   cargo test  --release -p remote-screen-core
#   cargo run   --release -p remote-screen-host-win -- displays
#   cargo run   --release -p remote-screen-core --bin core_selfcheck
