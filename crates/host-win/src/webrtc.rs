//! WebRTC 会话管理（P1.2 · 路线 A）。
//!
//! # 架构：线程桥接（不把媒体管线异步化）
//!
//! ```text
//! 同步线程（现有链路，原样保留）          tokio 线程（current_thread 运行时）
//! ┌──────────────────────────┐          ┌─────────────────────────────┐
//! │ WGC 采集 → QSV 编码 → AU  │          │  RTCPeerConnection          │
//! │        │                  │          │        ▲                    │
//! │        ▼                  │          │        │ write_sample()     │
//! │  push_frame() ──sync_ch(2)──▶ frame_rx │        │                    │
//! │        │                  │          │  TrackLocalStaticSample     │
//! │  poll_signal() ◀──signal──┼──────────│  on_ice_candidate / gather  │
//! │        │                  │          │        │                    │
//! │  deliver(answer/cand) ────┼─────────▶│  set_remote_description     │
//! └──────────────────────────┘          └─────────────────────────────┘
//!            │                                      │
//!            └────────► WS Hub 广播给浏览器 ◄───────┘
//! ```
//!
//! # 关键设计
//!
//! - **有界通道（容量 2）+ `try_send` 背压**：编码器消费不过来时丢帧保延迟，
//!   与现有 WS 路径语义一致；
//! - **外部硬编 + 仅 RTP 打包**：`write_sample` 只做 packetization，
//!   H.264 编码由 QSV 完成（保住 7.57ms 优势）；
//! - **`ice_servers: []`**：首版只收 host candidate，不做 STUN/TURN；
//! - **RTP 时间戳 90kHz**：与 `clock_rate` 一致，按目标帧率递增。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::mpsc::{sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::serve::ClientId;

