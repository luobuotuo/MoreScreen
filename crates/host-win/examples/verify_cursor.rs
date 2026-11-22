//! 自绘光标功能探针（2026-11-22）。
//!
//! 验证 CursorPainter 的完整链路：SetCursorPos 移动鼠标 → render 合成 →
//! 断言光标像素落在正确位置、干净帧不被污染、光标移走后旧位置可重画。
//!
//! 运行：`cargo run --release --example verify_cursor`

use remote_screen_host_win::cursor::CursorPainter;
use remote_screen_host_win::input::init_dpi_awareness;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

const W: usize = 1920;
const H: usize = 1080;

fn white_frame() -> Vec<u8> {
    vec![248u8; W * H * 4] // 近白底（BGRA 全 248，与光标像素易区分）
}

/// 在 (x,y) 周围 48x48 窗口内找“被画过的像素”（与底色 248 差异 > 40）。
fn painted_near(buf: &[u8], x: i32, y: i32) -> usize {
    let mut n = 0;
    for dy in -24i32..=24 {
        for dx in -24i32..=24 {
            let tx = x + dx;
            let ty = y + dy;
            if tx < 0 || ty < 0 || tx >= W as i32 || ty >= H as i32 {
                continue;
            }
            let o = (ty as usize * W + tx as usize) * 4;
            for c in 0..3 {
                if (buf[o + c] as i32 - 248).abs() > 40 {
                    n += 1;
                    break;
                }
            }
        }
    }
    n
}

fn main() {
    init_dpi_awareness();

    let mut clean = white_frame();
    let mut painter = CursorPainter::default();
    let mut scratch: Vec<u8> = Vec::new();

    // ---- 用例 1：移动到主屏 (500,500) → 光标应画在该处 ----
    unsafe { SetCursorPos(500, 500) };
    std::thread::sleep(std::time::Duration::from_millis(60));
    let pos1 = {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) };
        p
    };
    println!("鼠标位置 1：({}, {})", pos1.x, pos1.y);

    let out = painter.render(&mut scratch, &clean, W, H, 0, 0);
    assert!(out.is_some(), "用例1：光标应可见且在区域内，render 返回了 None");
    let out = out.unwrap();
    let n1 = painted_near(out, 500, 500);
    println!("用例1：(500,500) 附近被画像素数 = {n1}");
    assert!(n1 > 5, "用例1：光标像素未落在 (500,500) 附近");

    // 干净帧不能被污染（render 只能写 scratch，不能写 clean）
    let clean_pixels = painted_near(&clean, 500, 500);
    assert_eq!(clean_pixels, 0, "用例1：干净帧被污染了！render 必须只写 scratch");
    println!("用例1：干净帧未被污染 ✓");

    // ---- 用例 2：移动到 (1000,600) → 新位置有光标，旧位置干净 ----
    unsafe { SetCursorPos(1000, 600) };
    std::thread::sleep(std::time::Duration::from_millis(60));
    let out2 = painter.render(&mut scratch, &clean, W, H, 0, 0);
    assert!(out2.is_some(), "用例2：render 返回 None");
    let out2 = out2.unwrap();
    let n2 = painted_near(out2, 1000, 600);
    let old = painted_near(out2, 500, 500);
    println!("用例2：(1000,600) 附近被画像素数 = {n2}；(500,500) 附近 = {old}");
    assert!(n2 > 5, "用例2：光标未移动到 (1000,600)");
    assert_eq!(old, 0, "用例2：旧位置应干净（scratch 从 clean 重新克隆）");

    // ---- 用例 3：光标移出采集区域（模拟区域外）→ render 应返回 None ----
    // 采集区域设为虚拟屏原点 (1920,0)（虚拟屏），主屏鼠标必然在外。
    let out3 = painter.render(&mut scratch, &clean, W, H, 1920, 0);
    let status = if out3.is_none() { "None ✓" } else { "Some ✗（应返回 None）" };
    println!("用例3：光标在区域外 → render = {status}");
    assert!(out3.is_none(), "用例3：区域外应返回 None（直接发干净帧）");

    // ---- 导出最终合成帧供肉眼复核（BGRA→PPM）----
    let out_fin = painter
        .render(&mut scratch, &clean, W, H, 0, 0)
        .expect("用例4：光标在区域内");
    let ppm_path = "cursor_verify.ppm";
    use std::io::Write;
    let mut f = std::fs::File::create(ppm_path).expect("创建 PPM");
    write!(f, "P6\n{W} {H}\n255\n").unwrap();
    for px in out_fin.chunks_exact(4) {
        f.write_all(&[px[2], px[1], px[0]]).unwrap();
    }
    println!("已导出 {ppm_path}（光标在 1000,600 附近）");

    println!("\n=== verify_cursor：4/4 用例通过 ===");
}
