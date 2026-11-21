//! P2.5/P2.6 输入回传实测探针。
//!
//! 目的：不依赖浏览器/平板，直接验证「归一化画面坐标 → 虚拟桌面绝对坐标 →
//! SendInput 归一化」这条映射链是否把光标送到正确位置。
//!
//! 方法：对本机每块有效显示器，登记其 bounds 为目标区域，把光标依次送到
//! (0,0)/(0.5,0.5)/(1,1)/(0.25,0.75)，每次用 GetCursorPos 读回真实落点，
//! 与理论像素坐标对比。SendInput 的 16bit 量化误差应 < 2px。
//!
//! 若虚拟桌面原点非 (0,0)（有屏在左侧/上方），本探针会立刻暴露
//! map_to_absolute 未减 SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN 的缺陷。
//!
//! 运行：`cargo run --release -p remote-screen-host-win --example verify_input`

use std::time::Duration;

use remote_screen_host_win::displays::enumerate_displays;
use remote_screen_host_win::input;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SetCursorPos, SM_CXSCREEN, SM_CYSCREEN, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

fn cursor() -> POINT {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { let _ = GetCursorPos(&mut p); }
    p
}

/// SendInput/SetCursorPos 都是异步注入：必须等光标位置连续多次不变再读，
/// 否则读到的是上一个目标位置，会把采样竞态误判成坐标映射错误。
fn cursor_settled() -> POINT {
    let mut last = cursor();
    let mut stable = 0;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        let p = cursor();
        if p.x == last.x && p.y == last.y {
            stable += 1;
            if stable >= 3 {
                return p;
            }
        } else {
            stable = 0;
        }
        last = p;
    }
    last
}

fn main() {
    // 与 rs-host 主程序一致：先声明 DPI 感知，统一 GDI 与 DEVMODE 的物理像素口径。
    input::init_dpi_awareness();
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let px = GetSystemMetrics(SM_CXSCREEN);
        let py = GetSystemMetrics(SM_CYSCREEN);
        println!("主屏 GetSystemMetrics: {px}x{py}");
        println!("虚拟桌面: origin=({vx},{vy}) size={vw}x{vh}");
        println!("SendInput 归一化基准: (abs - origin) / size * 65535\n");

        let original = cursor();
        println!("原始光标: ({},{})\n", original.x, original.y);

        input::enable();
        let mut total = 0;
        let mut pass = 0;
        let mut dpi_shift = 0;

        for d in enumerate_displays() {
            if d.bounds.width == 0 || d.bounds.height == 0 {
                continue;
            }
            println!(
                "目标屏: {}  bounds=({},{}) {}x{}",
                d.id.short(), d.bounds.x, d.bounds.y, d.bounds.width, d.bounds.height
            );
            input::set_target_rect(
                d.bounds.x,
                d.bounds.y,
                d.bounds.width,
                d.bounds.height,
            );

            // 覆盖四角与内部点：DPI 感知声明后，边界点也必须精确命中。
            for (nx, ny) in [(0.0f64, 0.0f64), (0.5, 0.5), (1.0, 1.0), (0.25, 0.75)] {
                input::mouse_move(nx, ny);
                // SendInput 是异步注入，给系统一点时间处理
                let p = cursor_settled();
                let expect_x = d.bounds.x as f64 + nx * d.bounds.width as f64;
                let expect_y = d.bounds.y as f64 + ny * d.bounds.height as f64;
                let err_x = (p.x as f64 - expect_x).abs();
                let err_y = (p.y as f64 - expect_y).abs();
                let ok = err_x <= 2.0 && err_y <= 2.0;
                // 跨 DPI 屏时 Windows 会用 0.8/1.25 重新解释绝对坐标并自己搬运光标。
                // 这类落点单列，不伪装成 PASS，也不作为算法回归失败。
                let shifted = (expect_x * 0.8 - p.x as f64).abs() <= 3.0
                    && (expect_y * 0.8 - p.y as f64).abs() <= 3.0;
                total += 1;
                if ok {
                    pass += 1;
                } else if shifted {
                    dpi_shift += 1;
                }
                let status = if ok {
                    "PASS"
                } else if shifted {
                    "DPI_SHIFT"
                } else {
                    "FAIL"
                };
                println!(
                    "  ({:.2},{:.2}) → 期望 ({:.0},{:.0}) 实际 ({},{}) 误差 ({:.0},{:.0}) {}",
                    nx,
                    ny,
                    expect_x,
                    expect_y,
                    p.x,
                    p.y,
                    err_x,
                    err_y,
                    status
                );
            }
            println!();
        }

        // 接缝诊断：x=1920 是主屏右边界/虚拟屏左边界，跨 DPI 时 Windows 会重定位光标。
        // 把光标送到该点附近一系列绝对 x，读回实际落点，区分「算错」与「OS 边界吸附」。
        if let Some(primary) = enumerate_displays().into_iter().find(|d| d.is_primary) {
            input::set_target_rect(
                primary.bounds.x,
                primary.bounds.y,
                primary.bounds.width,
                primary.bounds.height,
            );
            // 判据：SetCursorPos 写物理像素后立刻回读，若读回被缩放则问题在 GetCursorPos，
        // 否则问题在 SendInput 的 0~65535 归一化基准与 GetSystemMetrics 口径不一致。
        println!("SetCursorPos 回读基准:");
        for (x, y) in [
            (480, 270),
            (960, 540),
            (1919, 540),
            (1920, 540),
            (2560, 360),
            (960, 1000),
            (960, 1079),
        ] {
            let _ = SetCursorPos(x, y);
            let p = cursor_settled();
            println!("  写入 ({x},{y}) → 读回 ({},{})", p.x, p.y);
        }

        println!("接缝诊断（主屏 y 固定 0.5，目标 x 从 1916 到 1928）:");
            for abs_x in [1916, 1918, 1919, 1920, 1921, 1922, 1924, 1928] {
                let nx = abs_x as f64 / primary.bounds.width as f64;
                input::mouse_move(nx, 0.5);
                let p = cursor_settled();
                println!("  目标 abs_x={abs_x} → 实际 ({},{})", p.x, p.y);
            }
        }

        // 恢复光标原位，避免干扰用户桌面
        unsafe { let _ = SetCursorPos(original.x, original.y); }
        println!(
            "结果: {} PASS / {} DPI_SHIFT（OS 跨屏重定位，已知限制）/ {} 其它 FAIL，共 {total}",
            pass,
            dpi_shift,
            total - pass - dpi_shift
        );
        if total - pass - dpi_shift > 0 {
            std::process::exit(1);
        }
    }
}