/// 抽取 SDP 里与"通道是否真的建立"相关的行，压成一行便于日志比对。
///
/// 重点看三样：`a=rtcp-mux`（RTCP 是否复用媒体端口，没有它 RTCP 走独立端口，
/// ICE 往往没为它建通路 → 对端 NACK 根本到不了宿主，重传永远是 0）、
/// `a=rtcp-fb`（重传/关键帧请求能力）、`a=ssrc`（流身份）。
fn sdp_summary(sdp: &str) -> String {
    sdp.lines()
        .filter(|l| {
            l.starts_with("m=")
                || l.starts_with("a=rtcp")
                || l.starts_with("a=ssrc")
                || l.starts_with("a=setup")
                || l.starts_with("a=mid")
                || l.starts_with("a=send")
                || l.starts_with("a=recv")
                || l.starts_with("a=inactive")
                || l.starts_with("a=rtpmap")
                || l.starts_with("a=fmtp")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// H.264 的 SDP fmtp 行：Constrained Baseline + packetization-mode=1（支持 STAP-A/FU-A）。
///
/// `42e01f` = Constrained Baseline，与 QSV 输出及华为平板诊断结果一致。
pub const H264_FMTP: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

/// 构建注册了默认 codec 的 WebRTC API。
///
/// **必须显式注册**：`APIBuilder::new().build()` 的默认 MediaEngine 不含任何 codec，
/// `create_offer` 会直接返回 `ErrSenderWithNoCodecs`。
/// `register_default_codecs()` 会注册 VP8/VP9/H264/AV1 等，其中 H264 的默认 fmtp 为
/// `profile-level-id=42001f;packetization-mode=1`，与我们的 H264 轨道一致。
pub fn build_api() -> Result<webrtc::api::API, String> {
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
    use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
    use webrtc::rtp_transceiver::RTCPFeedback;

    let mut media_engine = MediaEngine::default();

    // **只注册一种 H.264**：Constrained Baseline + packetization-mode=1（42e01f）。
    //
    // 不用 register_default_codecs()：它会注册 6 种 H264 变体（42001f/42e01f/640032
    // × packetization-mode 0/1）以及 VP8/VP9/AV1/HEVC 等。浏览器会从列表里挑一个，
    // 很可能挑到 `42001f`（非 constrained 的 Baseline），
    // 而我们的 QSV 编码器输出是 `42e01f`（Constrained Baseline）——
    // profile 不匹配会导致浏览器拒收或花屏。
    // 只注册一种可让 SDP 协商结果唯一、必然匹配编码器输出。
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/H264".to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: H264_FMTP.to_owned(),
                    // 必须声明 RTCP 反馈，否则 SDP 里一个 a=rtcp-fb 都没有：
                    //  - 没有 nack  → 丢包无法重传，丢一个包就永久缺失，只能等下一个关键帧；
                    //  - 没有 nack pli → 浏览器无法主动请求关键帧，新接入/丢包后要干等 GOP。
                    // 之前的 0.49% 丢包和 1271ms 首帧都与此有关。
                    rtcp_feedback: vec![
                        RTCPFeedback {
                            typ: "nack".to_owned(),
                            parameter: String::new(),
                        },
                        RTCPFeedback {
                            typ: "nack".to_owned(),
                            parameter: "pli".to_owned(),
                        },
                    ],
                },
                payload_type: 102,
                ..Default::default()
            },
            RTPCodecType::Video,
        )
        .map_err(|e| format!("register_codec(H264) 失败: {e}"))?;

    // RTX 重传流（RFC 4588）：必须用 `apt=` 关联到 H264 的 payload_type。
    // 没有它，SDP 里就没有 `a=rtpmap rtx` / `a=ssrc-group:FID`，
    // 浏览器收到 NACK 响应时只认 RTX 流，原 SSRC 重发的同序号包会被当重复包丢弃
    // —— 这就是「NACK 发了 326 次、重传 0 次、丢帧 34%」的根因。
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/rtx".to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "apt=102".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
                ..Default::default()
            },
            RTPCodecType::Video,
        )
        .map_err(|e| format!("register_codec(rtx) 失败: {e}"))?;

    // **必须显式注册拦截器**。APIBuilder 默认给的是空的 `Registry::new()`：
    // SDP 里虽然声明了 nack，但没有任何 interceptor 去处理 NACK 请求，
    // 于是丢一个包就等于丢一整帧 —— 实测 1.2% 的 RTP 包丢包被放大成
    // 40% 掉帧（宿主 20fps 稳定送出，浏览器只解 12fps）。
    // 这一组还会带上 RTCP 报告收发与 TWCC 接收端反馈。
    use webrtc::api::interceptor_registry::register_default_interceptors;
    use webrtc::interceptor::registry::Registry;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)
        .map_err(|e| format!("注册默认拦截器失败: {e}"))?;

    // 禁用 mDNS 候选：默认模式会产生 `.local` 主机名候选，
    // 局域网对端（尤其本机回环与某些设备）无法解析，导致 ICE 永远停在 Connecting。
    // 局域网场景不需要这层隐私保护，直接用真实 IP 更可靠。
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
    // **RTX 的总开关**。`RTPSender::add_encoding_internal` 里
    // `create_rtx_stream = self.enable_rtx && media_engine 有 rtx codec`，
    // 而 `enable_rtx` 就是这个 setting。默认 false → add_track 根本不建 RTX 流，
    // 于是 SDP 里只有 a=rtpmap rtx 却没有 a=ssrc-group:FID，浏览器不认重传。
    //
    // 实测结论（2026-11-21）：必须开启。
    // webrtc-rs 0.12 的 nack::Responder 重传时直接写**原始 RTP 包**（原 SSRC / 原 PT），
    // 浏览器对这种重传一律丢弃（retx 恒为 0）。关闭 RTX 让浏览器「只能收原 SSRC 重传」
    // 也不行（实测仍 0/186）。正确做法是 RFC 4588：重发包要改写成 RTX SSRC + RTX PT
    // + 独立序列号，payload 前置原序列号 —— 该改写已在 vendor/interceptor 中实现。
    setting_engine.enable_sender_rtx(true);
    // **收集 loopback（127.0.0.1）host 候选**。
    //
    // 默认 webrtc-rs 只发真实网卡地址（如 192.168.0.104）。当浏览器与宿主
    // 在**同一台机器**上时，包要经过本机网卡地址回环，会被系统代理（TUN 模式）
    // 或防火墙规则干扰，表现为 ICE 能连上、DTLS 握手始终失败（连接 failed）。
    // 打开 loopback 候选后，本机客户端可直接走 127.0.0.1 建立，绕开这条路径。
    //
    // 对局域网其它设备无副作用：它们拿到 127.0.0.1 候选会快速探测失败并忽略。
    setting_engine.set_include_loopback_candidate(true);

    Ok(APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build())
}

