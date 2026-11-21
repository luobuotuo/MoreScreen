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

use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SetProcessDPIAware, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 全局输入开关：只有显式 `--allow-input` 才打开。
static INPUT_ENABLED: AtomicBool = AtomicBool::new(false);

/// 目标显示器的区域（虚拟桌面坐标系）。
static TARGET: std::sync::Mutex<(i32, i32, u32, u32)> = std::sync::Mutex::new((0, 0, 0, 0));

/// 让进程使用物理像素坐标（必须在任何 GDI / SendInput 调用之前执行）。
///
/// 实测：DPI 无关的进程在高 DPI 主屏（125%）上，`GetSystemMetrics` 报 1536x864，
/// 而 `EnumDisplaySettings` 的 dmPosition/dmPelsWidth 报物理 1920x1080。
/// 两套口径混用会让输入落点在屏幕右侧/下侧系统性偏移，且这些区域根本投不到。
/// 声明 DPI 感知后两处口径统一为物理像素。
pub fn init_dpi_awareness() {
    unsafe {
        // 必须用 per-monitor v2：本机主屏 125%、虚拟屏 100%，两种 DPI 并存。
        // system-DPI-aware 下所有坐标按主屏 DPI 当逻辑坐标换算，光标跨屏时会被
        // ×0.8 / ×1.25 重定位——实测表现为点击虚拟屏却落到主屏的窗口上。
        // per-monitor v2 让每个坐标都以物理像素解释，跨屏不再换算。
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() {
            return;
        }
        // 老系统（Win7/早期 Win8）不支持 v2，回退到 system-aware，
        // 单显示器或无 DPI 混合时仍然正确。
        let _ = SetProcessDPIAware();
    }
}

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

/// 虚拟桌面的原点与总尺寸（多屏拼接后的整体），SendInput 的绝对坐标以此归一化。
///
/// 原点必须为 (SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN)：当某块屏被放在主屏左侧/上方时原点为负，
/// 少了这一步会把坐标整体平移，光标落点系统性偏移一整块屏的宽度。
fn virtual_desktop_geometry() -> (i32, i32, i32, i32) {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w <= 0 || h <= 0 {
            (0, 0, 1920, 1080)
        } else {
            (x, y, w, h)
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
    let (ox, oy, vdw, vdh) = virtual_desktop_geometry();
    let nx = nx.clamp(0.0, 1.0);
    let ny = ny.clamp(0.0, 1.0);
    let abs_x = tx as f64 + nx * tw as f64;
    let abs_y = ty as f64 + ny * th as f64;
    // MOUSEEVENTF_VIRTUALDESK 的 0~65535 是相对整个虚拟桌面矩形的，
    // 必须先减去虚拟桌面原点（可能为负），再除以总尺寸。
    let sx = ((abs_x - ox as f64) / vdw as f64 * 65535.0).round() as i32;
    let sy = ((abs_y - oy as f64) / vdh as f64 * 65535.0).round() as i32;
    Some((sx.clamp(0, 65535), sy.clamp(0, 65535)))
}

/// 注入输入并检查结果。SendInput 返回成功注入的条数，返回不足即被拒绝——
/// 最典型是 UIPI：目标窗口完整性级别（管理员进程/部分系统 UI）高于本进程时，
/// SendInput 会被静默丢弃。不检查返回值，「点击无响应」就永远无法区分
/// 是通道断还是注入被拒（2026-11-21 补，定位根本原因的关键观测点）。
fn send_input_checked(inputs: &[INPUT], what: &str) {
    let ok = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if ok != inputs.len() as u32 {
        let err = unsafe { GetLastError() }.0;
        eprintln!(
            "[input] SendInput 失败（{what}）：注入 {}/{}，GetLastError={err}（5=拒绝访问，典型为 UIPI：目标是提权窗口）",
            ok,
            inputs.len()
        );
    }
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
    send_input_checked(&[input], "鼠标");
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
    send_input_checked(&[input], "键盘");
}
