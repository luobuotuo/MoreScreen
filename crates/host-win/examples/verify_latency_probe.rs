//! 端到端延迟探针的编码/解码一致性验证（2026-11-22）。
//!
//! 宿主 `latency_probe::stamp` 画色块，接收端 JS 按 (32px 块 / 8px 间隔 /
//! 中心 8×8 平均亮度) 读回。两端布局一旦漂移，测出的延迟就是垃圾。
//! 本探针用与 JS 完全相同的读取规则解码宿主写出的帧，断言：
//!   1. 任意 host_ms 都能原样解出（15 位时间戳）；
//!   2. bit15 恒 1（探针标志，未开启时接收端据此判定）；
//!   3. 色块区域之外一个像素都不改。
//!
//! 运行：`cargo run --release --example verify_latency_probe`

use remote_screen_host_win::latency_probe::{self, PROBE_BITS, PROBE_BLOCK, PROBE_FLAG, PROBE_GAP};

const W: usize = 1280;
const H: usize = 720;
const BG: u8 = 120; // 背景灰（避免黑白判断受底色影响）

/// 与接收端 JS 相同的解码规则。
fn decode_like_js(bgra: &[u8]) -> u16 {
    let mut v = 0u16;
    for i in 0..PROBE_BITS {
        let bx = i * (PROBE_BLOCK + PROBE_GAP) + PROBE_BLOCK / 2;
        let by = PROBE_BLOCK / 2;
        let mut sum = 0u32;
        for dy in -4i32..4 {
            for dx in -4i32..4 {
                let o = ((by as i32 + dy) as usize * W + (bx as i32 + dx) as usize) * 4;
                sum += (bgra[o] as u32 + bgra[o + 1] as u32 + bgra[o + 2] as u32) / 3;
            }
        }
        if sum / 64 > 128 {
            v |= 1 << i;
        }
    }
    v
}

fn main() {
    let mut fails = 0;

    // ---- 用例 1：多组时间戳往返 ----
    for t in [0u64, 1, 2, 0x7FFF, 0x8000, 12345, 32767, 65535, 100_000] {
        let mut f = vec![BG; W * H * 4];
        latency_probe::stamp(&mut f, W, H, t);
        let v = decode_like_js(&f);
        let expect = ((t & 0x7FFF) as u16) | PROBE_FLAG;
        let ok = v == expect;
        println!(
            "host_ms={t:>6} → 解码 0x{v:04X} 期望 0x{expect:04X} {}",
            if ok { "✓" } else { "✗" }
        );
        if !ok {
            fails += 1;
        }
        assert_eq!(v & PROBE_FLAG, PROBE_FLAG, "bit15 必须恒 1（探针标志）");
    }

    // ---- 用例 2：色块区域外零改动 ----
    let mut f = vec![BG; W * H * 4];
    latency_probe::stamp(&mut f, W, H, 0x1234);
    let mut changed_outside = 0;
    let probe_w = PROBE_BITS * (PROBE_BLOCK + PROBE_GAP);
    for y in 0..H {
        for x in 0..W {
            if x < probe_w && y < PROBE_BLOCK {
                continue; // 探针区域
            }
            let o = (y * W + x) * 4;
            if f[o] != BG || f[o + 1] != BG || f[o + 2] != BG {
                changed_outside += 1;
            }
        }
    }
    println!("探针区域外被改像素数 = {changed_outside}（期望 0）");
    if changed_outside != 0 {
        fails += 1;
    }

    // ---- 用例 3：分辨率过小（窄条）不越界 -
    let mut small = vec![BG; 200 * 20 * 4];
    latency_probe::stamp(&mut small, 200, 20, 999); // 应静默跳过
    println!("窄条分辨率 200x20：安全跳过 ✓");

    if fails == 0 {
        println!("\n=== verify_latency_probe：全部用例通过 ===");
    } else {
        println!("\n=== 失败 {fails} 项 ===");
        std::process::exit(1);
    }
}