/// 会话 → 主线程 的信令输出（主线程负责通过 WS 广播给浏览器）。
#[derive(Debug, Clone)]
pub enum SignalOut {
    /// offer 已生成（SDP 文本）
    Offer { sdp: String },
    /// 本地 ICE candidate
    Candidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    /// 连接状态变化（用于日志/UI）
    State(String),
    /// DataChannel 收到的客户端消息（输入事件 / 设备信息 / 网络质量上报）。
    ///
    /// 走 DataChannel 而不是 WS：延迟更低、与媒体同一传输路径，
    /// 且不需要在 WS 上再区分消息来源客户端。
    Ctl(String),
}

/// 主线程 → 会话 的信令输入（浏览器回传的内容）。
#[derive(Debug, Clone)]
pub enum SignalIn {
    Answer { sdp: String },
    Candidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    /// 结束本会话（客户端断开或服务停止）。
    ///
    /// 不再有 `Restart`：每个客户端一个独立 PeerConnection，
    /// 新客户端接入只需新建会话，不必打断任何已有会话。
    Shutdown,
    /// 通过 DataChannel 向客户端下发一条文本（状态/配置/回执）。
    Ctl(String),
}

/// 单个客户端会话的内部句柄。
struct SessionHandle {
    /// 送帧通道（同步线程 → 该会话的 tokio 线程），容量 2，满则丢帧
    frame_tx: SyncSender<Vec<u8>>,
    /// 信令输入（主线程投递）
    signal_in_tx: tokio::sync::mpsc::Sender<SignalIn>,
}

/// 多客户端 WebRTC 会话管理器（同步线程持有）。
///
/// **每个客户端一个独立 PeerConnection**：新客户端接入只新建会话，
/// 不触碰已有会话，因此不会出现旧的"新客户端把正在看的客户端踢掉"的问题
/// （那是单 pc 共享会话架构的必然结果）。
///
/// 视频帧只编码一次，通过有界通道分发给各会话；每个会话独立打包成 RTP 发出。
pub struct WebRtcHub {
    fps: u32,
    sessions: Arc<Mutex<HashMap<ClientId, SessionHandle>>>,
    /// 汇总所有会话的信令输出，带上来源客户端 ID。
    /// `Receiver` 不是 Sync，而 Hub 需跨线程共享（采集线程 push_frame、
    /// 信令泵线程 poll_signal），故用 Mutex 包一层。
    signal_out_rx: Mutex<Receiver<(ClientId, SignalOut)>>,
    signal_out_tx: Sender<(ClientId, SignalOut)>,
}

impl WebRtcHub {
    /// 创建会话管理器。`fps` 用于各会话计算 RTP 时间戳增量。
    pub fn new(fps: u32) -> Self {
        let (signal_out_tx, signal_out_rx) = std::sync::mpsc::channel::<(ClientId, SignalOut)>();
        Self {
            fps,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            signal_out_rx: Mutex::new(signal_out_rx),
            signal_out_tx,
        }
    }

    /// 为新接入的客户端建立独立会话。
    pub fn join(&self, id: ClientId) -> Result<(), String> {
        if self.sessions.lock().unwrap().contains_key(&id) {
            return Ok(()); // 幂等：同一客户端重复登记不重建
        }
        let fps = self.fps;
        let (frame_tx, frame_rx) = sync_channel::<Vec<u8>>(2);
        let (signal_in_tx, signal_in_rx) = tokio::sync::mpsc::channel::<SignalIn>(32);
        // 每个会话有独立的 SignalOut 通道（不带 id），由转发线程补上 id 汇入总通道。
        // 这样 run_once 内部完全不需要感知 client_id，改动面最小。
        let (sess_out_tx, sess_out_rx) = std::sync::mpsc::channel::<SignalOut>();
        let hub_out = self.signal_out_tx.clone();
        thread::Builder::new()
            .name(format!("webrtc-fwd-{id}"))
            .spawn(move || {
                for s in sess_out_rx {
                    if hub_out.send((id, s)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| format!("spawn 信令转发线程失败: {e}"))?;

        thread::Builder::new()
            .name(format!("webrtc-{id}"))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = sess_out_tx
                            .send(SignalOut::State(format!("tokio 运行时创建失败: {e}")));
                        return;
                    }
                };
                let mut rx = signal_in_rx;
                rt.block_on(async move {
                    if let Err(e) = run_once(fps, &frame_rx, &sess_out_tx, &mut rx).await {
                        let _ = sess_out_tx.send(SignalOut::State(format!("会话错误: {e}")));
                    }
                });
            })
            .map_err(|e| format!("spawn webrtc 会话线程失败: {e}"))?;

