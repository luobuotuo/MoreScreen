//! 核心层自检二进制。
//!
//! 为什么需要它：部分安全策略会阻止执行 debug 版 `cargo test` 产物，
//! 但普通二进制不受影响。这个自检把关键断言跑一遍，便于在受限环境中验证。
//!
//! 退出码 0 = 全部通过，非 0 = 有断言失败。

use remote_screen_core::{
    json_escape, DisplayId, DisplayInfo, DisplayMode, DisplayRegistry, FrameRecord,
    MetricsRecorder, Rect, SessionEvent, SessionMachine, SessionState,
};

fn sample(id: &str, is_virtual: bool, is_active: bool) -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(id.to_string()),
        device_path: format!("\\\\.\\DISPLAY_{id}"),
        adapter_device_id: id.to_string(),
        is_virtual,
        is_active,
        is_primary: !is_virtual,
        bounds: Rect { x: 0, y: 0, width: 1920, height: 1080 },
        mode: DisplayMode { width: 1920, height: 1080, refresh_hz: 60, bits_per_pixel: 32 },
    }
}

fn main() {
    let mut failed = 0usize;
    let mut check = |name: &str, ok: bool| {
        if ok {
            println!("  PASS  {name}");
        } else {
            println!("  FAIL  {name}");
            failed += 1;
        }
    };

    println!("remote-screen core selfcheck (v{})", remote_screen_core::VERSION);

    // 1) 显示注册表
    let reg = DisplayRegistry::new(vec![
        sample("ROOT\\DISPLAY\\0000", true, false),
        sample("ROOT\\DISPLAY\\0001", true, true),
        sample("PCI\\VEN_8086&DEV_9A49", false, true),
    ]);
    check("前缀匹配大小写不敏感", reg.resolve_prefix("root").is_some());
    check("优先返回已点亮的虚拟屏", {
        reg.first_active_virtual().map(|d| d.id.0.clone())
            == Some("ROOT\\DISPLAY\\0001".to_string())
    });
    check("按前缀找不到时返回 None", reg.resolve_prefix("zzz").is_none());

    // 2) 状态机
    let mut m = SessionMachine::new();
    let mut seq_ok = true;
    for ev in [
        SessionEvent::DisplayReady,
        SessionEvent::CaptureStarted,
        SessionEvent::EncoderReady,
        SessionEvent::TransportStarted,
        SessionEvent::PeerConnected,
    ] {
        if m.apply(ev).is_err() {
            seq_ok = false;
        }
    }
    check("正常路径走到 connected", seq_ok && m.state() == SessionState::Connected);

    let mut m2 = SessionMachine::new();
    check("非法迁移被拒绝且状态不变", {
        m2.apply(SessionEvent::PeerConnected).is_err() && m2.state() == SessionState::Idle
    });

    let mut m3 = SessionMachine::new();
    for ev in [
        SessionEvent::DisplayReady,
        SessionEvent::CaptureStarted,
        SessionEvent::EncoderReady,
        SessionEvent::TransportStarted,
        SessionEvent::PeerConnected,
    ] {
        let _ = m3.apply(ev);
    }
    check("断线后可回到 connected", {
        m3.apply(SessionEvent::LinkLost).unwrap() == SessionState::Reconnecting
            && m3.apply(SessionEvent::Reconnected).unwrap() == SessionState::Connected
    });

    // 3) 指标写入
    let dir = std::env::temp_dir().join("rs_core_selfcheck");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("metrics.jsonl");
    let mut rec = MetricsRecorder::create(&path, 1_000_000.0, 1).expect("创建指标文件");
    rec.record(&FrameRecord {
        session_id: 7,
        display_id: "ROOT\\DISPLAY\\0001".into(),
        frame_id: 42,
        capture_src_ts: 0,
        capture_deq_ts: 1_000,
        enc_submit_ts: 2_000,
        enc_out_ts: 4_000,
        send_ts: 5_000,
        bytes: 4096,
        keyframe: true,
        drop_reason: None,
    })
    .expect("写指标");
    rec.flush().expect("刷盘");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    check("指标写出 JSONL 且含分段耗时", {
        content.contains("\"frame_id\":42") && content.contains("\"encode_ms\":2.000")
    });
    check("JSON 转义正确", json_escape("a\"b") == "a\\\"b");
    let _ = std::fs::remove_dir_all(&dir);

    if failed == 0 {
        println!("全部通过");
    } else {
        println!("失败 {failed} 项");
        std::process::exit(1);
    }
}
