# MoreScreen（多屏扩展 · 第一版）

> 远端仓库：<https://github.com/luobuotuo/MoreScreen>（SSH：`git@github.com:luobuotuo/MoreScreen.git`）

把闲置的平板/手机/旧电脑变成电脑的**第二块屏幕**，走的是"虚拟显示器 + 原生采集编码 + 局域网传输"的路线，
架构参考与对标对象是 [Deskreen](https://deskreen.com)（AGPL-3.0，本仓库仅作原理参考，未复制其代码）。

当前进度（实测，非计划）：

| 阶段 | 状态 | 关键数据 |
|---|---|---|
| Stage 0 基线 | ✅ | Win10 19045 / Intel Iris Xe / 1920×1080@60 |
| Stage 1 枚举 | ✅ | 37 个显示目标，虚拟判定可靠 |
| Stage 2 采集 | ✅ | WGC+D3D11 60.1fps、首帧 14.1ms、亮度自检 240.8/99.5% |
| Stage 3 编码 | ✅ | H.264 Constrained Baseline 1920×1080，`h264_qsv` 硬编可用 |
| Stage 4 传输 | ✅ | WebSocket 广播 + 浏览器解码器配置成功；AU `[SPS,PPS,IDR]` 可独立解码 |
| Stage 5 测量 | ✅ | 主机侧 **p50 7.57ms / p95 20ms**（背压丢帧保延迟）；真实单帧编码 ~7ms |
| 虚拟屏采集 | ✅ | ToDeskIddDriver 创建 800×600@60 虚拟屏（位置 1920,0），采集 358 帧/60.0fps、非黑占比 100%、NV12 720,000B，导出帧肉眼可读 |

提交会自动推送到 GitHub（见 [`docs/03-git-workflow.md`](docs/03-git-workflow.md)）。
提交会自动推送到 GitHub（见 [`docs/03-git-workflow.md`](docs/03-git-workflow.md)）。
详细进度见 [`docs/01-progress.md`](docs/01-progress.md)，环境基线见 [`docs/00-baseline.md`](docs/00-baseline.md)，
虚拟屏出路见 [`docs/02-vdd-options.md`](docs/02-vdd-options.md)，Git 工作流见 [`docs/03-git-workflow.md`](docs/03-git-workflow.md)。

## 目录

```
crates/core/       平台无关核心：显示模型 / 会话状态机 / 分段指标（零依赖）
crates/host-win/   Windows 宿主端：显示枚举 / WGC 采集 / GPU 回读 / H.264 编码 / WS 传输 / VDD 节点
web/receiver/      浏览器接收端（WebCodecs 解码 + 统计）
scripts/           env.sh（环境变量）/ auto_push.sh（提交自动推送）/ ws_probe.py / screen_grab.py
tools/             VDD 驱动包、SetupAPI 建节点脚本、一键安装脚本
docs/              基线 / 进度 / 虚拟屏出路 / Git 工作流
```

## 快速开始

```bash
source scripts/env.sh          # 必须：绕过失效代理 + 指定构建输出目录

cargo build --release -p remote-screen-host-win
cargo test  --release -p remote-screen-core

# 枚举显示目标（区分物理/虚拟）
cargo run --release -p remote-screen-host-win -- displays

# 只读探测某设备支持的模式
cargo run --release -p remote-screen-host-win -- modes --index 0

# 采集探测：抓指定屏 5 秒，输出帧率与首帧延迟
cargo run --release -p remote-screen-host-win -- capture-probe --index 0 --seconds 5

# 受限环境下跑核心断言
cargo run --release -p remote-screen-core --bin core_selfcheck

# 虚拟屏就绪检测 + 扩展屏采集闭环验证（等待虚拟屏出现后自动采集并导出 PNG）
python scripts/verify_virtual_screen.py --timeout 180

# 启动流服务（手机/平板浏览器接收端用）。rs-host.exe 未加入 PATH，请用启动脚本：
scripts\run_stream.cmd                        # Windows：默认物理主屏，WS 端口 8787
scripts\run_stream.cmd --index 5              # 指定显示目标编号
# 启动后用手机/平板浏览器打开它打印的「接收端页面」地址（形如 http://192.168.0.104:8788/）
# 注意：服务前台运行，关闭窗口即停止
```

### 点亮虚拟屏的两种方式

| 方式 | 原理 | 操作 |
|---|---|---|
| **A. 远程连接一次** | ToDesk / Parsec 是**会话型** IddCx 驱动：远端连上时客户端才通过 IddCx 创建显示器，断开即销毁 | 手机/平板装 ToDesk → 输入本机设备代码 → 连上并保持 |
| **B. 常驻型驱动** | MttVDD 是**常驻型**：`vdd_settings.xml` 里 `<monitors><count>1</count>`，驱动加载即自动建 1 块显示器 | 管理员终端 `bcdedit /set testsigning on` → 重启（SignPath 签名驱动在测试模式下才被允许加载） |

两条路任选其一，随后跑 `python scripts/verify_virtual_screen.py` 即可完成采集闭环验证。

## 设计要点

- **显示身份用设备实例 ID**，不用"显示器 1/2"下标，避免插拔/重排后错绑。
- **每块虚拟屏独立会话**，独立采集器、编码器、视频轨；不拼超宽图再裁切。
- **逐帧分段打点**（采集 → 编码 → 发送），延迟能被定位到具体环节。
- **状态机强制合法迁移**，故障必须落到明确状态，不允许"画面不动但不知道卡在哪"。
- 采集用 **Windows.Graphics.Capture + D3D11**，帧面留在 GPU，不做 CPU 回读。

## 许可证

本仓库自有代码 MIT OR Apache-2.0；`zpw.down/` 下为第三方开源源码（Deskreen CE，AGPL-3.0），
仅作为原理参考与对照实现，**未复制进本项目代码**，且不参与构建。
