# 第一版执行进度与下一步

## 一、阶段状态（对照"先打通可测量视频链路"）

| 阶段 | 目标 | 状态 | 证据 |
|---|---|---|---|
| 0 固定基线 | 环境/硬件/工具链可复现 | ✅ 完成 | `docs/00-baseline.md` |
| 1 虚拟屏 | 枚举并绑定虚拟显示器 | ⚠️ 部分 | 能枚举 + 识别虚拟适配器；**本机虚拟适配器未点亮**（模式列表为空） |
| 2 指定屏采集 | WGC + D3D11 抓指定屏 | ✅ 完成（物理屏） | 5s / 300 帧 / 60.1fps / 首帧 14.1ms / 1920×1080 |
| 2b 内容自检 | GPU 回读 + 亮度判定 + NV12 导出 | ✅ 完成 | 平均亮度 240.8 / 非黑占比 99.5% / NV12 3,110,400 B = 1920×1080×1.5，导出帧肉眼可读 |
| 3 本地编码 | H.264 编码并落盘 | ✅ 完成（POC 用 ffmpeg，接口留好待换 MF） | Constrained Baseline / yuv420p / 1920×1080；ffprobe 可解；访问单元 [SPS,PPS,IDR] 独立解码出 6,220,800B 正确 RGB |
| 4 传输与接收 | WebSocket 广播 + 浏览器解码 | ⚠️ 传输已验证，**浏览器解码在本机无法验证** | WS 探针收到访问单元（NUL= SPS/PPS/IDR + P 帧），字节流无损；浏览器端 `isConfigSupported` 通过、解码器 configured；**但本机 Chromium 解码器对 H.264 与 VP8 一律报 `EncodingError: Decoding error.`（对照实验证明是环境问题）** |
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

## 三点五、实时链路实测（物理主屏，12 秒，h264_qsv 硬编）

| 指标 | 实测 |
|---|---|
| 采集 | 55 fps（显示器 60Hz 采集，未限速时）；限速到 30fps 后按目标节奏送帧 |
| 访问单元 | 211 个 / 9.2s ≈ 23 fps（编码器实际吞吐） |
| 采集→编码出流 | p50 **175 ms** / p95 204 ms / max 802 ms |
| 编码→发送完成 | p50 0.11 ms / p95 0.45 ms |
| 主机侧合计 | p50 175 ms / p95 204 ms |
| 平均码率 | ≈ 5.0 Mbps（1080p） |

**瓶颈定位：CPU 侧 BGRA→NV12 逐像素转换**（1080p 每帧 200 万像素、单线程 Rust 循环），
其次是采集回读本身。下一步优化方向：
1. 把 BGRA 原样喂给 ffmpeg，由其 SIMD 路径转 NV12（去掉我们自己的逐像素转换）；
2. 采集侧改 GPU 侧转换（D3D11 VideoProcessor），彻底不回读 CPU；
3. 编码背压：编码器跟不上时丢帧而不是堆积延迟。

## 四、本轮踩到并已修复的真实缺陷（记录以免重犯）

1. **访问单元不能按 NAL 切**：SPS/PPS 必须与它描述的图像帧放在一起，否则浏览器端 IDR 解不出。
2. **ffmpeg 的 SPS/PPS 只在码流开头出现一次**（global_header 模式）：必须缓存参数集，给每个关键帧补齐。
3. **客户端中途接入拿不到关键帧**：需缓存最近关键帧，新客户端接入时立即补发。
4. **`VideoDecoder.isConfigSupported()` 返回 Promise**：当同步值用会永远拿到 `undefined`，误判成"浏览器不支持 H.264"。
5. **解码器就绪前不能丢首包**：关键帧要排队，解码器 configure 成功后按序补喂。
6. **诊断代码会覆盖错误信息**：标题被每帧刷新覆盖，导致真实错误被掩盖（已改为独立字段）。
7. **指标 JSON 里 display_id 漏了引号**：整行不是合法 JSON，消费脚本直接解析失败（已修 + 补单测）。
8. **AU 边界不能等同于 VCL 边界**：x264 一帧会切成多个 slice，必须解析 slice 头的
   `first_mb_in_slice == 0` 才算新访问单元（已实现 ue(v) 解析 + 防竞争字节剥离 + 单测）。
9. **采集帧率必须与编码器标称帧率一致**：采集 55fps 喂给标称 30fps 的编码器会导致时间基错乱（已在采集端限速）。
10. **编码器读写必须解耦**：硬件编码器（QSV async_depth=1）常常"等下一帧才吐数据"，
    推帧后阻塞读会与编码器互锁死锁（已改为独立读线程 + 通道 + 非阻塞收取）。

## 五、下一步三件事（按顺序）

1. **点亮一块虚拟屏**（方案 A/B），然后跑：
   `rs-host displays` 确认该屏 `活动=是`，再 `rs-host capture-probe --index <虚拟屏序号>`。
2. **实现 Stage 3：H.264 硬件编码**（Media Foundation MFT + GPU 色彩转换 BGRA→NV12），
   先输出 `.h264` 文件并用外部播放器/ffprobe 验证帧数与时间戳，再谈传输。
3. **实现 Stage 4：局域网传输 + 浏览器解码**，优先用极简方案验证链路可测量性，
   再替换为正式 WebRTC（协议不变，替换实现）。

## 六、已知的取舍与风险

- 采集用**轮询 `TryGetNextFrame`** 而非事件回调：少一层跨线程复杂度，首版可接受；
  正式版应换回事件回调以降低空转。
- 帧面保持在 GPU，**尚未做像素回读**，因此当前不能判断"是否黑屏"。
  虚拟屏采集后必须补上亮度抽检，否则会出现"帧数正常但画面全黑"的假通过。
- 模式修改接口 `ChangeDisplaySettingsExW` 已实现但**默认不调用**：
  它会改变用户桌面布局，必须由 UI 显式触发。
- `display.rs` 的虚拟判定依赖 `ROOT\...` 枚举器这一约定；
  若换用自研驱动，需复核该判定仍然成立。
