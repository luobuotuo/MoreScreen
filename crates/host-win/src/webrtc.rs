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
                    rtcp_feedback: vec![],
                },
                payload_type: 102,
                ..Default::default()
            },
            RTPCodecType::Video,
        )
        .map_err(|e| format!("register_codec(H264) 失败: {e}"))?;

    // 禁用 mDNS 候选：默认模式会产生 `.local` 主机名候选，
    // 局域网对端（尤其本机回环与某些设备）无法解析，导致 ICE 永远停在 Connecting。
    // 局域网场景不需要这层隐私保护，直接用真实 IP 更可靠。
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);

    Ok(APIBuilder::new()
        .with_media_engine(media_engine)
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
    Shutdown,
}

/// WebRTC 会话句柄（同步线程持有）。
pub struct WebRtcHandle {
    /// 送帧通道（同步线程 → tokio 线程），容量 2，满则丢帧
    frame_tx: SyncSender<Vec<u8>>,
    /// 信令输出（主线程轮询）
    signal_out_rx: Receiver<SignalOut>,
    /// 信令输入（主线程投递）
    signal_in_tx: Sender<SignalIn>,
}

impl WebRtcHandle {
    /// 启动 WebRTC 会话（spawn tokio 线程）。
    ///
    /// `fps`：目标帧率，用于计算 RTP 时间戳增量与 sample duration。
    pub fn spawn(fps: u32) -> Result<Self, String> {
        let (frame_tx, frame_rx) = sync_channel::<Vec<u8>>(2);
        let (signal_out_tx, signal_out_rx) = std::sync::mpsc::channel::<SignalOut>();
        let (signal_in_tx, signal_in_rx) = std::sync::mpsc::channel::<SignalIn>();

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

    /// 投递浏览器回传的信令。
    pub fn deliver(&self, sig: SignalIn) {
        let _ = self.signal_in_tx.send(sig);
    }

    /// 关闭会话。
    pub fn close(&self) {
        let _ = self.signal_in_tx.send(SignalIn::Shutdown);
    }
}

/// tokio 线程内的会话主循环。
async fn run_session(
    fps: u32,
    frame_rx: Receiver<Vec<u8>>,
    signal_out_tx: Sender<SignalOut>,
    signal_in_rx: Receiver<SignalIn>,
) -> Result<(), String> {
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
    pc.add_track(track.clone())
        .await
        .map_err(|e| format!("add_track 失败: {e}"))?;

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
    let _ = signal_out_tx.send(SignalOut::Offer { sdp: offer_full.sdp });

    // ---- 5. 主循环：收信令 + 送帧 ----
    let fps = fps.max(1) as u64;
    let ts_step = 90000 / fps; // RTP 时间戳增量（90kHz / fps）
    let mut packet_ts: u32 = 0;
    let mut last_ts = std::time::Instant::now();
    let min_interval = Duration::from_micros(1_000_000 / fps);

    loop {
        // 5a. 处理来自主线程的信令
        while let Ok(sig) = signal_in_rx.try_recv() {
            match sig {
                SignalIn::Answer { sdp } => {
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
                        let _ =
                            signal_out_tx.send(SignalOut::State(format!("add_ice_candidate: {e}")));
                    }
                }
                SignalIn::Shutdown => {
                    let _ = pc.close().await;
                    return Ok(());
                }
            }
        }

        // 5b. 送帧（按目标帧率限速，与采集端限速策略一致）
        if last_ts.elapsed() >= min_interval {
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
                    last_ts = std::time::Instant::now();
                    if let Err(e) = track.write_sample(&sample).await {
                        let _ = signal_out_tx.send(SignalOut::State(format!("write_sample: {e}")));
                    }
                }
                Err(TryRecvError::Empty) => {
                    last_ts = std::time::Instant::now();
                }
                Err(TryRecvError::Disconnected) => {
                    let _ = pc.close().await;
                    return Ok(());
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
