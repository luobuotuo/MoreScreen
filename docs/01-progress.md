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

### 修正后的结论（11-15 优化后实测）

原判断"瓶颈 = CPU 侧逐像素转换"**不准确**。优化后实测证明：

| 版本 | 采集→编码出 p50 | 有效帧率 | 说明 |
|---|---|---|---|
| 原始 NV12 逐像素 + 阻塞写 stdin | 175 ms | 23 fps | 阻塞写入把排队时间算在前一帧，**低估了真实延迟** |
| BGRA 逐像素拷贝 | 190 ms | 20 fps | 没省（拷贝也是逐像素循环） |
| BGRA + 按行 memcpy + 写线程解耦 | 872 ms | 31 fps | 解耦后暴露真实积压深度 |
| BGRA + memcpy + 背压丢帧 + 24fps 目标 | **7.23 ms** | 19.7 fps | **延迟降低约 23 倍** |

**真正的认知**：
- 175ms/872ms 都是**积压深度**（输出滞后于输入），不是单帧计算耗时；
- 单帧真实处理耗时约 **7ms**（编码器跟得上时）；
- 度量口径必须区分"单帧耗时"与"积压深度"——AU 无法与送帧序列一一对应
  （ffmpeg 内部可能丢帧/重排），改用"最近一次送帧时刻"作基准；
- **背压丢帧是必须的**：输入 45fps 而编码器吞吐 ~20fps 时，丢帧保延迟
  否则延迟会随时间无限增长。

本轮优化（均在 `readback.rs` / `encode.rs` / `main.rs`）：
1. BGRA 输出改为**按行 memcpy**（`copy_nonoverlapping`），取代逐像素循环；
2. 亮度统计改为**只遍历采样点**，去掉每像素取模判断；
3. **写线程解耦**：独立线程阻塞写 ffmpeg stdin，主循环只做非阻塞 channel 发送；
4. **有界通道（容量 2）+ try_send 背压**：满了就丢帧并记录 `drop_reason`；
5. 目标帧率 30→24，匹配编码器实际吞吐。

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


## 七之前、虚拟屏采集闭环：已完成（11-15 10:44 实测）

### 点亮方式

手机 ToDesk 连接本机 → 在手机端会话里开启「隐私屏」→ 切换到「虚拟屏1」。
开启后系统新增 `Root\ToDeskIddDriver` 适配器（10 个设备节点），
切到虚拟屏后 IddCx 监视器才真正创建。

### 实测结果（`scripts/verify_virtual_screen.py` 自动完成）

| 指标 | 实测值 |
|---|---|
| 虚拟屏 | `MONITOR\MTT1337\{4d36e96e-...}\0005`（设备实例 ID，符合"不用下标"铁律） |
| 设备路径 | `\\.\DISPLAY76` |
| 分辨率 / 刷新率 | 800×600 @ 60 Hz |
| **位置** | **(1920, 0)** —— 主屏右侧，即真正的"扩展桌面"布局 |
| 采集帧数 | 358 帧 / 6 秒 |
| **平均帧间隔** | **16.66 ms ≈ 60.0 fps** |
| 首帧延迟 | 39.6 ms |
| 内容尺寸 | 800×600 |
| 内容自检 | 平均亮度 195.9 / 峰值 250 / **非黑占比 100.0%** |
| NV12 导出 | 720,000 字节（= 800×600×1.5，字节数精确一致） |
| 肉眼验证 | 真实桌面壁纸 + 完整任务栏 + 时间 10:44（`out/vscreen_preview.png`） |

### 结论

「虚拟显示器 → 指定屏采集 → 内容自检 → NV12 输出」整条链路**已在真实虚拟屏上跑通**，
且采集目标用**设备实例 ID**绑定（插拔/重排不错绑）。剩下的瓶颈仍是
CPU 侧 BGRA→NV12 转换（见下节实测），与虚拟屏本身无关。

> 说明：本次点亮用的是 ToDesk 自带的会话型 IddCx 驱动；自研 MttVDD 因签名策略
> 无法加载（见下节），产品化阶段需换成 Microsoft attestation 签名的驱动或自研。


## 七之前、虚拟屏采集闭环：已完成（11-15 10:44 实测）

### 点亮方式

手机 ToDesk 连接本机 → 在手机端会话里开启「隐私屏」→ 切换到「虚拟屏1」。
开启后系统新增 `Root\ToDeskIddDriver` 适配器（10 个设备节点），
切到虚拟屏后 IddCx 监视器才真正创建。

