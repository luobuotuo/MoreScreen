//! Windows 显示目标枚举。
//!
//! 目标：把"系统里有几块屏、哪些是虚拟屏、各自的模式和位置"变成可编程数据，
//! 并且给每块屏一个**稳定身份**（设备实例 ID），避免用数组下标绑定。
//!
//! 依据的 Win32 API：
//! - `EnumDisplayDevicesW`     —— 枚举显示适配器及其挂载的显示设备（可拿到设备实例 ID）
//! - `EnumDisplaySettingsExW`  —— 取当前模式（分辨率/刷新率/位置）

use remote_screen_core::{DisplayId, DisplayInfo, DisplayMode, Rect};

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, CDS_UPDATEREGISTRY, DEVMODE_FIELD_FLAGS, DEVMODEW,
    DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICEW, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, EnumDisplayDevicesW, EnumDisplaySettingsExW,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS, ENUM_DISPLAY_SETTINGS_MODE,
};

/// 把 Win32 的 UTF-16 固定缓冲区转成 String（遇 NUL 截断）。
fn buf_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 适配器是否属于"虚拟显示适配器"（无真实面板）。
///
/// 判据：真实 GPU 适配器的设备实例 ID 走 PCI 总线，形如 `PCI\VEN_8086&DEV_...`；
/// 虚拟显示适配器走软件枚举器，形如 `ROOT\DISPLAY\0000`。
fn is_virtual_adapter(adapter_device_id: &str, adapter_name: &str) -> bool {
    if adapter_device_id.to_ascii_uppercase().starts_with("ROOT\\") {
        return true;
    }
    let name = adapter_name.to_ascii_lowercase();
    name.contains("virtual") || name.contains("dummy")
}

/// 枚举系统全部显示目标。
pub fn enumerate_displays() -> Vec<DisplayInfo> {
    let mut out = Vec::new();

    unsafe {
        // ---- 第一层：显示适配器 ----
        let mut adapter_index: u32 = 0;
        loop {
            let mut adapter = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), adapter_index, &mut adapter, 0).as_bool() {
                break;
            }
            adapter_index += 1;

            let adapter_name = buf_to_string(&adapter.DeviceString);
            let adapter_device_id = buf_to_string(&adapter.DeviceID);
            let adapter_device_name = buf_to_string(&adapter.DeviceName);
            let adapter_is_virtual = is_virtual_adapter(&adapter_device_id, &adapter_name);
            let adapter_flags = adapter.StateFlags.0;
            let adapter_is_primary = adapter_flags & DISPLAY_DEVICE_PRIMARY_DEVICE.0 != 0;

            // ---- 第二层：该适配器挂载的显示设备（显示器） ----
            let mut device_index: u32 = 0;
            let mut attached = 0u32;
            loop {
                let mut device = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                let adapter_pcwstr = to_pcwstr(&adapter_device_name);
                let ok = EnumDisplayDevicesW(
                    PCWSTR(adapter_pcwstr.as_ptr()),
                    device_index,
                    &mut device,
                    0,
                )
                .as_bool();
                if !ok {
                    break;
                }
                device_index += 1;
                attached += 1;

                let raw_device_path = buf_to_string(&device.DeviceName);
                // 显示器设备名形如 "\\.\DISPLAY1\Monitor0"，取模式时要用 "\\.\DISPLAY1"
                let device_path = strip_monitor_suffix(&raw_device_path);
                let device_device_id = buf_to_string(&device.DeviceID);
                let flags = device.StateFlags.0;

                let is_active = flags & DISPLAY_DEVICE_ACTIVE.0 != 0;
                let is_primary =
                    (flags & DISPLAY_DEVICE_PRIMARY_DEVICE.0 != 0) || adapter_is_primary;

                let (mode, bounds) = read_current_mode(&device_path);

                // 稳定身份：优先用设备实例 ID（跨重启稳定）
                let id = if !device_device_id.is_empty() {
                    device_device_id.clone()
                } else {
                    format!("{adapter_device_id}#{}", device_index.saturating_sub(1))
                };

                out.push(DisplayInfo {
                    id: DisplayId(id),
                    device_path,
                    adapter_device_id: adapter_device_id.clone(),
                    is_virtual: adapter_is_virtual,
                    is_active,
                    is_primary,
                    bounds,
                    mode,
                });
            }

            // 没有任何显示器挂载的适配器也要列出：
            // 典型的就是"已安装但未点亮"的虚拟显示适配器，正是扩展屏的候选目标。
            if attached == 0 {
                let (mode, bounds) = read_current_mode(&adapter_device_name);
                out.push(DisplayInfo {
                    id: DisplayId(adapter_device_id.clone()),
                    device_path: adapter_device_name.clone(),
                    adapter_device_id: adapter_device_id.clone(),
                    is_virtual: adapter_is_virtual,
                    is_active: adapter_flags & DISPLAY_DEVICE_ACTIVE.0 != 0,
                    is_primary: adapter_is_primary,
                    bounds,
                    mode,
                });
            }
        }
    }

    out
}

