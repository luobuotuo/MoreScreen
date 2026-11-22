//! 端到端（玻璃到玻璃）延迟探针（2026-11-22）。
//!
//! **为什么需要它**：P1 退出条件要求「端到端延迟有可信测量值」。链路各段
//! （采集→编码、RTP、jitterBuffer）都有分段指标，但缺一个闭环锚点——
//! 无法回答「画面上的这一帧，从宿主渲染到平板上屏究竟差多久」。
//!
//! **做法**：在发送帧的左上角叠一行 16 个黑白方块，编码
//! `host_ms（进程启动起算的单调毫秒） mod 65536` 的低 16 位。
//! 接收端用 `requestVideoFrameCallback` + canvas 读回这 16 位，得到该帧的
//! 宿主发送时刻；再通过 DataChannel 往返（`{"t":"sync"}` → `sync-ok` 带
//! `hs`）估算两端时钟偏移，即可算出 `上屏时刻 − 宿主时刻 − 偏移`。
//!
//! **精度**：时钟偏移误差 ±RTT/2（局域网 <1ms）；色块经 H.264 压缩后
//! 用块中心 8×8 平均亮度判定，抗模糊。光标可能在瞬间遮挡其中一块——
//! 接收端对 `host_ms` 做连续性校验，跳变的样本直接丢弃（不影响 p50/p95）。
//!
//! 仅在 `--latency-probe` 显式开启时生效，默认关闭（会遮挡画面左上角）。

use std::sync::OnceLock;
use std::time::Instant;

/// 16 个色块：每块 32×32，水平间隔 8px，贴左上角。
const BLOCK: usize = 32;
const GAP: usize = 8;
const BITS: usize = 16;
/// bit15：探针标志位（恒 1）。低 15 位才是时间戳。
pub const PROBE_FLAG: u16 = 0x8000;

static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// 进程启动起算的单调毫秒。宿主与接收端同步时使用同一基准。
pub fn host_ms() -> u64 {
    ORIGIN.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 把 `host_ms` 的低 16 位编码成 16 个黑白色块，画在 BGRA 帧左上角。
///
/// **必须画在「即将发送的帧」上，且绝不能画进静止保活的干净帧缓存**——
/// 否则保活帧会带着旧时间戳反复发送，接收端测出的延迟会虚高。
pub fn stamp(bgra: &mut [u8], w: usize, h: usize, host_ms: u64) {
    if w < BITS * (BLOCK + GAP) || h < BLOCK {
        return; // 分辨率过小（如 128x720 窄条），不画，避免越界
    }
    // bit15 恒 1 作探针标志：接收端读到 bit15=0 即判定「宿主未开探针」，
    // 避免把画面内容的随机像素误当时间戳。低 15 位编码时间（32768ms 循环，
    // 延迟 <1s 时解卷绕无歧义）。
    let v = ((host_ms & 0x7FFF) as u16) | PROBE_FLAG;
    for i in 0..BITS {
        let on = (v >> i) & 1 == 1;
        let (b, g, r) = if on { (255u8, 255u8, 255u8) } else { (0u8, 0u8, 0u8) };
        let x0 = i * (BLOCK + GAP);
        for y in 0..BLOCK {
            let row = y * w * 4;
            for x in x0..x0 + BLOCK {
                let o = row + x * 4;
                bgra[o] = b;
                bgra[o + 1] = g;
                bgra[o + 2] = r;
                bgra[o + 3] = 255;
            }
        }
    }
}

/// 探针色块在图上的几何参数（接收端 JS 侧必须与之保持一致）。
pub const PROBE_BLOCK: usize = BLOCK;
pub const PROBE_GAP: usize = GAP;
pub const PROBE_BITS: usize = BITS;