### 实测结果（`scripts/verify_virtual_screen.py` 自动完成）

| 指标 | 实测值 |
|---|---|
| 虚拟屏 | `MONITOR\MTT1337\{4d36e96e-...} 5`（设备实例 ID，符合"不用下标"铁律） |
| 设备路径 | `\.\DISPLAY76` |
| 分辨率 / 刷新率 | 800×600 @ 60 Hz |
| **位置** | **(1920, 0)** —— 主屏右侧，即真正的"扩展桌面"布局 |
| 采集帧数 | 358 帧 / 6 秒 |
| **平均帧间隔** | **16.66 ms ≈ 60.0 fps** |
| 首帧延迟 | 39.6 ms |
| 内容尺寸 | 800×600 |
| 内容自检 | 平均亮度 195.9 / 峰值 250 / **非黑占比 100.0%** |
| NV12 导出 | 720,000 字节（= 800×600×1.5，字节数精确一致） |
| 肉眼验证 | 真实桌面壁纸 + 完整任务栏 + 时间 10:44（`out/vscreen_preview.png`） |

### 结论

「虚拟显示器 → 指定屏采集 → 内容自检 → NV12 输出」整条链路**已在真实虚拟屏上跑通**，
且采集目标用**设备实例 ID**绑定（插拔/重排不错绑）。剩下的瓶颈仍是
CPU 侧 BGRA→NV12 转换（见下节实测），与虚拟屏本身无关。

> 说明：本次点亮用的是 ToDesk 自带的会话型 IddCx 驱动；自研 MttVDD 因签名策略
> 无法加载（见下节），产品化阶段需换成 Microsoft attestation 签名的驱动或自研。

## 七、VDD 设备节点：已建立（方法与结论）

### 关键认知：`pnputil` 不会创建 root-enumerated 设备节点

`MttVDD.inf` 用的是**软件枚举设备（SWD / root-enumerated）**：

```text
[Standard.NTamd64]
%DeviceName%=MyDevice_Install, Root\MttVDD
```

`pnputil /add-driver xxx.inf /install` 只做两件事：① 把驱动包发布到 DriverStore（生成 `oem223.inf`）；
② 给**已存在**的匹配设备安装驱动。它**不会凭空创建** root-enumerated 设备节点。
所以"驱动包装好了但 `ROOT\MttVDD` 查不到设备"是必然结果，不是安装失败。

### 建立节点的唯一正确路径：SetupAPI 四步

| 步骤 | API | 说明 |
|---|---|---|
| 1 | `SetupDiCreateDeviceInfoList` | 建设备信息集 |
| 2 | `SetupDiCreateDeviceInfo` + `SetupDiSetDeviceRegistryProperty(SPDRP_HARDWAREID)` | 创建设备并写硬件 ID |
| 3 | `SetupDiCallClassInstaller(DIF_REGISTERDEVICE)` | 注册设备 |
| 4 | `UpdateDriverForPlugAndPlayDevices` | 把驱动绑上去 |

等价于 WDK 的 `devcon install MttVDD.inf Root\MttVDD`，但**不需要安装 WDK**。
本项目实现于 `crates/host-win/src/vdd.rs`，CLI 为 `rs-host vdd-install`
（另附 `tools/create_vdd_device.ps1` 免编译版与 `tools/install_vdd_full.cmd` 一键脚本）。

### 踩到的两个真实坑

1. **`SetupDiCreateDeviceInfo` 的 `DeviceName` 不能带反斜杠**。
   微软文档规定 DeviceName 含 `\` 会被解释为**硬件 ID**，而 `Root\MttVDD` 在 SetupAPI 侧
   没有对应的已注册 setup class，直接返回 `0xE0000205 (SPAPI_E_NO_SUCH_REGCLASS)`。
   正确做法：DeviceName 传普通名（`MttVDD`），硬件 ID 另行写入（devcon 也是这么做的）。
2. **错误码要用 u32**。SetupAPI 的 HRESULT（0xE00002xx）超出 i32 范围，写成 i32 直接编译失败。

### 实测结果（已在本机跑通）

```text
管理员权限   : true
节点原已存在 : false
本次创建节点 : true
驱动绑定成功 : true
需要重启     : false
实例 ID      : ROOT\MTTVDD\0000
UMDF 驱动    : C:\windows\System32\drivers\UMDF\MttVDD.dll (251640 字节)
```

`rs-host displays` 中出现新条目（索引 5）：`\\.\DISPLAY36  Root\MttVDD`；
`pnputil /enum-devices /instanceid "ROOT\MTTVDD\0000"` 显示状态"已启动"、驱动 `oem223.inf`。

### 遗留阻塞：UMDF 驱动加载被系统拒绝

节点与驱动绑定都成功，但 **MttVDD.dll 没有被任何进程加载**（`tasklist /m MttVDD.dll` 无结果），
setupapi.dev.log 给出明确原因：

```text
!!!  dvi:  Device not started: Device has problem: 0x0a (CM_PROB_FAILED_START),
           problem status: 0x8007001f        (= ERROR_NOT_SUPPORTED)
