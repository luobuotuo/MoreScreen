//! VDD（虚拟显示器驱动）设备节点管理。
//!
//! # 为什么需要这个模块
//!
//! IddCx 间接显示驱动（如 MikeTheTech 的 MttVDD）在 INF 里用 **root-enumerated
//! 软件设备**声明自己：
//!
//! ```text
//! [Standard.NTamd64]
//! %DeviceName%=MyDevice_Install, Root\MttVDD
//! ```
//!
//! `pnputil /add-driver xxx.inf /install` 只做两件事：
//! 1. 把驱动包发布到 DriverStore（生成 `oemNNN.inf`）；
//! 2. 给**已经存在**的匹配设备安装驱动。
//!
//! 它**不会凭空创建** root-enumerated 设备节点。所以驱动包装好后，
//! `pnputil /enum-devices /instanceid "ROOT\MttVDD"` 依然找不到设备。
//!
//! 建立节点必须走 SetupAPI 的三步（等价于 WDK 的 `devcon install`）：
//! 1. `SetupDiCreateDeviceInfo` —— 在设备信息集里创建软件设备；
//! 2. `SetupDiSetDeviceRegistryProperty(SPDRP_HARDWAREID)` —— 写入硬件 ID；
//! 3. `SetupDiCallClassInstaller(DIF_REGISTERDEVICE)` —— 注册设备；
//! 4. `UpdateDriverForPlugAndPlayDevices` —— 把驱动绑定上去。
//!
//! 本模块直接用 `windows` crate 调这些 API，**不需要安装 WDK/devcon**。

use std::path::Path;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetDeviceInstanceIdW,
    SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW, DICD_GENERATE_ID,
    DIF_REGISTERDEVICE, INSTALLFLAG_FORCE, SPDRP_FRIENDLYNAME, SPDRP_HARDWAREID, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 显示适配器设备类 GUID：{4D36E968-E325-11CE-BFC1-08002BE10318}
pub const DISPLAY_CLASS_GUID: GUID = GUID::from_values(
    0x4D36E968,
    0xE325,
    0x11CE,
    [0xBF, 0xC1, 0x08, 0x00, 0x2B, 0xE1, 0x03, 0x18],
);

/// 当前进程是否以管理员身份运行（令牌已提升）。
pub fn is_admin() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        // 直接读字节，不依赖 TOKEN_ELEVATION 结构体布局
        let mut buf = [0u8; 4];
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            buf.len() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && buf[0] != 0
    }
}

/// 生成以 NUL 结尾的 UTF-16 缓冲区。
fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 生成 REG_MULTI_SZ 的字节缓冲（UTF-16LE，多个字符串，末尾双 NUL）。
fn wide_multi(items: &[&str]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    for s in items {
        for c in s.encode_utf16() {
            buf.extend_from_slice(&c.to_le_bytes());
        }
        buf.extend_from_slice(&[0, 0]);
    }
    // REG_MULTI_SZ 需要额外的终止 NUL
    buf.extend_from_slice(&[0, 0]);
    buf
}

/// 一次 VDD 安装/建节点的结果。
#[derive(Debug, Default)]
pub struct VddInstallReport {
    /// 执行前节点是否已存在
    pub node_existed: bool,
    /// 本次是否新建了节点
    pub node_created: bool,
    /// 驱动是否绑定成功
    pub driver_bound: bool,
    /// 系统是否提示需要重启
    pub reboot_required: bool,
    /// 设备的实例 ID
    pub instance_id: Option<String>,
    /// 过程信息
    pub messages: Vec<String>,
}

/// 把常见的 SetupAPI 错误码翻译成可读提示。
pub fn explain_setupapi_error(code: u32) -> &'static str {
    match code {
        0xE0000200 => "SPAPI_E_CLASS_NOTREGISTERED：指定的设备类未注册",
        0xE0000201 => "SPAPI_E_DEVICE_INTERFACE_ACTIVE：设备接口已被占用",
        0xE0000202 => "SPAPI_E_FAILED：SetupAPI 内部失败",
        0xE0000203 => "SPAPI_E_INVALID_PARAM：参数无效",
        0xE0000204 => "SPAPI_E_INVALID_REG_PROPERTY：注册表属性无效",
        0xE0000205 => "SPAPI_E_NO_SUCH_REGCLASS：硬件 ID 没有对应的已注册 setup class（常见原因：创建时的 DeviceName 带反斜杠被当成 HW ID）",
        0xE0000206 => "SPAPI_E_NO_SUCH_DEVINST：设备实例不存在",
        0xE0000207 => "SPAPI_E_NO_SUCH_DRIVER：没有匹配的驱动",
        0xE0000208 => "SPAPI_E_NO_DEVICE_IDS：设备没有硬件 ID",
        0xE0000209 => "SPAPI_E_UNKNOWN_ERROR：未知错误",
        5 => "ERROR_ACCESS_DENIED：权限不足，需要管理员",
        _ => "",
    }
}

