//! remote-screen-core
//!
//! 与平台无关的核心层：显示目标模型、会话状态机、链路指标。
//! 这一层不依赖任何 Windows API，因此可以在任意环境编译与单元测试。

pub mod display;
pub mod metrics;
pub mod session;

pub use display::{DisplayId, DisplayInfo, DisplayMode, DisplayRegistry, Rect};
pub use metrics::{elapsed_ms, json_escape, FrameRecord, MetricsRecorder};
pub use session::{SessionEvent, SessionMachine, SessionState, TransitionError};

/// 项目版本（与 Cargo.toml 保持一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
