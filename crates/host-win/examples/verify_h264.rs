//! P1.1.2 验证程序：用 rs-host 编码器的真实输出测试 H264Payloader 兼容性。
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| {
        r"D:/app-workbuddy/01_remote-screen/out/first_au.h264".to_string()
    });
    let mtu: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1200);

    println!("=== P1.1.2 webrtc-rs H264Payloader 兼容性验证 ===");
    println!("输入文件 : {path}");
    println!("MTU      : {mtu}");

    let data = match std::fs::read(Path::new(&path)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("读取失败: {e}");
            std::process::exit(1);
        }
    };
    println!("输入大小 : {} 字节", data.len());

    // 原始 NAL 结构
    let nals = remote_screen_host_win::h264probe::split_annexb(&data);
    println!("\n--- 原始 NAL 结构（我们编码器的输出）---");
    let mut sizes = Vec::new();
    for n in &nals {
        let t = if n.is_empty() { 0 } else { n[0] & 0x1F };
        sizes.push(n.len());
        println!("  type {:>2} {:<14} {} 字节", t, remote_screen_host_win::h264probe::nal_type_name(t), n.len());
    }
    let max_nal = sizes.iter().max().copied().unwrap_or(0);
    println!("  最大 NAL: {max_nal} 字节");

    // payload
    let (payloads, restored_count, restored_bytes) = remote_screen_host_win::h264probe::roundtrip(&data, mtu);
    println!("\n--- payload() 输出（RTP payload 序列）---");
    println!("  payload 数量: {}", payloads.len());
    for (i, p) in payloads.iter().enumerate().take(12) {
        let b0 = if p.is_empty() { 0 } else { p[0] };
        let kind = match b0 & 0x1F {
            24 => "STAP-A",
            28 => "FU-A",
            1 | 5 => "single NAL",
            t => {
                println!("    #{i}: {} 字节  type{t}", p.len());
                continue;
            }
        };
        let extra = if b0 & 0x1F == 28 && p.len() > 1 {
            let b1 = p[1];
            let s = if b1 & 0x80 != 0 { " START" } else { "" };
            let e = if b1 & 0x40 != 0 { " END" } else { "" };
            format!("{s}{e}")
        } else {
            String::new()
        };
        println!("    #{i}: {} 字节  {kind}{extra}", p.len());
    }
    if payloads.len() > 12 {
        println!("    …（其余 {} 个省略）", payloads.len() - 12);
    }

    // 往返还原
    println!("\n--- depacketize() 往返还原 ---");
    println!("  还原 NAL 数  : {restored_count}（原始 {}）", nals.len());
    println!("  还原字节数  : {restored_bytes}");

    // 判定
    let ok = restored_count == nals.len() && restored_bytes == data.len();
    println!("\n=== 结论 ===");
    if ok {
        println!("  ✅ 完全兼容：NAL 数量与总字节数均一致");
        println!("  → webrtc-rs 可直接消费我们的 QSV Annex B 输出，无需自研 packetization");
    } else {
        let diff = restored_bytes as i64 - data.len() as i64;
        println!("  ⚠️ 有差异（字节差 {diff}）");
        println!("  → 常见原因：AUD/FILLER NAL 被 payloader 主动丢弃（这是正确行为，浏览器也不需要）");
        println!("  → 或 STAP-A 聚合导致的起始码数量变化，需进一步核对");
    }
}
