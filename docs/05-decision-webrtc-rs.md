# 技术选型变更：str0m → webrtc-rs

> 日期：2026-11-15 12:4x · 决策人：项目负责人 · 状态：已生效 + P1.1 验证通过

## 一、变更内容

| 项 | 原决策 | 新决策 |
|---|---|---|
| WebRTC 实现 | str0m（纯 Rust，同步 API） | **webrtc-rs**（pion/webrtc 的 Rust 移植，tokio async） |

## 二、变更原因

**后续确定要做公网远程**，需要 STUN/TURN 穿透能力。webrtc-rs 在这块更成熟、可参考的部署经验更多（pion 生态案例可直接借鉴）。

## 三、P1.1 验证结果（2026-11-15 实测，全部通过）

| # | 验证项 | 结果 | 证据 |
|---|---|---|---|
| 1 | **webrtc-rs 在本机构建**（windows-gnu + rsproxy） | ✅ 通过 | webrtc 0.12.0 编译 2m21s；依赖链含 `turn 0.9.0`（TURN 客户端，公网远程必需）、`webrtc-ice`、`webrtc-dtls`、`webrtc-srtp`、`webrtc-sctp` |
| 2 | **外部 H.264 样本兼容性** | ✅ 通过 | 用 `rs-host` QSV 编码器真实输出（70,861 B，SPS+PPS+70822B IDR）测试：`H264Payloader` 产出 STAP-A 33B（= 1+2+24 SPS+2+4 PPS，**SPS/PPS 自动聚合**）+ 60 个 FU-A 分片；depacketize 往返还原 NAL 数 3==3 |
| 3 | **线程桥接可行性** | ✅ 通过 | 同步生产线程 + 有界 channel(容量2) + `tokio::runtime::Builder::new_current_thread()` 消费：100 帧全收发，背压语义与现有链路一致 |
| 4 | **H.264 profile 匹配** | ✅ 通过 | QSV 输出 Constrained Baseline = `42e01f`；华为平板诊断页显示支持 `42e01f/4d001f/64001f` 三种，**无需转码** |

## 四、两个超出预期的收获（工作量下降）

### 1. H.264 RTP packetization **完全不需要自研**

`rtp` crate 的 `H264Payloader` 已实现：

- **STAP-A 聚合**：自动把 SPS/PPS 打包进 STAP-A，紧跟 VCL NAL 发送 → 顺带解决了"客户端中途接入拿不到参数集"这个我们之前自研攻克过的问题
- **FU-A 分片**：NAL 超过 MTU 自动分片，START/END 标记正确
- **AUD/FILLER 自动丢弃**
- 直接接受 **Annex B 输入**——正是我们 QSV 编码器的输出格式

**影响**：原计划中的 `rtp_h264.rs`（自研 packetization）**取消**；`encode.rs` 中约 200 行的 AU 聚合 / 参数集缓存补齐 / `first_mb_in_slice` 解析逻辑在 WebRTC 路线下不再需要（暂保留以不影响现有 WS 路径，P1.2 接入后清理）。

### 2. 无需把媒体管线异步化

原评估的"代价 1：必须异步化改造"被证伪。**线程桥接即可**：

```
同步线程：WGC 采集 → QSV 编码 → AU        （现有代码原样保留）
                ↓ sync_channel(2) 背压
tokio 线程：current_thread 运行时 → track.write_sample().await
```

现有 `capture_stream` / `encode.rs` 的 7.57ms 调优成果**不受影响**。

## 五、保留的代价（仍存在但已缩小）

| # | 代价 | 现状 |
|---|---|---|
| 1 | 依赖体量上升 | 接受（换来 TURN 能力与 pion 生态） |
| 2 | tokio 运行时常驻 | 用 `current_thread` 而非 `multi_thread`，开销最小 |
| 3 | ~~媒体管线异步化~~ | **已不需要**（线程桥接解决） |

## 六、落地资产

| 文件 | 说明 |
|---|---|
| `crates/host-win/Cargo.toml` | 新增 `webrtc 0.12` / `rtp 0.12` / `bytes` / `tokio` |
| `crates/host-win/src/h264probe.rs` | 验证工具：`split_annexb` / `nal_type_name` / `roundtrip`（含 3 项单测） |
| `crates/host-win/examples/verify_h264.rs` | H.264 payloader 兼容性验证程序（可用任意 AU 文件测试） |
| `crates/host-win/examples/bridge_thread.rs` | 线程桥接验证程序 |
| 测试 | host-win 6 → **9 passed / 0 failed** |

复现命令：

```bash
cargo run --release -p remote-screen-host-win --example verify_h264 -- out/first_au.h264 1200
cargo run --release -p remote-screen-host-win --example bridge_thread
```

## 七、影响面清单

| 文件/模块 | 状态 |
|---|---|
| `docs/04-roadmap.md` | ✅ 已更新（P1.1 完成、取消自研 packetization、风险表调整） |
| `README.md` | ✅ 已更新 |
| `crates/host-win/Cargo.toml` | ✅ 已加依赖 |
| `crates/host-win/src/h264probe.rs` | ✅ 新增 |
| `crates/host-win/examples/*` | ✅ 新增 2 个验证程序 |
| `.workbuddy/memory/MEMORY.md` | ✅ 已记录 |
| `crates/host-win/src/capture.rs` / `encode.rs` | ⬜ P1.2/P1.3 接入时改（加发送端，现有逻辑不动） |
| `web/receiver/index.html` | ⬜ P1.2 换 `<video srcObject>` |

## 八、下一步

P1.2 最小闭环：信令层加 `offer`/`answer`/`candidate` → webrtc-rs 建连 → 接收端换 `<video srcObject>`。