impl VddInstallReport {
    fn note(&mut self, msg: impl Into<String>) {
        self.messages.push(msg.into());
    }
}

/// 在设备信息集中按实例 ID 查找设备；找到返回实例 ID 与 devinfo 下标。
fn find_device_instance(set: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO) -> Option<(String, u32)> {
    unsafe {
        let mut index = 0u32;
        loop {
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(set, index, &mut info).is_err() {
                return None;
            }
            // 先问缓冲区大小
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInstanceIdW(set, &info, None, Some(&mut needed));
            if needed == 0 {
                index += 1;
                continue;
            }
            let mut buf = vec![0u16; needed as usize];
            if SetupDiGetDeviceInstanceIdW(set, &info, Some(&mut buf), Some(&mut needed)).is_ok() {
                let s = String::from_utf16_lossy(&buf[..needed.saturating_sub(1) as usize]);
                if s.to_ascii_lowercase().contains("mttvdd") {
                    return Some((s, index));
                }
            }
            index += 1;
        }
    }
}

/// 创建（或复用）root-enumerated 设备节点并绑定驱动。
///
/// `hardware_id` 形如 `Root\MttVDD`，必须与 INF `[Standard.NTamd64]` 段的模型行一致。
pub fn install_vdd_device(inf_path: &Path, hardware_id: &str) -> Result<VddInstallReport, String> {
    let mut report = VddInstallReport::default();

    if !is_admin() {
        return Err(
            "需要管理员权限：请在『管理员』PowerShell / 终端中运行（创建设备节点属于系统级操作）"
                .to_string(),
        );
    }
    if !inf_path.exists() {
        return Err(format!("INF 文件不存在: {}", inf_path.display()));
    }
    let inf_str = inf_path
        .canonicalize()
        .unwrap_or_else(|_| inf_path.to_path_buf())
        .to_string_lossy()
        .to_string();

    unsafe {
        // ---- 步骤 1：打开设备信息集，检查节点是否已存在 ----
        let set = SetupDiCreateDeviceInfoList(None, None)
            .map_err(|e| format!("SetupDiCreateDeviceInfoList 失败: {e}"))?;

        let mut instance_id: Option<String> = None;
        {
            let found = find_device_instance(set);
            if let Some((id, _)) = found {
                report.node_existed = true;
                report.note(format!("设备节点已存在: {id}"));
                report.instance_id = Some(id.clone());
                instance_id = Some(id);
            }
        }

        // ---- 步骤 2：创建节点（若不存在）----
        if !report.node_existed {
            // 注意：DeviceName **不能**带反斜杠。
            // 微软文档规定 DeviceName 含 `\` 时会被解释为硬件 ID，
            // 而 `Root\MttVDD` 在 SetupAPI 侧还没有对应的已注册 setup class，
            // 会直接返回 SPAPI_E_NO_SUCH_REGCLASS (0xE0000205)。
            // devcon 的做法同样是传普通名字，硬件 ID 另行写入。
            let device_name = hardware_id.rsplit('\\').next().unwrap_or("MttVDD");
            let name_w = wide(device_name);
            let desc_w = wide("Virtual Display Driver");
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;

            SetupDiCreateDeviceInfoW(
                set,
                PCWSTR(name_w.as_ptr()),
                &DISPLAY_CLASS_GUID,
                PCWSTR(desc_w.as_ptr()),
                None,
                DICD_GENERATE_ID,
                Some(&mut info),
            )
            .map_err(|e| {
                let code = e.code().0 as u32;
                let hint = explain_setupapi_error(code);
                if hint.is_empty() {
                    format!("SetupDiCreateDeviceInfo 失败: {e} (0x{:08X})", code)
                } else {
                    format!("SetupDiCreateDeviceInfo 失败: {e} (0x{:08X}) — {hint}", code)
                }
            })?;

            // 写硬件 ID（REG_MULTI_SZ）。INF 有两条模型行：Root\MttVDD 与 MttVDD，
            // 两个都写上以提高匹配成功率。
            let hw = wide_multi(&[hardware_id, "MttVDD"]);
            SetupDiSetDeviceRegistryPropertyW(set, &mut info, SPDRP_HARDWAREID, Some(&hw))
                .map_err(|e| {
                    let code = e.code().0 as u32;
                    let hint = explain_setupapi_error(code);
                    if hint.is_empty() {
                        format!("写 HardwareID 失败: {e} (0x{:08X})", code)
                    } else {
                        format!("写 HardwareID 失败: {e} (0x{:08X}) — {hint}", code)
                    }
                })?;

            let fname = wide_multi(&["Virtual Display Driver"]);
            let _ = SetupDiSetDeviceRegistryPropertyW(set, &mut info, SPDRP_FRIENDLYNAME, Some(&fname));

            SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&info))
                .map_err(|e| {
                    let code = e.code().0 as u32;
                    let hint = explain_setupapi_error(code);
                    if hint.is_empty() {
                        format!("DIF_REGISTERDEVICE 失败: {e} (0x{:08X})", code)
                    } else {
                        format!("DIF_REGISTERDEVICE 失败: {e} (0x{:08X}) — {hint}", code)
                    }
                })?;

            report.node_created = true;
            report.note("设备节点已创建并注册".to_string());

            // 重新取一次实例 ID
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInstanceIdW(set, &info, None, Some(&mut needed));
            if needed > 0 {
                let mut buf = vec![0u16; needed as usize];
                if SetupDiGetDeviceInstanceIdW(set, &info, Some(&mut buf), Some(&mut needed)).is_ok() {
                    let s = String::from_utf16_lossy(&buf[..needed.saturating_sub(1) as usize]);
                    report.instance_id = Some(s.clone());
                    instance_id = Some(s);
                }
            }
        }

        let _ = SetupDiDestroyDeviceInfoList(set);

        // ---- 步骤 3：绑定驱动 ----
        let target_id = instance_id.unwrap_or_else(|| hardware_id.to_string());
        let id_w = wide(&target_id);
        let inf_w = wide(&inf_str);
        let mut reboot = windows::core::BOOL::default();

        let bind_result = UpdateDriverForPlugAndPlayDevicesW(
            None,
            PCWSTR(id_w.as_ptr()),
            PCWSTR(inf_w.as_ptr()),
            INSTALLFLAG_FORCE,
            Some(&mut reboot),
        );

        match bind_result {
            Ok(()) => {
                report.driver_bound = true;
                report.reboot_required = reboot.as_bool();
                report.note(format!("驱动已绑定到 {target_id}"));
            }
            Err(e) => {
                // 兜底：用硬件 ID 再试一次（实例 ID 与硬件 ID 可能不同）
                let hw_w = wide(hardware_id);
                let mut reboot2 = windows::core::BOOL::default();
                let retry = UpdateDriverForPlugAndPlayDevicesW(
                    None,
                    PCWSTR(hw_w.as_ptr()),
                    PCWSTR(inf_w.as_ptr()),
                    INSTALLFLAG_FORCE,
                    Some(&mut reboot2),
                );
                if retry.is_ok() {
                    report.driver_bound = true;
                    report.reboot_required = reboot2.as_bool();
                    report.note(format!("驱动已绑定到 {hardware_id}（硬件 ID 重试成功）"));
                } else {
                    return Err(format!(
                        "绑定驱动失败: {e}；用硬件 ID 重试也失败: {}",
                        retry.unwrap_err()
                    ));
                }
            }
        }
    }

    Ok(report)
}

/// 检查 VDD 驱动文件是否已落地到 UMDF 目录。
pub fn umdf_driver_installed() -> Option<(String, u64)> {
    let path = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let dll = Path::new(&path).join("System32").join("drivers").join("UMDF").join("MttVDD.dll");
    if dll.exists() {
        let len = std::fs::metadata(&dll).map(|m| m.len()).unwrap_or(0);
        Some((dll.to_string_lossy().to_string(), len))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_multi_ends_with_double_nul() {
        let b = wide_multi(&["Root\\MttVDD", "MttVDD"]);
        assert!(b.len() >= 4);
        // 末尾必须是双 NUL
        assert_eq!(&b[b.len() - 4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn wide_is_nul_terminated() {
        let w = wide("abc");
        assert_eq!(w.len(), 4);
        assert_eq!(w[3], 0);
    }

    #[test]
    fn display_class_guid_is_correct() {
        assert_eq!(DISPLAY_CLASS_GUID.data1, 0x4D36E968);
        assert_eq!(DISPLAY_CLASS_GUID.data2, 0xE325);
        assert_eq!(DISPLAY_CLASS_GUID.data3, 0x11CE);
    }
}
