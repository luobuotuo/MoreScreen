# Git 工作流与自动推送

## 远端绑定

| 项 | 值 |
|---|---|
| remote | `origin` |
| URL | `git@github.com:luobuotuo/MoreScreen.git`（SSH） |
| 分支 | `master` → 跟踪 `origin/master` |
| 认证 | 本机 `~/.ssh/id_ed25519`，GitHub 账户 `luobuotuo`（已验证 `ssh -T git@github.com` 通过） |

**为什么走 SSH 而不是 HTTPS**：本机 GitHub HTTPS 通道不稳定——直连 `github.com:443` 会超时，
全局代理 `127.0.0.1:19828` 对 github.com 返回 `Empty reply`；而 SSH 的 22 与 443 端口均连通。
SSH 的另一个好处是**无交互**，后台自动推送不会卡在浏览器授权上。

## 自动推送机制

`scripts/auto_push.sh` 被安装为 `.git/hooks/post-commit`，每次提交后自动推送到 `origin`。

设计要点：

- **推送失败绝不阻塞提交**——`post-commit` 的退出码不影响 commit 成功；
- 每次执行结果写入 `.git/auto-push.log`，便于事后排查；
- 未绑定远端时静默跳过，不产生噪音；
- 远程名取 `remote.pushRemote` → `branch.<name>.pushRemote` → `origin`；
- 只推当前分支，不推其它 refs。

### 开关

```bash
git config auto-push.enabled false   # 临时关闭
git config auto-push.enabled true    # 恢复
```

### 手动推送

```bash
git push                       # 推到 upstream
git push origin master         # 显式
```

### 稳定性加固（实测踩坑后加入）

Windows Defender 会在 ssh 进程首次打开 `~/.ssh/` 下文件时做扫描，可能造成**瞬时独占锁**，
表现为：

```text
Load key "/c/Users/sz-ws016/.ssh/id_ed25519": Permission denied
git@github.com: Permission denied (publickey).
Failed to add the host to the list of known hosts
```

hook 对策：

- **不使用 `-i` 显式指定私钥**——ssh 默认即可从 `~/.ssh` 读取，少一次文件打开即少一个锁竞争点；
- **失败自动重试 5 次、间隔 5 秒**（覆盖约 20 秒的文件锁窗口）；
- 最后一次兜底忽略 `known_hosts`（密钥认证仍然生效）；
- `HOME` 统一规范化为 POSIX 形式（Windows 形式会让 Git Bash 的 OpenSSH 拼出非法路径）。

### 可选：常驻 ssh-agent（更稳，需管理员一次）

把密钥放进常驻 agent 后，ssh 不再直读私钥文件，可彻底绕开文件锁：

```cmd
:: 管理员终端执行一次
Set-Service ssh-agent -StartupType Automatic
Start-Service ssh-agent
ssh-add %USERPROFILE%\.ssh\id_ed25519
```

未启用时依赖 hook 的重试机制，实测同样可用。

### 稳定性加固（实测踩坑后加入）

Windows Defender 会在 ssh 进程首次打开 `~/.ssh/` 下文件时做扫描，可能造成**瞬时独占锁**，
表现为：

```text
Load key "/c/Users/sz-ws016/.ssh/id_ed25519": Permission denied
git@github.com: Permission denied (publickey).
Failed to add the host to the list of known hosts
```

hook 对策：

- **不使用 `-i` 显式指定私钥**——ssh 默认即可从 `~/.ssh` 读取，少一次文件打开即少一个锁竞争点；
- **失败自动重试 5 次、间隔 5 秒**（覆盖约 20 秒的文件锁窗口）；
- 最后一次兜底忽略 `known_hosts`（密钥认证仍然生效）；
- `HOME` 统一规范化为 POSIX 形式（Windows 形式会让 Git Bash 的 OpenSSH 拼出非法路径）。

### 可选：常驻 ssh-agent（更稳，需管理员一次）

把密钥放进常驻 agent 后，ssh 不再直读私钥文件，可彻底绕开文件锁：

```cmd
:: 管理员终端执行一次
Set-Service ssh-agent -StartupType Automatic
Start-Service ssh-agent
ssh-add %USERPROFILE%\.ssh\id_ed25519
```

未启用时依赖 hook 的重试机制，实测同样可用。

### 排障

```bash
cat .git/auto-push.log         # 查看自动推送历史
git log --oneline origin/master..master   # 查看未推送的提交
```

## 仓库体积约束（GitHub 硬限制）

| 限制 | 值 |
|---|---|
| 单文件大小 | 100 MB（硬拒绝） |
| 仓库建议体积 | < 1 GB |
| 单次推送建议 | < 1 GB |

`tools/ffmpeg/bin/ffmpeg.exe`（100.54 MB）**已从 git 历史中清除**（`git filter-repo --path tools/ffmpeg --invert-paths`），
并在 `.gitignore` 中忽略。ffmpeg 改为由本机安装、通过 `RS_FFMPEG` 环境变量指定路径，
`scripts/env.sh` 与 `resolve_ffmpeg()` 会依次在环境变量、`tools/ffmpeg/bin/`、WinGet 链接目录、PATH 中查找。

**重写前的完整历史已备份为**：
`D:/app-workbuddy/remote-screen-backup-20261114-1846.bundle`（38 MB）
（恢复方式：`git clone <bundle> <目标目录>`）

## 当前仓库状态

- 12 次提交，SHA 因历史重写已全部更新（旧 SHA 不再存在于远端）；
- 代码 3,134 行 Rust + 接收端 152 行 + 工具脚本；
- 测试：core 10 项 + host-win 6 项，全部通过。