        self.sessions
            .lock()
            .unwrap()
            .insert(id, SessionHandle { frame_tx, signal_in_tx });
        Ok(())
    }

    /// 客户端断开：移除并结束它的会话。其他会话不受影响。
    pub fn leave(&self, id: ClientId) {
        if let Some(s) = self.sessions.lock().unwrap().remove(&id) {
            let _ = s.signal_in_tx.blocking_send(SignalIn::Shutdown);
        }
    }

    /// 把一帧 H.264 访问单元分发给所有活跃会话。返回成功入队的会话数。
    ///
    /// 用 `try_send`：某个会话消费不过来时只丢它自己的帧，不阻塞采集线程，
    /// 也不影响其他会话。
    pub fn push_frame(&self, au: &[u8]) -> usize {
        let sessions = self.sessions.lock().unwrap();
        let mut ok = 0;
        for s in sessions.values() {
            if s.frame_tx.try_send(au.to_vec()).is_ok() {
                ok += 1;
            }
        }
        ok
    }

    /// 把浏览器回传的信令投递给指定客户端的会话。
    pub fn deliver(&self, id: ClientId, sig: SignalIn) {
        if let Some(s) = self.sessions.lock().unwrap().get(&id) {
            let _ = s.signal_in_tx.blocking_send(sig);
        }
    }

    /// 非阻塞取一条会话输出信令（含来源客户端 ID）。
    pub fn poll_signal(&self) -> Option<(ClientId, SignalOut)> {
        self.signal_out_rx.lock().ok()?.try_recv().ok()
    }

    /// 当前活跃会话数。
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// 关闭所有会话（服务退出时调用）。
    pub fn close_all(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock().unwrap());
        for (_, s) in sessions {
            let _ = s.signal_in_tx.blocking_send(SignalIn::Shutdown);
        }
    }
}

