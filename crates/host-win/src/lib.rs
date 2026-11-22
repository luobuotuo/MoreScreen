
//! Windows 宿主端实现。
pub mod capture;
pub mod cursor;
pub mod displays;
pub mod encode;
pub mod input;
pub mod readback;
pub mod serve;
pub mod vdd;

/// P1.1 验证工具：H.264 Annex B 解析与 webrtc-rs payloader 兼容性检查。
pub mod h264probe;
pub mod webrtc;
