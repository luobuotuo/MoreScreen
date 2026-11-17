//! 输入回传：把接收端的鼠标/触摸/键盘事件注入本机（P2.5 / P2.6）。
//!
//! # 安全边界
//!
//! - **默认关闭**：只有宿主启动时显式传 `--allow-input` 才会真正注入事件，
//!   否则本模块的调用点直接被跳过（`input_enabled()` 为 false）。
//! - 只在本机执行合成输入，不涉及任何网络外发。
//!
//! # 坐标口径
//!
//! 接收端上报的是**画面内的归一化坐标**（0.0~1.0，左上为原点），
//! 因为平板不知道虚拟屏在虚拟桌面中的绝对位置。
//! 本模块负责把归一化坐标换算成：
//!
//! ```text
//! 虚拟桌面绝对像素 = 目标屏 bounds + 归一化 * 目标屏尺寸
//! SendInput 归一化   = 绝对像素 / 虚拟桌面总尺寸 * 65535
//! ```
//!
//! 这样即使虚拟屏被摆放在 (1920, 0) 或负坐标，映射依然正确。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN};

/// 全局输入开关：只有显式 `--allow-input` 才打开。
static INPUT_ENABLED: AtomicBool = AtomicBool::new(false);

/// 目标显示器的区域（虚拟桌面坐标系）。
static TARGET: std::sync::Mutex<(i32, i32, u32, u32)> = std::sync::Mutex::new((0, 0, 0, 0));

/// 打开输入回传开关。
pub fn enable() {
    INPUT_ENABLED.store(true, Ordering::SeqCst);
}

/// 输入回传是否已启用。
pub fn input_enabled() -> bool {
    INPUT_ENABLED.load(Ordering::SeqCst)
}

/// 登记采集目标的区域，用于把画面归一化坐标映射到虚拟桌面绝对坐标。
pub fn set_target_rect(x: i32, y: i32, width: u32, height: u32) {
    if let Ok(mut g) = TARGET.lock() {
        *g = (x, y, width, height);
    }
}

/// 虚拟桌面总尺寸（多屏拼接后的整体），SendInput 的绝对坐标以此归一化。
fn virtual_desktop_size() -> (i32, i32) {
    unsafe {
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w <= 0 || h <= 0 {
            (1920, 1080)
        } else {
            (w, h)
        }
    }
}

/// 把画面内归一化坐标（0~1）换算成 SendInput 需要的 0~65535 归一化绝对坐标。
///
/// 返回 `None` 表示目标区域尚未登记或参数异常。
fn map_to_absolute(nx: f64, ny: f64) -> Option<(i32, i32)> {
    let (tx, ty, tw, th) = *TARGET.lock().ok()?;
    if tw == 0 || th == 0 {
        return None;
    }
    let (vdx, vdy) = virtual_desktop_size();
    let nx = nx.clamp(0.0, 1.0);
    let ny = ny.clamp(0.0, 1.0);
    let abs_x = tx as f64 + nx * tw as f64;
    let abs_y = ty as f64 + ny * th as f64;
    let sx = (abs_x / vdx as f64 * 65535.0).round() as i32;
    let sy = (abs_y / vdy as f64 * 65535.0).round() as i32;
    Some((sx.clamp(0, 65535), sy.clamp(0, 65535)))
}

/// 发送一个鼠标输入事件。
fn send_mouse(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, mouse_data: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// 把光标移动到画面内归一化位置。
pub fn mouse_move(nx: f64, ny: f64) {
    if !input_enabled() {
        return;
    }
    if let Some((sx, sy)) = map_to_absolute(nx, ny) {
        send_mouse(
            sx,
            sy,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            0,
        );
    }
}

/// 鼠标按键：`btn` 取 `left` / `right` / `middle`。
pub fn mouse_button(btn: &str, down: bool) {
    if !input_enabled() {
        return;
    }
    let flags = match (btn, down) {
        ("left", true) => MOUSEEVENTF_LEFTDOWN,
        ("left", false) => MOUSEEVENTF_LEFTUP,
        ("right", true) => MOUSEEVENTF_RIGHTDOWN,
        ("right", false) => MOUSEEVENTF_RIGHTUP,
        ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
        ("middle", false) => MOUSEEVENTF_MIDDLEUP,
        _ => return,
    };
    send_mouse(0, 0, flags, 0);
}

/// 滚轮：`delta` 为正向上、负向下，单位为 Windows 的 120 分之一格。
pub fn wheel(delta: i32) {
    if !input_enabled() {
        return;
    }
    send_mouse(0, 0, MOUSEEVENTF_WHEEL, delta);
}

/// 横向滚轮（触摸板横向滑动）。
pub fn hwheel(delta: i32) {
    if !input_enabled() {
        return;
    }
    send_mouse(0, 0, MOUSEEVENTF_HWHEEL, delta);
}

/// 键盘按键：`vk` 为 Windows 虚拟键码。
pub fn key(vk: u16, down: bool) {
    if !input_enabled() || vk == 0 {
        return;
    }
    let flags = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}