/// 一次 pc 生命周期：建 pc → 发 offer → 收发信令与帧，直到 Shutdown。
///
/// 会话寿命 = 客户端连接寿命，不再有"重建轮次"。
async fn run_once(
    fps: u32,
    frame_rx: &Receiver<Vec<u8>>,
    signal_out_tx: &Sender<SignalOut>,
    signal_in_rx: &mut tokio::sync::mpsc::Receiver<SignalIn>,
) -> Result<(), String> {
    // 会话计时起点：用来定位"客户端点开始 → 首帧上屏"这 1.2 秒花在哪一段
    let t0 = std::time::Instant::now();

    // 注意：这里**不能**盲目 try_recv 丢弃残留信令。
    // 竞态链条：run_session 外层在 run_once 返回 Restart 后立即重建，
    // 若此时主线程信令泵又投递了新的 Restart/answer，这里 try_recv 会把它们
    // 吞掉或与真正的 answer 时序错乱 → answer 在 stable 状态被应用 →
    // "invalid proposed signaling state transition" → 会话死掉、客户端永远等 offer。
    // 正确做法：让 select 循环里的 Answer/Restart 分支自己处理时序（见下）。

    // ---- 1. 建 PeerConnection（空 ICE 服务器，只收 host candidate）----
    let api = build_api()?;
    let config = RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    };
    let pc = api
        .new_peer_connection(config)
        .await
        .map_err(|e| format!("new_peer_connection 失败: {e}"))?;

    // ---- 2. 建 H.264 视频轨并加入连接 ----
    // ⚠ 这里的 rtcp_feedback 不能为空。
    // webrtc-rs 用 **track 自己的 codec** 构造 StreamInfo（rtp_sender/mod.rs:480-488），
    // 而 interceptor 的 nack::Responder 在 bind_local_stream 里靠
    // `stream_support_nack(info)` 决定要不要建重传缓存。
    // 为空 → 不建缓存 → 浏览器发来的 NACK 全部静默丢弃
    // （实测 nack 累计 900+、接收端重传恒为 0，且无任何报错）。
    // 注意：注册给 media_engine 的那份只影响 SDP 协商，管不到这条路径。
    let codec = RTCRtpCodecCapability {
        mime_type: "video/H264".to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: H264_FMTP.to_owned(),
        rtcp_feedback: vec![
            webrtc::rtp_transceiver::RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: String::new(),
            },
            webrtc::rtp_transceiver::RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "pli".to_owned(),
            },
        ],
    };
    let track = Arc::new(TrackLocalStaticSample::new(
        codec,
        "video".to_owned(),
        "morescreen".to_owned(),
    ));
    // RTX 流由 `enable_sender_rtx(true)` + add_track 自动建立。
    // **不要手动调 sender.send()**：它会在 offer 生成前读取 transceiver 的 mid（此时为 None），
    // 且把 has_sent 置 true，导致 set_local_description 时 start_rtp_senders 跳过它、
    // 或进入错误状态——实测 answer 一到 pc 立即 Closed。
    // 正确流程：add_track 后什么都不做，等 set_local_description 自动 start_rtp_senders。
    pc.add_track(track.clone())
        .await
        .map_err(|e| format!("add_track 失败: {e}"))?;
    // 拿到 sender 引用，供下面的 read_rtcp 循环用
    let sender = pc
        .get_senders()
        .await
        .into_iter()
        .next()
        .ok_or_else(|| "add_track 后无 sender".to_string())?;

    // **必须主动读 RTCP**：`PeerConnection` 内部不会替我们调 `read_rtcp()`
    // （已 grep 确认 peer_connection/*.rs 里没有任何调用点）。
    // 不读的话，浏览器发来的 NACK 永远停在 socket 缓冲区里，
    // `nack::Responder` 收不到 → 重传永远 0，且失败时是静默 return、无任何日志。
    // 会话关闭后 read_rtcp 会因 stop_called 返回 Err，循环自然退出。
    {
        let reader = sender.clone();
        tokio::spawn(async move {
            // 累计 NACK 计数，每 60 秒汇总一次（仅在非零时打印），
            // 用于弱网测试观测：NACK 有量而接收端「重传」仍为 0 即代表重传链路异常。
            let mut n_nack: u64 = 0;
            let mut last = std::time::Instant::now();
            loop {
                match reader.read_rtcp().await {
                    // interceptor 已在读取过程中完成 NACK 匹配与重传，
                    // 这里只负责持续驱动读取，否则 RTCP 会停在 socket 缓冲区。
                    Ok((pkts, _attrs)) => {
                        for p in pkts.iter() {
                            let d = format!("{p:?}");
                            let t = d
                                .split(|c: char| c == '(' || c == '{' || c == ' ')
                                .next()
                                .unwrap_or("?");
                            if t == "TransportLayerNack" {
                                n_nack += 1;
                            }
                        }
                        if last.elapsed().as_secs() >= 60 {
                            if n_nack > 0 {
                                eprintln!("[rtcp] 累计 NACK {n_nack}");
                            }
                            last = std::time::Instant::now();
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // ---- 3. 状态与 ICE candidate 上报 ----
    {
        let tx = signal_out_tx.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let _ = tx.send(SignalOut::State(format!("{state:?}")));
            Box::pin(async {}) as Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
        }));
    }
    {
        let tx = signal_out_tx.clone();
        pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            if let Some(c) = c {
                if let Ok(init) = c.to_json() {
                    let _ = tx.send(SignalOut::Candidate {
                        candidate: init.candidate,
                        sdp_mid: init.sdp_mid,
                        sdp_mline_index: init.sdp_mline_index,
                    });
                }
            }
            Box::pin(async {}) as Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
        }));
    }

    // ---- 4. 生成 offer（等 ICE 候选收集完成，SDP 内含候选）----
    //
    // 两个必须（来自 webrtc-rs 官方 signal_pair 流程，本机回环实测）：
    //  a) 必须先 create_data_channel，否则后台 ICE gathering 不会被触发；
    //  b) 必须等 gathering_complete_promise，local_description 才会包含候选。
    // 这样做的好处：浏览器收到含候选的 offer 即可直连，不依赖 trickle。
    // 数据通道 `ctl`：承载输入回传、设备信息与网络质量上报。
    // 必须在这里创建（否则 ICE gathering 不会被触发，见下方注释）。
    let ctl = pc
        .create_data_channel("ctl", None)
        .await
        .map_err(|e| format!("create_data_channel 失败: {e}"))?;
    {
        let tx = signal_out_tx.clone();
        ctl.on_message(Box::new(move |msg: DataChannelMessage| {
            let text = String::from_utf8_lossy(&msg.data).to_string();
            let _ = tx.send(SignalOut::Ctl(text));
            Box::pin(async {}) as Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>
        }));
    }
    let offer = pc
        .create_offer(None)
        .await
        .map_err(|e| format!("create_offer 失败: {e}"))?;
    let mut gathering = pc.gathering_complete_promise().await;
    pc.set_local_description(offer)
        .await
        .map_err(|e| format!("set_local_description 失败: {e}"))?;
    let _ = gathering.recv().await;
    let offer_full = pc
        .local_description()
        .await
        .ok_or_else(|| "local_description 为空".to_string())?;
    let cand_n = offer_full
        .sdp
        .lines()
        .filter(|l| l.starts_with("a=candidate:"))
        .count();
    // 用来确认 nack/pli 声明真的进了 SDP（否则浏览器不会发重传/关键帧请求）
    let fb_n = offer_full
        .sdp
        .lines()
        .filter(|l| l.starts_with("a=rtcp-fb:"))
        .count();
    let _ = signal_out_tx.send(SignalOut::State(format!(
        "offer 已生成：{} 字节，含 {cand_n} 个 ICE 候选 / {fb_n} 条 rtcp-fb（含等 ICE 收集，耗时 {} ms）",
        offer_full.sdp.len(),
        t0.elapsed().as_millis()
    )));
    let _ = signal_out_tx.send(SignalOut::State(format!(
        "offer SDP 摘要: {}",
        sdp_summary(&offer_full.sdp)
    )));
    let _ = signal_out_tx.send(SignalOut::Offer { sdp: offer_full.sdp });

    // ---- 5. 主循环：绝对节拍发送 + select 信令 ----
    //
    // 发送必须用**绝对节拍器**（tokio interval 按 wall-clock 对齐），不能"来一帧发一帧"：
    //   a) "来一帧发一帧"时 write_sample 的耗时（~8ms）会叠加进帧间隔，实际帧率低于目标；
    //   b) 上游偶发两帧紧邻时（编码器输出积压），第二帧 duration≈0，
    //      RTP 时间戳与前一帧几乎相同，浏览器按重复帧丢弃——
    //      实测丢帧比例恒定 70%，与帧率（20fps→14、10fps→7）、丢包（0% 时同样）都无关，
    //      只能用这种系统性机制解释。
    // 节拍对齐后：帧间隔恒等于 min_interval，duration 也恒等于 min_interval，
    // 时间戳与真实到达严格同步；上游积压时取"最新一帧"（丢旧保新，延迟优先）。
    let fps = fps.max(1) as u64;
    let ts_step = 90000 / fps; // RTP 时间戳增量（90kHz / fps）
    let mut packet_ts: u32 = 0;
    let min_interval = Duration::from_micros(1_000_000 / fps);
    let mut sent: u64 = 0;
    let mut ticker = tokio::time::interval(min_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut diag_ticker = tokio::time::interval(Duration::from_secs(3));
    // 本会话是否已接受过 answer（一个 pc 只能协商一次，多余 answer 忽略）
    let mut answered = false;

    loop {
        tokio::select! {
            // 5a. 信令：到达即处理，不等节拍（answer 延迟直接影响首帧时间）
            maybe_sig = signal_in_rx.recv() => {
                match maybe_sig {
                    Some(sig) => match sig {
                        SignalIn::Answer { sdp } => {
                            // **answer 代际守卫**：一个 pc 只能接受一次 answer。
                            // 第二个 answer 会触发 "invalid proposed signaling state
                            // transition from stable applying remote answer" 并让会话死掉。
                            // 多客户端/重连时多余的 answer 直接忽略，不再报错。
                            if answered {
                                let _ = signal_out_tx.send(SignalOut::State(
                                    "忽略重复 answer（本会话已协商）".into(),
                                ));
                                continue;
                            }
                            answered = true;
                            let setup = sdp
                                .lines()
                                .find(|l| l.trim_start().starts_with("a=setup:"))
                                .map(|l| l.trim().to_string())
                                .unwrap_or_else(|| "a=setup:(缺)".to_string());
                            let _ = signal_out_tx.send(SignalOut::State(format!(
                                "收到 answer：{} 字节，{setup}（+{} ms）",
                                sdp.len(),
                                t0.elapsed().as_millis()
                            )));
                            let _ = signal_out_tx.send(SignalOut::State(format!(
                                "answer SDP 摘要: {}",
                                sdp_summary(&sdp)
                            )));
                            let desc = RTCSessionDescription::answer(sdp.clone())
                                .map_err(|e| format!("answer 描述无效: {e}"))?;
                            pc.set_remote_description(desc)
                                .await
                                .map_err(|e| format!("set_remote_description 失败: {e}"))?;
                            let _ = signal_out_tx.send(SignalOut::State("远端描述已设置".into()));
                        }
                        SignalIn::Candidate {
                            candidate,
                            sdp_mid,
                            sdp_mline_index,
                        } => {
                            let init = RTCIceCandidateInit {
                                candidate,
                                sdp_mid,
                                sdp_mline_index,
                                username_fragment: None,
                            };
                            if let Err(e) = pc.add_ice_candidate(init).await {
                                let _ = signal_out_tx
                                    .send(SignalOut::State(format!("add_ice_candidate: {e}")));
                            }
                        }
                        SignalIn::Shutdown => {
                            let _ = pc.close().await;
                            return Ok(());
                        }
                        SignalIn::Ctl(text) => {
                            // 宿主 → 客户端：状态/配置/回执（send_text 接受 Into<String>）
                            let _ = ctl.send_text(text).await;
                        }
                    },
                    None => {
                        // 通道断开（会话被移除或进程退出）
                        let _ = pc.close().await;
                        return Ok(());
                    }
                }
            }

            // 5b. 节拍到点：取一帧发送（不排空）
            _ = ticker.tick() => {
                // 每拍只取一帧，不排空队列：
                // 排空会制造"0 帧/2 帧"的 micro-burst，节拍器看似 50ms 均匀，
                // 实际取帧节奏在 0ms 和 100ms 之间跳，浏览器 jitterBuffer 因此
                // 判定"迟到 120ms"并持续丢帧。队列容量 2 本身就会丢旧保新，
                // 这里只需要消费一帧，让背压自然生效。
                match frame_rx.try_recv() {
                    Ok(au) => {
                        let sample = webrtc::media::Sample {
                            data: Bytes::from(au),
                            timestamp: std::time::SystemTime::now(),
                            duration: min_interval,
                            packet_timestamp: packet_ts,
                            prev_dropped_packets: 0,
                            prev_padding_packets: 0,
                        };
                        packet_ts = packet_ts.wrapping_add(ts_step as u32);
                        match track.write_sample(&sample).await {
                            Ok(()) => {
                                sent += 1;
                                if sent == 1 {
                                    let _ = signal_out_tx.send(SignalOut::State(format!(
                                        "首帧写入 SRTP：+{} ms（从会话重建起算）",
                                        t0.elapsed().as_millis()
                                    )));
                                } else if sent % 120 == 0 {
                                    let _ = signal_out_tx.send(SignalOut::State(format!(
                                        "已写入 {sent} 帧（SRTP 发送中）"
                                    )));
                                }
                            }
                            Err(e) => {
                                let _ = signal_out_tx
                                    .send(SignalOut::State(format!("write_sample: {e}")));
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        // 无帧可发：上游供给不足。tick 照走，不补帧。
                    }
                    Err(TryRecvError::Disconnected) => {
                        let _ = pc.close().await;
                        return Ok(());
                    }
                }
            }

            // 5c. 诊断：3 秒一次
            _ = diag_ticker.tick() => {
                let _ = signal_out_tx.send(SignalOut::State(format!(
                    "诊断 conn={:?} ice={:?} 已写入 {sent} 帧（节拍 {} ms）",
                    pc.connection_state(),
                    pc.ice_connection_state(),
                    min_interval.as_millis()
                )));
            }
        }
    }
}
