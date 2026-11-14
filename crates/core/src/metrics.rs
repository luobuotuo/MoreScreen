//! 链路指标：逐帧分段打点，输出 JSONL。
//!
//! 核心原则：同一个 `frame_id` 贯穿 采集 → 编码 → 发送 全生命周期，
//! 这样才能把"延迟高"定位到具体某一段，而不是只看到一个总延迟数字。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

/// 一帧的完整生命周期打点（时间戳统一为 QPC ticks）。
#[derive(Debug, Clone, Default)]
pub struct FrameRecord {
    pub session_id: u64,
    pub display_id: String,
    pub frame_id: u64,
    /// 采集源时间戳（WGC `SystemRelativeTime` 对齐的 QPC 基准）。
    pub capture_src_ts: u64,
    /// 从帧池取出该帧的本地 QPC。
    pub capture_deq_ts: u64,
    /// 提交编码的本地 QPC。
    pub enc_submit_ts: u64,
    /// 编码输出的本地 QPC。
    pub enc_out_ts: u64,
    /// 交给传输层的本地 QPC。
    pub send_ts: u64,
    /// 编码后字节数。
    pub bytes: u32,
    /// 是否关键帧。
    pub keyframe: bool,
    /// 丢帧原因（None 表示未丢）。
    pub drop_reason: Option<String>,
}

/// 指标写入器：JSONL，逐行追加，支持抽样与周期 flush。
pub struct MetricsRecorder {
    writer: BufWriter<File>,
    /// QPC 频率（ticks per second），用于把 ticks 换算成毫秒。
    qpc_freq: f64,
    /// 抽样间隔：每 N 帧写一条。
    sample_every: u64,
    /// 已见帧数（用于抽样判定，与实际写入条数分开）。
    seen: u64,
    written: u64,
    dropped_by_recorder: u64,
}

impl MetricsRecorder {
    /// 打开（或追加）一个 JSONL 指标文件。
    pub fn create(path: &Path, qpc_freq: f64, sample_every: u64) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            qpc_freq: if qpc_freq > 0.0 { qpc_freq } else { 1.0 },
            sample_every: sample_every.max(1),
            seen: 0,
            written: 0,
            dropped_by_recorder: 0,
        })
    }

    /// 记录一帧。返回是否真正写盘（被抽样丢弃时返回 false）。
    pub fn record(&mut self, rec: &FrameRecord) -> std::io::Result<bool> {
        let should_write = self.seen % self.sample_every == 0;
        self.seen += 1;
        if !should_write {
            self.dropped_by_recorder += 1;
            return Ok(false);
        }
        let ms = |a: u64, b: u64| -> f64 {
            if a == 0 || b == 0 || b < a {
                return 0.0;
            }
            (b - a) as f64 * 1000.0 / self.qpc_freq
        };
        let line = format!(
            "{{\"session_id\":{},\"display_id\":{},\"frame_id\":{},\
\"capture_src_ts\":{},\"capture_deq_ts\":{},\"enc_submit_ts\":{},\"enc_out_ts\":{},\"send_ts\":{},\
\"capture_ms\":{:.3},\"encode_ms\":{:.3},\"to_send_ms\":{:.3},\"pipe_ms\":{:.3},\
\"bytes\":{},\"keyframe\":{},\"drop\":{}}}\n",
            rec.session_id,
            json_escape(&rec.display_id),
            rec.frame_id,
            rec.capture_src_ts,
            rec.capture_deq_ts,
            rec.enc_submit_ts,
            rec.enc_out_ts,
            rec.send_ts,
            ms(rec.capture_src_ts, rec.capture_deq_ts),
            ms(rec.enc_submit_ts, rec.enc_out_ts),
            ms(rec.enc_out_ts, rec.send_ts),
            ms(rec.capture_src_ts, rec.send_ts),
            rec.bytes,
            rec.keyframe,
            match &rec.drop_reason {
                Some(d) => format!("\"{}\"", json_escape(d)),
                None => "null".to_string(),
            }
        );
        self.writer.write_all(line.as_bytes())?;
        self.written += 1;
        Ok(true)
    }

    /// 写入一条事件记录（状态迁移、错误等）。
    pub fn event(&mut self, session_id: u64, kind: &str, detail: &str) -> std::io::Result<()> {
        let line = format!(
            "{{\"session_id\":{},\"kind\":\"{}\",\"detail\":\"{}\"}}\n",
            session_id,
            json_escape(kind),
            json_escape(detail)
        );
        self.writer.write_all(line.as_bytes())?;
        Ok(())
    }

    /// 刷盘并返回已写入条数。
    pub fn flush(&mut self) -> std::io::Result<u64> {
        self.writer.flush()?;
        Ok(self.written)
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn dropped_by_sampler(&self) -> u64 {
        self.dropped_by_recorder
    }
}

/// 极简 JSON 字符串转义（避免为一行日志引入 serde 依赖）。
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 把 Instant 差值换算成毫秒（用于日志打印，不进入 JSONL）。
pub fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_handles_control_chars() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("x\ny"), "x\\ny");
    }

    #[test]
    fn record_writes_jsonl_and_samples() {
        let dir = std::env::temp_dir().join("rs_metrics_test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("m.jsonl");
        let mut rec = MetricsRecorder::create(&path, 1_000_000.0, 2).unwrap();
        for i in 0..4u64 {
            let fr = FrameRecord {
                session_id: 1,
                display_id: "D1".into(),
                frame_id: i,
                capture_src_ts: 0,
                capture_deq_ts: 1_000,
                enc_submit_ts: 2_000,
                enc_out_ts: 4_000,
                send_ts: 5_000,
                bytes: 100,
                keyframe: i == 0,
                drop_reason: None,
            };
            rec.record(&fr).unwrap();
        }
        rec.flush().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "sample_every=2 时应只写一半");
        assert!(content.contains("\"frame_id\":0"));
        assert!(content.contains("\"encode_ms\":2.000"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
