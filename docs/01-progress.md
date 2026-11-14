# 第一版执行进度与下一步

## 一、阶段状态（对照"先打通可测量视频链路"）

| 阶段 | 目标 | 状态 | 证据 |
|---|---|---|---|
| 0 固定基线 | 环境/硬件/工具链可复现 | ✅ 完成 | `docs/00-baseline.md` |
| 1 虚拟屏 | 枚举并绑定虚拟显示器 | ⚠️ 部分 | 能枚举 + 识别虚拟适配器；**本机虚拟适配器未点亮**（模式列表为空） |
| 2 指定屏采集 | WGC + D3D11 抓指定屏 | ✅ 完成（物理屏） | 5s / 300 帧 / 60.1fps / 首帧 14.1ms / 1920×1080 |
| 2b 内容自检 | GPU 回读 + 亮度判定 + NV12 导出 | ✅ 完成 | 平均亮度 240.8 / 非黑占比 99.5% / NV12 3,110,400 B = 1920×1080×1.5，导出帧肉眼可读 |
| 3 本地编码 | H.264 硬编并落盘 | ⬜ 未开始 | NV12 输入已就绪，待接 Media Foundation MFT |
| 4 WebRTC | 局域网传输 + 浏览器解码 | ⬜ 未开始 | 接收端骨架：`web/receiver/index.html` |
| 5 测量与故障注入 | 分段耗时 + 弱网 + 恢复 | ⬜ 未开始 | 指标写入器已就绪（JSONL） |

## 二、已落地的工程骨架

```
crates/core/        与平台无关的核心层（零外部依赖）
  ├ display.rs      显示目标模型 + 稳定身份 + 虚拟屏判定
  ├ session.rs      会话状态机（非法迁移直接报错，不允许状态漂移）
  └ metrics.rs      逐帧分段指标 → JSONL
crates/host-win/    Windows 宿主端
  ├ displays.rs     EnumDisplayDevicesW / EnumDisplaySettingsExW / ChangeDisplaySettingsExW
  ├ capture.rs      D3D11 设备 + WGC CaptureItem/FramePool/Session + 帧率统计
  └ main.rs         CLI: displays / modes / capture-probe / selftest
web/receiver/       浏览器接收端骨架（Stage 4）
scripts/env.sh      环境变量（proxy 绕过 + CARGO_TARGET_DIR + PATH）
```

## 三、当前阻塞点（含 UAC 提权）

**本机的三个虚拟显示适配器（Parsec / ToDesk / AskLink）在没有客户端会话时不暴露任何显示模式**，
所以现在能抓的是物理屏，不是"扩展出来的第二块屏"。三条解法：

| 方案 | 代价 | 建议 |
|---|---|---|
| A. 安装静态 VDD（Virtual Display Driver / IddCx，MIT） | 需要管理员权限安装驱动 | ✅ 推荐用于打通链路；注意 GPU 大版本升级前先卸载 |
| B. 让 ToDesk / Parsec / AskLink 任一客户端建立一次会话 | 无需安装，但依赖第三方软件在线 | 可作临时验证 |
| C. 自研 IddCx 驱动 | 3～6 个月 + 签名成本 | 产品化阶段再做 |

> **提权安装（方案 A）需要你点一次 UAC**：驱动包已下载到 `tools/vdd-driver/VirtualDisplayDriver/`
> （`MttVDD.inf` + 签名的 `mttvdd.cat` + `MttVDD.dll`，版本 25.7.23）。
> 桌面弹窗点"是"即可；若弹窗已消失或你看不到安全桌面，用管理员终端手动执行一行：
> ```
> pnputil /add-driver "D://app-workbuddy//01_remote-screen//tools//vdd-driver//VirtualDisplayDriver//MttVDD.inf" /install
> ```
> 卸载/回滚：`pnputil /delete-driver mttvdd.inf /uninstall /force`
> （GPU 大版本升级前务必先卸载，否则可能黑屏。）

> 采集链路本身已验证可用，所以**只要有一块被点亮的虚拟屏，Stage 2→4 就能连续推进**。

## 四、下一步三件事（按顺序）

1. **点亮一块虚拟屏**（方案 A/B），然后跑：
   `rs-host displays` 确认该屏 `活动=是`，再 `rs-host capture-probe --index <虚拟屏序号>`。
2. **实现 Stage 3：H.264 硬件编码**（Media Foundation MFT + GPU 色彩转换 BGRA→NV12），
   先输出 `.h264` 文件并用外部播放器/ffprobe 验证帧数与时间戳，再谈传输。
3. **实现 Stage 4：局域网传输 + 浏览器解码**，优先用极简方案验证链路可测量性，
   再替换为正式 WebRTC（协议不变，替换实现）。

## 五、已知的取舍与风险

- 采集用**轮询 `TryGetNextFrame`** 而非事件回调：少一层跨线程复杂度，首版可接受；
  正式版应换回事件回调以降低空转。
- 帧面保持在 GPU，**尚未做像素回读**，因此当前不能判断"是否黑屏"。
  虚拟屏采集后必须补上亮度抽检，否则会出现"帧数正常但画面全黑"的假通过。
- 模式修改接口 `ChangeDisplaySettingsExW` 已实现但**默认不调用**：
  它会改变用户桌面布局，必须由 UI 显式触发。
- `display.rs` 的虚拟判定依赖 `ROOT\...` 枚举器这一约定；
  若换用自研驱动，需复核该判定仍然成立。
