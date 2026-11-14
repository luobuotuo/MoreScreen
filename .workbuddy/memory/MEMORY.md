# 项目长期记忆：remote-screen（多屏扩展）

## 目标与定位
把闲置平板/手机/旧电脑变成电脑的第二块屏幕。对标 Deskreen，但走"虚拟显示器 + 原生采集编码 + 局域网传输"。
最终形态：Tauri 2 + React/TS 宿主 UI、Rust 核心、平台原生采集编码、WebRTC 传输、浏览器/原生接收端。
**POC 阶段直接用 Rust 写 CLI 验证链路，验证通过再包 Tauri。**

## 目录与职责
- `crates/core/`：平台无关核心（零依赖）。`display.rs` 显示模型/稳定身份、`session.rs` 会话状态机、`metrics.rs` JSONL 分段指标。
- `crates/host-win/`：Windows 端。`displays.rs` 枚举+模式+拓扑、`capture.rs` WGC/D3D11 采集、`main.rs` CLI。
- `web/receiver/`：浏览器接收端骨架（统计口径已落地）。
- `scripts/env.sh`：环境变量。`docs/`：基线与进度。`zpw.down/`：Deskreen CE 源码（AGPL-3.0，仅参考，不参与构建）。

## 铁律
1. **显示身份用设备实例 ID**，绝不用"显示器 1/2"下标（插拔/重排会错绑）。
2. **每块虚拟屏独立会话**：独立采集器/编码器/视频轨，不拼超宽图再裁切。
3. **逐帧分段打点**：采集→编码→发送同一 frame_id 串起来，延迟必须能定位到环节。
4. **状态机强制合法迁移**，非法迁移直接报错，不允许状态漂移。
5. **改用户桌面布局的接口（如 ChangeDisplaySettingsExW）必须显式触发**，默认不调用。
6. 先打通可测量链路，再做 UI/配对/输入回传。

## 环境铁律（本机，跨会话有效）
- 代理 `127.0.0.1:64796` 不可用 → `curl --noproxy '*'`；cargo 必须同时 `no_proxy=*`。
- **一律用 `--release` 跑测试**（debug 版 cargo test 产物在本机被拒执行，WinError 5）。
- **`CARGO_TARGET_DIR` 必须指向系统临时目录**（工作区内 .exe 会被回收）。
- 工具链：Rust 1.100 windows-gnu 宿主 + MinGW `D:\mingw64x2`（GCC 15.2.0，脚本钉住）；无 MSVC/新 SDK。
- **两套 MinGW 并存**（`D:\mingw64x2` 15.2.0 + WinGet WinLibs 16.2.0）：PATH 顺序不可漂移，改动后必须重跑回归。
- crates 走 rsproxy 稀疏索引（`~/.cargo/config.toml` 已配置）。

## 现状（2026-11-14）
Stage 0 ✅ / Stage 1 ✅（枚举 24~35 个显示目标，虚拟屏判定可靠）/ Stage 2 ✅（物理主屏实测 60.1fps、首帧 14ms、1920×1080）
/ Stage 3a ✅（GPU 回读 + 亮度自检 240.8/99.5% + NV12 导出 3,110,400B，肉眼可读）。
**阻塞点：本机 Parsec/ToDesk/AskLink 虚拟适配器未建会话时不暴露模式 → 已下载静态 VDD(MttVDD 25.7.23)，
安装需 UAC 提权，等待用户确认；回滚 `pnputil /delete-driver mttvdd.inf /uninstall /force`。**
未做：H.264 编码(MFT)、WebRTC 传输、端到端延迟测量、macOS/Linux。
沙箱限制：PowerShell 工具禁止拉起 cmd.exe；提权用 `Start-Process -Verb RunAs` 直接调 pnputil.exe。

## 参考实现要点（Deskreen CE）
Electron + React + TS + electron-vite；桌面捕获走 `desktopCapturer`；传输 simple-peer/WebRTC；
宿主内嵌 Koa + Socket.IO 做本地信令与静态页托管（`src/server/index.ts`，room id 做 SHA-256，端口占用自动降级）；
接收端为独立 Vite 静态工程 `src/client-viewer`。**它不自带虚拟显示器**，需外部虚拟显示适配器。
