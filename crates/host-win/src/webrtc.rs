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

use std::pin::Pin;
use std::sync::mpsc::{sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

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
    setting_engine.enable_sender_rtx(true);

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
    /// 重建会话：有新的接收端接入时由主线程投递。
    ///
    /// 一个 PeerConnection 只能完成一次 offer/answer 协商——接收端关掉页面再连，
    /// 复用旧的 pc/offer 会让 ICE 永远停在 checking（实测 14:29 那次就是这样）。
    /// 因此新客户端必须重建 pc 并重新生成 offer。
    Restart,
    Shutdown,
}

/// WebRTC 会话句柄（同步线程持有）。
pub struct WebRtcHandle {
    /// 送帧通道（同步线程 → tokio 线程），容量 2，满则丢帧
    frame_tx: SyncSender<Vec<u8>>,
    /// 信令输出（主线程轮询）
    signal_out_rx: Receiver<SignalOut>,
    /// 信令输入（主线程投递）
    signal_in_tx: tokio::sync::mpsc::Sender<SignalIn>,
}

impl WebRtcHandle {
    /// 启动 WebRTC 会话（spawn tokio 线程）。
    ///
    /// `fps`：目标帧率，用于计算 RTP 时间戳增量与 sample duration。
    pub fn spawn(fps: u32) -> Result<Self, String> {
        let (frame_tx, frame_rx) = sync_channel::<Vec<u8>>(2);
        let (signal_out_tx, signal_out_rx) = std::sync::mpsc::channel::<SignalOut>();
        // 信令输入走 tokio mpsc：主循环要用 select! 同时等信令和发送节拍，
        // std mpsc 的 recv() 不是 Future 进不了 select；同步侧用 blocking_send 投递。
        let (signal_in_tx, signal_in_rx) = tokio::sync::mpsc::channel::<SignalIn>(32);

        thread::Builder::new()
            .name("webrtc-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = signal_out_tx.send(SignalOut::State(format!(
                            "tokio 运行时创建失败: {e}"
                        )));
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = run_session(fps, frame_rx, signal_out_tx.clone(), signal_in_rx).await
                    {
                        let _ = signal_out_tx.send(SignalOut::State(format!("会话错误: {e}")));
                    }
                });
            })
            .map_err(|e| format!("spawn webrtc 线程失败: {e}"))?;

        Ok(Self {
            frame_tx,
            signal_out_rx,
            signal_in_tx,
        })
    }

    /// 送入一帧 H.264 访问单元（Annex B）。返回 `false` 表示因背压被丢弃。
    pub fn push_frame(&self, au: &[u8]) -> bool {
        match self.frame_tx.try_send(au.to_vec()) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => false,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    /// 非阻塞取一条待发送的信令。
    pub fn poll_signal(&self) -> Option<SignalOut> {
        self.signal_out_rx.try_recv().ok()
    }

    /// 投递浏览器回传的信令（在同步线程调用）。
    pub fn deliver(&self, sig: SignalIn) {
        let _ = self.signal_in_tx.blocking_send(sig);
    }

    /// 请求重建会话（有新接收端接入时调用）。
    pub fn restart(&self) {
        let _ = self.signal_in_tx.blocking_send(SignalIn::Restart);
    }

    /// 关闭会话。
    pub fn close(&self) {
        let _ = self.signal_in_tx.blocking_send(SignalIn::Shutdown);
    }
}

/// 一次 pc 生命周期的结束原因。
enum SessionEnd {
    /// 收到 Shutdown，或采集端已关闭
    Shutdown,
    /// 需要重建（有新接收端接入）
    Restart,
}

/// tokio 线程内的会话主循环：外层负责重建，内层跑一次完整的 pc 生命周期。
///
/// 拆两层的原因：PeerConnection 完成一次 offer/answer 后不能再接受新的 answer
/// （会报 `invalid proposed signaling state transition from stable applying remote answer`），
/// 而接收端随时可能关页面重连。只有换一个全新的 pc 才能重新协商。
async fn run_session(
    fps: u32,
    frame_rx: Receiver<Vec<u8>>,
    signal_out_tx: Sender<SignalOut>,
    mut signal_in_rx: tokio::sync::mpsc::Receiver<SignalIn>,
) -> Result<(), String> {
    let mut round: u32 = 0;
    loop {
        round += 1;
        match run_once(fps, &frame_rx, &signal_out_tx, &mut signal_in_rx).await? {
            SessionEnd::Shutdown => return Ok(()),
            SessionEnd::Restart => {
                let _ = signal_out_tx.send(SignalOut::State(format!(
                    "接收端重连：重建 PeerConnection（第 {round} 轮）并重新生成 offer"
                )));
                // 清掉通道里积压的 Restart/Shutdown（主线程可能连投了多次），
                // 否则新会话一进 select 循环就被旧的 Restart 打断，又立刻退出，
                // 表现为"检测到第 N 次接入"却始终不产 offer（客户端永远等 offer）。
                // Answer/Candidate 属于旧会话，一并丢弃（新 offer 会触发新 answer）。
                while signal_in_rx.try_recv().is_ok() {}
            }
        }
    }
}

/// 一次 pc 生命周期：建 pc → 发 offer → 收发信令与帧，直到 Shutdown / Restart。
async fn run_once(
    fps: u32,
    frame_rx: &Receiver<Vec<u8>>,
    signal_out_tx: &Sender<SignalOut>,
    signal_in_rx: &mut tokio::sync::mpsc::Receiver<SignalIn>,
) -> Result<SessionEnd, String> {
    // 会话重建的计时起点：用来定位"客户端点开始 → 首帧上屏"这 1.2 秒花在哪一段
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
    let codec = RTCRtpCodecCapability {
        mime_type: "video/H264".to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: H264_FMTP.to_owned(),
        rtcp_feedback: vec![],
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
            loop {
                match reader.read_rtcp().await {
                    // interceptor 已在读取过程中处理完 NACK / RR / PLI，
                    // 这里只需要持续驱动读取，不需要解析包体
                    Ok(_pkts) => {}
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
    pc.create_data_channel("initial_data_channel", None)
        .await
        .map_err(|e| format!("create_data_channel 失败: {e}"))?;
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
                        SignalIn::Restart => {
                            let _ = pc.close().await;
                            return Ok(SessionEnd::Restart);
                        }
                        SignalIn::Shutdown => {
                            let _ = pc.close().await;
                            return Ok(SessionEnd::Shutdown);
                        }
                    },
                    None => {
                        // 通道断开（主线程 drop 了句柄）
                        let _ = pc.close().await;
                        return Ok(SessionEnd::Shutdown);
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
                        return Ok(SessionEnd::Shutdown);
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
