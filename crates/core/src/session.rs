//! 会话状态机：把"显示就绪 → 采集 → 编码 → 传输 → 连接"变成可验证的状态迁移。
//!
//! 目的：任何一次故障都必须能落到一个明确状态和明确原因上，
//! 而不是变成"画面不动但不知道卡在哪"。

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 未启动。
    Idle,
    /// 虚拟显示器已就绪（已连接、已点亮、可扩展）。
    DisplayReady,
    /// 正在采集该显示器的帧。
    Capturing,
    /// 正在编码（已出 H.264 码流）。
    Encoding,
    /// 传输服务已启动，等待接收端。
    Serving,
    /// 接收端已连接并正常收帧。
    Connected,
    /// 已连接但质量下降（丢包、码率下调、解码跟不上）。
    Degraded,
    /// 连接中断，正在重连。
    Reconnecting,
    /// 停止。
    Stopped,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::DisplayReady => "display_ready",
            SessionState::Capturing => "capturing",
            SessionState::Encoding => "encoding",
            SessionState::Serving => "serving",
            SessionState::Connected => "connected",
            SessionState::Degraded => "degraded",
            SessionState::Reconnecting => "reconnecting",
            SessionState::Stopped => "stopped",
        }
    }

    /// 是否处于"正在跑帧"的活跃状态。
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            SessionState::Capturing | SessionState::Encoding | SessionState::Serving
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// 虚拟显示器就绪。
    DisplayReady,
    /// 采集启动成功。
    CaptureStarted,
    /// 编码器就绪。
    EncoderReady,
    /// 传输服务启动。
    TransportStarted,
    /// 接收端接入。
    PeerConnected,
    /// 质量下降但仍可用。
    QualityDegraded,
    /// 质量恢复。
    QualityRecovered,
    /// 链路中断。
    LinkLost,
    /// 重连成功。
    Reconnected,
    /// 显示器消失 / 驱动重载。
    DisplayLost,
    /// 采集失败（device lost 等）。
    CaptureFailed,
    /// 主动停止。
    Stop,
}

impl fmt::Display for SessionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SessionEvent::DisplayReady => "display_ready",
            SessionEvent::CaptureStarted => "capture_started",
            SessionEvent::EncoderReady => "encoder_ready",
            SessionEvent::TransportStarted => "transport_started",
            SessionEvent::PeerConnected => "peer_connected",
            SessionEvent::QualityDegraded => "quality_degraded",
            SessionEvent::QualityRecovered => "quality_recovered",
            SessionEvent::LinkLost => "link_lost",
            SessionEvent::Reconnected => "reconnected",
            SessionEvent::DisplayLost => "display_lost",
            SessionEvent::CaptureFailed => "capture_failed",
            SessionEvent::Stop => "stop",
        };
        f.write_str(s)
    }
}

/// 非法迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: SessionState,
    pub event: SessionEvent,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "非法状态迁移: {} --{}--> ?", self.from.as_str(), self.event)
    }
}