/// 读取某显示设备的当前模式与在虚拟桌面中的位置。
unsafe fn read_current_mode(device_path: &str) -> (DisplayMode, Rect) {
    let fallback = (
        DisplayMode { width: 0, height: 0, refresh_hz: 0, bits_per_pixel: 0 },
        Rect::default(),
    );
    if device_path.is_empty() {
        return fallback;
    }
    let path = to_pcwstr(device_path);
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = EnumDisplaySettingsExW(
        PCWSTR(path.as_ptr()),
        ENUM_CURRENT_SETTINGS,
        &mut dm,
        ENUM_DISPLAY_SETTINGS_FLAGS(0),
    )
    .as_bool();
    if !ok {
        return fallback;
    }
    let position = dm.Anonymous1.Anonymous2.dmPosition;
    let mode = DisplayMode {
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
        refresh_hz: dm.dmDisplayFrequency,
        bits_per_pixel: dm.dmBitsPerPel,
    };
    let bounds = Rect {
        x: position.x,
        y: position.y,
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
    };
    (mode, bounds)
}

/// 尝试把指定显示设备切到指定模式与位置（需显式调用，默认不执行）。
///
/// 注意：会改变用户桌面布局，因此只应由 UI 明确触发。
pub unsafe fn apply_display_mode(
    device_path: &str,
    width: u32,
    height: u32,
    refresh_hz: u32,
    position_x: i32,
    position_y: i32,
) -> Result<(), String> {
    if device_path.is_empty() {
        return Err("显示设备路径为空".into());
    }
    let path = to_pcwstr(device_path);
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    if !EnumDisplaySettingsExW(
        PCWSTR(path.as_ptr()),
        ENUM_CURRENT_SETTINGS,
        &mut dm,
        ENUM_DISPLAY_SETTINGS_FLAGS(0),
    )
    .as_bool()
    {
        return Err(format!("读取 {device_path} 当前模式失败"));
    }
    dm.dmPelsWidth = width;
    dm.dmPelsHeight = height;
    dm.dmDisplayFrequency = refresh_hz;
    dm.Anonymous1.Anonymous2.dmPosition.x = position_x;
    dm.Anonymous1.Anonymous2.dmPosition.y = position_y;
    dm.dmFields = DEVMODE_FIELD_FLAGS(
        DM_POSITION.0 | DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0,
    );
    let result = ChangeDisplaySettingsExW(
        PCWSTR(path.as_ptr()),
        Some(&dm),
        None,
        CDS_UPDATEREGISTRY,
        None,
    );
    if result.0 == 0 {
        Ok(())
    } else {
        Err(format!("ChangeDisplaySettingsExW 返回 DISP_CHANGE({})", result.0))
    }
}

/// 列出某显示设备支持的所有模式（只读探测，不改变任何设置）。
///
/// 用途：判断虚拟显示适配器在"未连接"状态下是否已经暴露可用模式——
/// 如果暴露了，说明可以直接用程序把它点亮成扩展屏；如果没有，
/// 说明必须由驱动客户端（如 Parsec / ToDesk / AskLink 的会话）先建立连接。
pub unsafe fn list_modes(device_path: &str) -> Vec<DisplayMode> {
    let mut modes = Vec::new();
    if device_path.is_empty() {
        return modes;
    }
    let path = to_pcwstr(device_path);
    let mut index: u32 = 0;
    loop {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = EnumDisplaySettingsExW(
            PCWSTR(path.as_ptr()),
            ENUM_DISPLAY_SETTINGS_MODE(index),
            &mut dm,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
        .as_bool();
        if !ok {
            break;
        }
        index += 1;
        if dm.dmPelsWidth == 0 || dm.dmPelsHeight == 0 {
            continue;
        }
        let mode = DisplayMode {
            width: dm.dmPelsWidth,
            height: dm.dmPelsHeight,
            refresh_hz: dm.dmDisplayFrequency,
            bits_per_pixel: dm.dmBitsPerPel,
        };
        if !modes.iter().any(|m| *m == mode) {
            modes.push(mode);
        }
        if modes.len() > 64 {
            break;
        }
    }
    modes
}

/// 生成以 NUL 结尾的 UTF-16 缓冲区。
fn to_pcwstr(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 去掉显示器设备名上的 `\MonitorN` 后缀，得到可用于设置模式的设备路径。
fn strip_monitor_suffix(path: &str) -> String {
    match path.find("\\Monitor") {
        Some(idx) => path[..idx].to_string(),
        None => path.to_string(),
    }
}
