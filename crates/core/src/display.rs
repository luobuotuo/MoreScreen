//! 显示目标模型。
//!
//! 设计要点：显示目标的身份必须稳定，不能用 "显示器 1 / 显示器 2" 这种下标，
//! 因为虚拟显示器插拔、重排、DPI 变化都会让下标漂移。

/// 一块显示目标的稳定身份与几何信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayId(pub String);

impl DisplayId {
    /// 生成对外可展示的短标识。
    pub fn short(&self) -> &str {
        let s = self.0.as_str();
        match s.len() {
            0 => "(unknown)",
            n if n <= 24 => s,
            _ => &s[..24],
        }
    }
}

/// 物理像素矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 显示模式（分辨率 + 刷新率）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub bits_per_pixel: u32,
}

/// 显示目标快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    /// 稳定身份：优先用适配器的设备实例 ID（如 `ROOT\DISPLAY\0001`），回退到设备路径。
    pub id: DisplayId,
    /// Win32 设备路径，如 `\\.\DISPLAY2`。
    pub device_path: String,
    /// 适配器设备实例 ID，用于区分物理 GPU 与虚拟显示适配器。
    pub adapter_device_id: String,
    /// 是否为虚拟显示适配器（无真实面板）。
    pub is_virtual: bool,
    /// 是否当前有输出模式（已连接并点亮）。
    pub is_active: bool,
    /// 是否为主显示器。
    pub is_primary: bool,
    /// 在虚拟桌面中的位置与尺寸。
    pub bounds: Rect,
    /// 当前显示模式。
    pub mode: DisplayMode,
}

impl DisplayInfo {
    /// 人类可读名称。
    pub fn label(&self) -> String {
        let kind = if self.is_virtual { "虚拟" } else { "物理" };
        format!(
            "{} [{}] {}x{}@{}Hz {}",
            self.id.short(),
            kind,
            self.mode.width,
            self.mode.height,
            self.mode.refresh_hz,
            self.device_path
        )
    }
}

/// 显示目标集合，提供按身份取用的能力。
#[derive(Debug, Clone, Default)]
pub struct DisplayRegistry {
    displays: Vec<DisplayInfo>,
}

impl DisplayRegistry {
    pub fn new(displays: Vec<DisplayInfo>) -> Self {
        Self { displays }
    }

    pub fn all(&self) -> &[DisplayInfo] {
        &self.displays
    }

    pub fn get(&self, id: &DisplayId) -> Option<&DisplayInfo> {
        self.displays.iter().find(|d| &d.id == id)
    }

    /// 找出第一个处于可用状态的虚拟显示器（扩展屏 POC 的首选目标）。
    pub fn first_active_virtual(&self) -> Option<&DisplayInfo> {
        self.displays
            .iter()
            .find(|d| d.is_virtual && d.is_active)
            .or_else(|| self.displays.iter().find(|d| d.is_virtual))
    }

    /// 按前缀/子串匹配显示 id，便于命令行用短名指定目标。
    pub fn resolve_prefix(&self, prefix: &str) -> Option<&DisplayInfo> {
        let p = prefix.to_ascii_lowercase();
        self.displays
            .iter()
            .find(|d| d.id.0.to_ascii_lowercase().starts_with(&p))
            .or_else(|| {
                self.displays
                    .iter()
                    .find(|d| d.id.0.to_ascii_lowercase().contains(&p))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, virtual_display: bool, active: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.to_string()),
            device_path: format!("\\\\.\\DISPLAY_{id}"),
            adapter_device_id: id.to_string(),
            is_virtual: virtual_display,
            is_active: active,
            is_primary: !virtual_display,
            bounds: Rect { x: 0, y: 0, width: 1920, height: 1080 },
            mode: DisplayMode { width: 1920, height: 1080, refresh_hz: 60, bits_per_pixel: 32 },
        }
    }

    #[test]
    fn resolve_prefix_matches_case_insensitively() {
        let reg = DisplayRegistry::new(vec![sample("ROOT\\DISPLAY\\0001", true, true)]);
        assert!(reg.resolve_prefix("root").is_some());
        assert!(reg.resolve_prefix("DISPLAY\\0001").is_some());
        assert!(reg.resolve_prefix("nope").is_none());
    }

    #[test]
    fn first_active_virtual_prefers_active() {
        let reg = DisplayRegistry::new(vec![
            sample("ROOT\\DISPLAY\\0000", true, false),
            sample("ROOT\\DISPLAY\\0001", true, true),
        ]);
        assert_eq!(
            reg.first_active_virtual().map(|d| d.id.0.clone()),
            Some("ROOT\\DISPLAY\\0001".to_string())
        );
    }

    #[test]
    fn first_active_virtual_falls_back_to_inactive_virtual() {
        let reg = DisplayRegistry::new(vec![sample("ROOT\\DISPLAY\\0000", true, false)]);
        assert!(reg.first_active_virtual().is_some());
    }
}