```

`pnputil /restart-device "ROOT\MTTVDD\0000"` 之后仍失败（错误来自更早的安装段）。

对照事实：
- 本机 `System32\drivers\UMDF\` 下已运行着多个 IddCx 驱动
  （`OrayIddDriver.sys` 向日葵、`tdIdd.dll` ToDesk、`RdpIdd.dll`、`MiraDisp.dll`）→ **IddCx + UMDF 通路本身是通的**；
- `IndirectKmd.sys`（IddCx 内核桥接）存在；
- HVCI/内存完整性未启用；
- MttVDD.dll 签名状态 `Valid`，但签发者是 **SignPath Foundation（跨签名）**，
  而本机那些能工作的第三方 IddCx 驱动用的是 **Microsoft Windows Hardware Compatibility Publisher（attestation）**。

结论：**这不是"节点没建起来"，而是"驱动被驱动签名策略拒绝加载"**。
四条可选出路见 `docs/02-vdd-options.md`。

## 八、P1.2 WebRTC 最小闭环：本机全通，待真机验证（11-15）

### 已完成

| 组件 | 文件 | 说明 |
|---|---|---|
| 会话管理 | `crates/host-win/src/webrtc.rs` | 线程桥接（同步采集 → `sync_channel(2)` → tokio `current_thread` 运行时 → `write_sample`）；有界通道背压丢帧；RTP 时间戳 90kHz |
| API 构建 | `webrtc::build_api()` | **只注册一种 H.264**（42e01f + packetization-mode=1）+ 禁用 mDNS |
| 信令 | `serve.rs` Hub | 新增 `broadcast_text` / `poll_inbox` / **信令缓存与新客户端补发**；JSON 手工编解码（`json_escape` 复用 core） |
| 接收端 | `web/receiver/webrtc.html` | `RTCPeerConnection` + `<video srcObject>`；`playsinline`/`muted`；连接状态与 `getStats()` 展示；需用户点「开始接收」 |
| CLI | `rs-host stream-webrtc` | 全链路串联；`--fps` 可调 |
| 启动器 | `scripts/run_stream.cmd --webrtc` | 一键启动 |

### 三个关键实测结论（都是踩出来的）

1. **必须 `create_data_channel` + 等 `gathering_complete_promise`**：否则后台 ICE gathering 不被触发，
   `local_description` 不含候选，ICE 永远停在 Connecting。标准做法是把**含候选的 SDP** 直接传给对端，
   不依赖 trickle 转发。（依据 webrtc-rs 官方 `signal_pair` 流程）
2. **必须禁用 mDNS**：默认模式产生 `.local` 候选，局域网对端无法解析。
   `SettingEngine::set_ice_multicast_dns_mode(MulticastDnsMode::Disabled)`。
3. **必须只注册一种 H.264 codec**：`register_default_codecs()` 会注册 6 种 H264 变体，
   浏览器可能协商到 `42001f`（非 constrained）而编码器输出是 `42e01f`，profile 不匹配会花屏/拒收。

### 本机验证结果

- **回环验证**（`examples/webrtc_loopback.rs`，双 PeerConnection）：
  ICE Connected → `on_track` 收到视频 → **1830 个 RTP 包 / 2,129,220 字节**，H264Packet 可正确解析；
- **信令验证**（`scripts/signal_probe.py`）：offer 正常生成并广播（1151 字节，单一 H264 m-line + host candidate）；
  延迟连接也能收到补发的 offer。

### 待验证（需真机）

浏览器端出画面：手机打开 `http://<主机IP>:8788/webrtc.html` → 点「开始接收」→ 应看到实时桌面。