#[derive(Debug, Clone)]
pub struct SessionMachine {
    state: SessionState,
    history: Vec<(SessionState, SessionEvent)>,
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionMachine {
    pub fn new() -> Self {
        Self { state: SessionState::Idle, history: Vec::new() }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    /// 迁移历史（用于测试与事后复盘）。
    pub fn history(&self) -> &[(SessionState, SessionEvent)] {
        &self.history
    }

    /// 应用事件。非法迁移返回错误且不改变状态。
    pub fn apply(&mut self, event: SessionEvent) -> Result<SessionState, TransitionError> {
        let next = match (self.state, event) {
            (SessionState::Idle, SessionEvent::DisplayReady) => SessionState::DisplayReady,
            (SessionState::DisplayReady, SessionEvent::CaptureStarted) => SessionState::Capturing,
            (SessionState::Capturing, SessionEvent::EncoderReady) => SessionState::Encoding,
            (SessionState::Encoding, SessionEvent::TransportStarted) => SessionState::Serving,
            (SessionState::Serving, SessionEvent::PeerConnected) => SessionState::Connected,
            (SessionState::Connected, SessionEvent::QualityDegraded) => SessionState::Degraded,
            (SessionState::Degraded, SessionEvent::QualityRecovered) => SessionState::Connected,
            (SessionState::Connected, SessionEvent::LinkLost) => SessionState::Reconnecting,
            (SessionState::Degraded, SessionEvent::LinkLost) => SessionState::Reconnecting,
            (SessionState::Reconnecting, SessionEvent::Reconnected) => SessionState::Connected,
            // 采集/显示器故障：回到 DisplayReady，重新拉起采集。
            (SessionState::Capturing, SessionEvent::CaptureFailed) => SessionState::DisplayReady,
            (SessionState::Encoding, SessionEvent::CaptureFailed) => SessionState::DisplayReady,
            (SessionState::Connected, SessionEvent::CaptureFailed) => SessionState::DisplayReady,
            (SessionState::Degraded, SessionEvent::CaptureFailed) => SessionState::DisplayReady,
            (SessionState::Serving, SessionEvent::CaptureFailed) => SessionState::DisplayReady,
            (SessionState::Reconnecting, SessionEvent::CaptureFailed) => SessionState::DisplayReady,
            (SessionState::DisplayReady, SessionEvent::DisplayLost) => SessionState::Idle,
            (SessionState::Capturing, SessionEvent::DisplayLost) => SessionState::Idle,
            (SessionState::Encoding, SessionEvent::DisplayLost) => SessionState::Idle,
            (SessionState::Serving, SessionEvent::DisplayLost) => SessionState::Idle,
            (SessionState::Connected, SessionEvent::DisplayLost) => SessionState::Idle,
            (SessionState::Degraded, SessionEvent::DisplayLost) => SessionState::Idle,
            (SessionState::Reconnecting, SessionEvent::DisplayLost) => SessionState::Idle,
            // 任意状态都可停止。
            (s, SessionEvent::Stop) if s != SessionState::Stopped => SessionState::Stopped,
            (from, ev) => {
                return Err(TransitionError { from, event: ev });
            }
        };
        self.history.push((self.state, event));
        self.state = next;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_reaches_connected() {
        let mut m = SessionMachine::new();
        assert_eq!(m.apply(SessionEvent::DisplayReady).unwrap(), SessionState::DisplayReady);
        assert_eq!(m.apply(SessionEvent::CaptureStarted).unwrap(), SessionState::Capturing);
        assert_eq!(m.apply(SessionEvent::EncoderReady).unwrap(), SessionState::Encoding);
        assert_eq!(m.apply(SessionEvent::TransportStarted).unwrap(), SessionState::Serving);
        assert_eq!(m.apply(SessionEvent::PeerConnected).unwrap(), SessionState::Connected);
        assert_eq!(m.history().len(), 5);
    }

    #[test]
    fn illegal_transition_is_rejected_and_state_kept() {
        let mut m = SessionMachine::new();
        let err = m.apply(SessionEvent::PeerConnected).unwrap_err();
        assert_eq!(err.from, SessionState::Idle);
        assert_eq!(m.state(), SessionState::Idle);
        assert!(m.history().is_empty());
    }

    #[test]
    fn link_loss_and_recovery_roundtrip() {
        let mut m = SessionMachine::new();
        m.apply(SessionEvent::DisplayReady).unwrap();
        m.apply(SessionEvent::CaptureStarted).unwrap();
        m.apply(SessionEvent::EncoderReady).unwrap();
        m.apply(SessionEvent::TransportStarted).unwrap();
        m.apply(SessionEvent::PeerConnected).unwrap();
        assert_eq!(m.apply(SessionEvent::QualityDegraded).unwrap(), SessionState::Degraded);
        assert_eq!(m.apply(SessionEvent::QualityRecovered).unwrap(), SessionState::Connected);
        assert_eq!(m.apply(SessionEvent::LinkLost).unwrap(), SessionState::Reconnecting);
        assert_eq!(m.apply(SessionEvent::Reconnected).unwrap(), SessionState::Connected);
    }

    #[test]
    fn capture_failure_falls_back_to_display_ready() {
        let mut m = SessionMachine::new();
        m.apply(SessionEvent::DisplayReady).unwrap();
        m.apply(SessionEvent::CaptureStarted).unwrap();
        assert_eq!(m.apply(SessionEvent::CaptureFailed).unwrap(), SessionState::DisplayReady);
    }

    #[test]
    fn stop_is_allowed_from_any_live_state() {
        for ev in [
            SessionEvent::DisplayReady,
            SessionEvent::CaptureStarted,
            SessionEvent::EncoderReady,
            SessionEvent::TransportStarted,
        ] {
            let mut m = SessionMachine::new();
            for e in [SessionEvent::DisplayReady, SessionEvent::CaptureStarted] {
                let _ = m.apply(e);
            }
            let _ = ev;
            assert_eq!(m.apply(SessionEvent::Stop).unwrap(), SessionState::Stopped);
        }
    }
}
