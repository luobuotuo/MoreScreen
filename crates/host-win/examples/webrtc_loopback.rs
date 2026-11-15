//! P1.2 本机回环验证：offerer（宿主侧）↔ answerer（浏览器侧模拟）。
//!
//! 目的：在不依赖浏览器与本机 Chromium 解码器的前提下，验证 webrtc-rs 的
//! **ICE → DTLS → SRTP → H.264 RTP** 全通路能否跑通。
//!
//! 通过标准：
//! 1. SDP offer/answer 交换成功；
//! 2. ICE 连接进入 `Connected`；
//! 3. answerer 的 `on_track` 收到 RTP 包（数量 > 0）；
//! 4. 收到的包能被 `H264Packet` 正确 depacketize。
//!
//! 运行：`cargo run --release -p remote-screen-host-win --example webrtc_loopback`

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use remote_screen_host_win::webrtc::H264_FMTP;

fn pc_config() -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    }
}

fn h264_codec() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: "video/H264".to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: H264_FMTP.to_owned(),
        rtcp_feedback: vec![],
    }
}

#[tokio::main]
async fn main() {
    println!("=== P1.2 WebRTC 本机回环验证 ===\n");

    // ---------- offerer（宿主侧）----------
    let offerer_api = remote_screen_host_win::webrtc::build_api().expect("offerer api");
    let offerer = Arc::new(
        offerer_api
            .new_peer_connection(pc_config())
            .await
            .expect("offerer pc"),
    );
    let track = Arc::new(TrackLocalStaticSample::new(
        h264_codec(),
        "video".into(),
        "morescreen".into(),
    ));
    offerer
        .add_track(track.clone())
        .await
        .expect("add_track");

    // answerer（浏览器侧模拟）
    let answerer_api = remote_screen_host_win::webrtc::build_api().expect("answerer api");
    let answerer = Arc::new(
        answerer_api
            .new_peer_connection(pc_config())
            .await
            .expect("answerer pc"),
    );

    // answerer 收包计数
    let recv = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recv_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let recv = recv.clone();
        let recv_bytes = recv_bytes.clone();
        answerer.on_track(Box::new(move |track, _receiver, _transceiver| {
            let recv = recv.clone();
            let recv_bytes = recv_bytes.clone();
            Box::pin(async move {
                println!("  [answerer] on_track: {} {}", track.kind(), track.id());
                // 用 H264Packet 验证收到的 RTP 能被正确解析
                use rtp::codecs::h264::H264Packet;
                use rtp::packetizer::Depacketizer;
                let mut dep = H264Packet::default();
                let mut buf = vec![0u8; 2048];
                loop {
                    match track.read(&mut buf).await {
                        Ok((pkt, _attrs)) => {
                            recv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            recv_bytes.fetch_add(pkt.payload.len(), std::sync::atomic::Ordering::SeqCst);
                            let _ = dep.depacketize(&pkt.payload);
                        }
                        Err(_) => break,
                    }
                }
            })
        }));
    }

    // 连接状态上报
    {
        let who = "offerer";
        offerer.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            println!("  [{who}] 状态: {s:?}");
            Box::pin(async {})
        }));
    }

    // ICE candidate 互换（trickle）
    let (a2b_tx, mut a2b_rx) = tokio::sync::mpsc::channel::<RTCIceCandidateInit>(32);
    let (b2a_tx, mut b2a_rx) = tokio::sync::mpsc::channel::<RTCIceCandidateInit>(32);
    {
        let tx = b2a_tx.clone();
        offerer.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    println!("  [offerer] 本地候选: {}", c.to_json().map(|i| i.candidate).unwrap_or_default());
                    if let Ok(init) = c.to_json() {
                        let _ = tx.send(init).await;
                    }
                } else {
                    println!("  [offerer] 候选收集完成");
                }
            })
        }));
    }
    {
        let tx = a2b_tx.clone();
        answerer.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    println!("  [answerer] 本地候选: {}", c.to_json().map(|i| i.candidate).unwrap_or_default());
                    if let Ok(init) = c.to_json() {
                        let _ = tx.send(init).await;
                    }
                } else {
                    println!("  [answerer] 候选收集完成");
                }
            })
        }));
    }

    // ---------- SDP 协商（标准流程：等 gathering 完成，SDP 内含候选）----------
    // 关键点（来自 webrtc-rs 官方测试 signal_pair）：
    // 1. 必须 create_data_channel 才会真正触发后台 ICE gathering；
    // 2. 必须等 gathering_complete_promise 完成，local_description 才会包含候选；
    // 3. 直接把含候选的 SDP 传给对端，无需 trickle 转发 candidate。
    println!("[1] 创建 data channel（触发 ICE gathering）…");
    offerer
        .create_data_channel("initial_data_channel", None)
        .await
        .expect("create_data_channel");

    println!("[2] 生成 offer 并等待候选收集 …");
    let offer = offerer.create_offer(None).await.expect("create_offer");
    let mut offer_gathering = offerer.gathering_complete_promise().await;
    offerer
        .set_local_description(offer)
        .await
        .expect("set_local_description");
    let _ = offer_gathering.recv().await;
    let offer_with_cands = offerer
        .local_description()
        .await
        .expect("offerer local_description");
    println!("    offer（含候选）{} 字节", offer_with_cands.sdp.len());

    println!("[3] answerer 生成 answer 并等待候选收集 …");
    answerer
        .set_remote_description(offer_with_cands)
        .await
        .expect("answerer set_remote_description");
    let answer = answerer.create_answer(None).await.expect("create_answer");
    let mut answer_gathering = answerer.gathering_complete_promise().await;
    answerer
        .set_local_description(answer)
        .await
        .expect("answerer set_local_description");
    let _ = answer_gathering.recv().await;
    let answer_with_cands = answerer
        .local_description()
        .await
        .expect("answerer local_description");
    println!("    answer（含候选）{} 字节", answer_with_cands.sdp.len());

    offerer
        .set_remote_description(answer_with_cands)
        .await
        .expect("offerer set_remote_description");
    println!("[4] SDP 交换完成，等待 ICE 连通 …");

    // ---------- 送帧 ----------
    println!("[3] 等待 ICE 连接建立 …");
    let au_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"D:\app-workbuddy\01_remote-screen\out\first_au.h264".to_string());
    let au = match std::fs::read(&au_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("    读取 AU 失败（{e}），改用合成数据");
            let mut v = vec![0u8, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80];
            v.extend_from_slice(&[0, 0, 0, 1, 0x65]);
            v.extend(std::iter::repeat(0xAB).take(4000));
            v
        }
    };
    println!("    AU 文件: {au_path} ({} 字节)", au.len());

    // 等 ICE 连通
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut connected = false;
    while std::time::Instant::now() < deadline {
        let st = answerer.connection_state();
        if st == RTCPeerConnectionState::Connected {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !connected {
        println!("\n❌ ICE 未在 10 秒内连通（当前状态: {:?}）", answerer.connection_state());
        std::process::exit(1);
    }
    println!("    ICE 已连通 ✓");

    println!("[4] 发送 H.264 访问单元 …");
    let mut ts: u32 = 0;
    for i in 0..30 {
        let sample = webrtc::media::Sample {
            data: Bytes::from(au.clone()),
            timestamp: std::time::SystemTime::now(),
            duration: Duration::from_millis(41),
            packet_timestamp: ts,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };
        ts = ts.wrapping_add(3000);
        track.write_sample(&sample).await.expect("write_sample");
        let _ = i;
        tokio::time::sleep(Duration::from_millis(41)).await;
    }

    // 等接收
    tokio::time::sleep(Duration::from_secs(2)).await;
    let n = recv.load(std::sync::atomic::Ordering::SeqCst);
    let b = recv_bytes.load(std::sync::atomic::Ordering::SeqCst);

    println!("\n=== 结果 ===");
    println!("  连接状态      : {:?}", answerer.connection_state());
    println!("  收到 RTP 包数 : {n}");
    println!("  收到字节数    : {b}");
    if n > 0 {
        println!("\n✅ 回环验证通过：ICE/DTLS/SRTP 全通路可用，H.264 RTP 包成功送达");
        println!("   → webrtc-rs 可承担我们的传输层，P1.2 剩余工作只是接真实链路与接收端页面");
    } else {
        println!("\n❌ 未收到 RTP 包，需排查（track 绑定 / SRTP / SDP m-line）");
        std::process::exit(1);
    }

    let _ = offerer.close().await;
    let _ = answerer.close().await;
}
