# Stage 0 基线与环境事实

> 记录时间：2026-11-14。这份文档是后续所有性能数据的"参照系"，换机器必须重跑。

## 一、参考主机

| 项 | 实测值 |
|---|---|
| 操作系统 | Windows 10 家庭中文版 22H2（Build 19045） |
| GPU | Intel(R) Iris(R) Xe Graphics（驱动 32.0.101.7088） |
| 物理显示 | AUO068B 内屏，1920×1080@60Hz，设备 `\\.\DISPLAY1`，位于 (0,0) |
| 虚拟显示适配器 | Parsec VDA（16 个显示设备）、ToDesk `tdIdd`（3 个）、AskLink（1 个）——**全部未点亮** |
| 虚拟适配器枚举器 | 均为 `ROOT\DISPLAY\xxxx`；真实 GPU 为 `PCI\VEN_8086&DEV_9A49` |
| MinGW | `D:\mingw64x2\mingw64\bin\gcc`（本机已预装） |
| Rust | 1.100.0，宿主 `x86_64-pc-windows-gnu`（本轮新装） |

## 二、工具链与网络事实（踩坑记录）

1. **本机代理不可用**：`http_proxy/https_proxy` 指向 `127.0.0.1:64796`，连接被拒。
   - 直连方案：`curl --noproxy '*'`，以及给 cargo 设 `no_proxy=*`（清空 proxy 变量不够，必须设 NO_PROXY）。
   - 国内镜像：crates 走 `rsproxy.cn`（实测 12.7 MB/s），rustup 亦可用 rsproxy 镜像。
2. **debug 版 `cargo test` 产物在本机被拒绝执行**（WinError 5 拒绝访问），
   同目录普通二进制可正常运行，`--release` 的测试 harness 也可正常运行。
   → 本项目统一用 `--release` 跑测试，并额外提供 `core_selfcheck` 二进制作为受限环境下的断言入口。
3. **构建产物不要放工作区**：工作区内新生成的 `.exe` 会在命令结束后被回收。
   → 统一用 `CARGO_TARGET_DIR` 指向系统临时目录（见 `scripts/env.sh`）。
4. **Windows SDK 只有旧版本**（10240 / 14393 / 17763），没有 MSVC Build Tools；
   因此选择 **Rust GNU 宿主 + 预装 MinGW**，而不是 MSVC 工具链。

## 三、已验证事实（实测）

| 结论 | 证据 |
|---|---|
| 枚举接口可用 | `rs-host displays` 列出 24 个显示目标，正确区分物理/虚拟 |
| 虚拟适配器识别可靠 | 靠适配器实例 ID 是否为 `ROOT\...` 判定，不依赖厂商名 |
| 模式读取可用 | 物理屏读到 18 种模式（1920×1080@60/40、640×480@60…） |
| **WGC 采集可用** | `capture-probe --index 0`：5 秒 300 帧，60.1 fps，首帧 14.1 ms，内容 1920×1080 |
| 虚拟适配器当前不可用 | Parsec/ToDesk/AskLink 三者模式列表均为空（需其客户端建立会话才点亮） |

## 四、未在本机验证（需要额外条件）

- **虚拟屏采集**：需先有一块被点亮的虚拟显示器（自装静态 VDD，或让上述任一客户端建立会话）。
- **H.264 硬件编码**：尚未实现与验证（MF MFT 路径）。
- **WebRTC 传输与浏览器解码**：尚未实现（Stage 4）。
- **端到端延迟测量**：尚未实施（高速摄像法 / 分段法）。
- **macOS / Linux**：完全未涉及。
