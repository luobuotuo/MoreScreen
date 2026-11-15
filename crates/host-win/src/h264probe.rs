//! P1.1 技术验证工具：H.264 Annex B 解析 + webrtc-rs `H264Payloader` 兼容性验证。
//!
//! 背景（2026-11-15 实测结论）：
//! webrtc-rs 的 `H264Payloader` **自动完成** STAP-A 聚合（含 SPS/PPS 携带）与 FU-A 分片，
//! 且直接接受 Annex B 输入——正是我们 QSV 编码器的输出格式。
//! 因此本项目原有的自研 AU 聚合 / 参数集补齐 / `first_mb_in_slice` 解析逻辑
//! （`encode.rs` 中约 200 行）在 WebRTC 路线下不再需要。
//!
//! 本模块保留这些工具函数，用于：
//! - 回归验证（升级 webrtc/rtp 版本后确认兼容性未破坏）；
//! - 诊断（码流结构异常时快速定位）。

/// 把 Annex B 数据按起始码切成 NAL 列表。
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                if let Some(s) = start {
                    out.push(&data[s..i]);
                }
                start = Some(i + 3);
                i += 3;
                continue;
            }
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                if let Some(s) = start {
                    out.push(&data[s..i]);
                }
                start = Some(i + 4);
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    if let Some(s) = start {
        if s < data.len() {
            out.push(&data[s..]);
        }
    }
    out
}

/// NAL 类型名称（用于日志与诊断输出）。
pub fn nal_type_name(t: u8) -> String {
    match t {
        1 => "P(non-IDR)".to_string(),
        5 => "IDR".to_string(),
        6 => "SEI".to_string(),
        7 => "SPS".to_string(),
        8 => "PPS".to_string(),
        9 => "AUD".to_string(),
        12 => "FILLER".to_string(),
        other => format!("type{other}"),
    }
}

/// 对一段 Annex B H.264 做 payload → depacketize 往返验证。
///
/// 返回 `(payload 列表, 还原出的 NAL 数, 还原字节数)`。
pub fn roundtrip(annexb: &[u8], mtu: usize) -> (Vec<bytes::Bytes>, usize, usize) {
    use bytes::Bytes;
    use rtp::codecs::h264::{H264Packet, H264Payloader};
    use rtp::packetizer::{Depacketizer, Payloader};

    let mut payer = H264Payloader::default();
    let payloads = match payer.payload(mtu, &Bytes::copy_from_slice(annexb)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[h264probe] payload 失败: {e}");
            return (Vec::new(), 0, 0);
        }
    };

    let mut dep = H264Packet::default();
    let mut nals: Vec<u8> = Vec::new();
    for p in &payloads {
        if let Ok(out) = dep.depacketize(p) {
            nals.extend_from_slice(out.as_ref());
        }
    }
    (payloads, split_annexb(&nals).len(), nals.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小可用的 Annex B 序列：SPS + PPS + IDR(小)。
    fn sample_au() -> Vec<u8> {
        let mut v = Vec::new();
        // 起始码 + SPS(type 7)
        v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F]);
        // 起始码 + PPS(type 8)
        v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
        // 起始码 + IDR(type 5)，载荷 2000 字节以触发 FU-A
        v.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        v.extend(std::iter::repeat(0xAB).take(2000));
        v
    }

    #[test]
    fn split_annexb_finds_all_nals() {
        let au = sample_au();
        let nals = split_annexb(&au);
        assert_eq!(nals.len(), 3, "应切出 SPS/PPS/IDR 三个 NAL");
        assert_eq!(nals[0][0] & 0x1F, 7);
        assert_eq!(nals[1][0] & 0x1F, 8);
        assert_eq!(nals[2][0] & 0x1F, 5);
        assert_eq!(nals[2].len(), 2001);
    }

    #[test]
    fn roundtrip_preserves_nal_count() {
        let au = sample_au();
        let (payloads, restored, _bytes) = roundtrip(&au, 1200);
        // STAP-A(SPS+PPS) 1 个 + IDR 2000 字节分 2 个 FU-A
        assert!(payloads.len() >= 2, "至少应有 STAP-A 与 FU-A，实际 {}", payloads.len());
        assert_eq!(restored, 3, "还原后仍应是 3 个 NAL");
    }

    #[test]
    fn nal_type_name_covers_common_types() {
        assert_eq!(nal_type_name(5), "IDR");
        assert_eq!(nal_type_name(7), "SPS");
        assert_eq!(nal_type_name(1), "P(non-IDR)");
        assert_eq!(nal_type_name(99), "type99");
    }
}
