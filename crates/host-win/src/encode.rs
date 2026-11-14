//! H.264 编码（POC 版本：借助 ffmpeg 子进程）。
//!
//! 为什么先用 ffmpeg：它的编码质量与实时性足够验证"链路是否成立"，
//! 而 Media Foundation 的 H.264 MFT 需要几百行 FFI、且在虚拟屏场景下
//! 还要额外处理 D3D11 输入绑定。先把链路打通，再把编码器换成自研 MFT，
//! 上层接口（`Encoder`）保持不变。
//!
//! 数据流：NV12 帧 → ffmpeg stdin → H.264 (Annex B) stdout → 访问单元切分。

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// 一次编码会话的输出：若干个 Annex B 访问单元。
pub type AccessUnit = Vec<u8>;

/// 带关键帧标记的访问单元（浏览器需要知道哪些 chunk 是 key）。
#[derive(Debug, Clone)]
pub struct FrameChunk {
    pub data: Vec<u8>,
    pub key: bool,
}

/// 基于 ffmpeg 子进程的 H.264 编码器。
pub struct FfmpegEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    /// 累积的 Annex B 字节缓冲（等待切分成访问单元）
    pending: Vec<u8>,
    /// 单帧 NV12 的期望字节数
    expected_len: usize,
    /// 最近一次看到的 SPS（NAL type 7）
    sps: Option<Vec<u8>>,
    /// 最近一次看到的 PPS（NAL type 8）
    pps: Option<Vec<u8>>,
    /// 已切出的访问单元数量
    pub units_emitted: u64,
    /// 喂进去的帧数
    pub frames_pushed: u64,
}

/// 编码参数。
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// 是否强制关键帧间隔（帧数）
    pub gop: u32,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 30, bitrate_kbps: 6000, gop: 30 }
    }
}

impl FfmpegEncoder {
    /// 启动 ffmpeg 编码进程（低延迟参数）。
    pub fn start(cfg: &EncodeConfig, ffmpeg_path: &str) -> Result<Self, String> {
        let args = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "-s",
            &format!("{}x{}", cfg.width, cfg.height),
            "-r",
            &cfg.fps.to_string(),
            "-i",
            "-",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-b:v",
            &format!("{}k", cfg.bitrate_kbps),
            "-g",
            &cfg.gop.to_string(),
            "-keyint_min",
            &cfg.gop.to_string(),
            "-sc_threshold",
            "0",
            "-x264-params",
            "nal-hrd=cbr:force-cfr=1",
            "-bsf:v",
            "h264_mp4toannexb",
            "-f",
            "h264",
            "-",
        ];
        let mut child = Command::new(ffmpeg_path)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动 ffmpeg 失败（{ffmpeg_path}）: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        Ok(Self {
            child,
            stdin,
            stdout,
            pending: Vec::new(),
            expected_len: (cfg.width as usize) * (cfg.height as usize) * 3 / 2,
            sps: None,
            pps: None,
            units_emitted: 0,
            frames_pushed: 0,
        })
    }

    /// 写入一帧 NV12。
    pub fn push_nv12(&mut self, frame: &[u8]) -> Result<(), String> {
        let expected = self.self_expected_len();
        if frame.len() != expected {
            return Err(format!(
                "NV12 长度不符：期望 {}，实际 {}",
                expected,
                frame.len()
            ));
        }
        if let Some(stdin) = self.stdin.as_mut() {
            stdin
                .write_all(frame)
                .map_err(|e| format!("写 ffmpeg stdin 失败: {e}"))?;
            self.frames_pushed += 1;
        }
        Ok(())
    }

    fn self_expected_len(&self) -> usize {
        self.expected_len
    }

    /// 读取并切分输出访问单元（阻塞直到有数据）。
    pub fn read_units(&mut self) -> Result<Vec<AccessUnit>, String> {
        let mut chunk = [0u8; 64 * 1024];
        let stdout = self
            .stdout
            .as_mut()
            .ok_or_else(|| "ffmpeg stdout 已关闭".to_string())?;
        let n = stdout
            .read(&mut chunk)
            .map_err(|e| format!("读 ffmpeg stdout 失败: {e}"))?;
        if n == 0 {
            return Ok(Vec::new());
        }
        self.pending.extend_from_slice(&chunk[..n]);
        Ok(self.split_units())
    }

    /// 按"访问单元"聚合 Annex B 分片。
    ///
    /// 关键点：**不能按 NAL 简单切分**。SPS/PPS/SEI 等非 VCL 分片必须和它所描述的
    /// 图像帧放在一起，否则浏览器端拿不到参数集，IDR 会解不出来。
    /// 规则：一个访问单元以参数集开头、以一个 VCL NAL（type 1~5）结束；
    /// 遇到下一个 VCL 时，上一个访问单元才算完整。
    fn split_units(&mut self) -> Vec<AccessUnit> {
        let data = &self.pending;
        // 1) 找出所有起始码
        let mut marks: Vec<(usize, usize, usize)> = Vec::new(); // (起始位置, 起始码长度, NAL 头位置)
        let mut i = 0usize;
        while i + 2 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                if data[i + 2] == 1 {
                    marks.push((i, 3, i + 3));
                    i += 3;
                    continue;
                }
                if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                    marks.push((i, 4, i + 4));
                    i += 4;
                    continue;
                }
            }
            i += 1;
        }
        if marks.is_empty() {
            return Vec::new();
        }

        // 2) 按 VCL 边界聚合
        let mut units: Vec<AccessUnit> = Vec::new();
        let mut cur_start: Option<usize> = None;
        let mut cur_has_vcl = false;
        let mut last_vcl_end: Option<usize> = None;
        let mut consumed = 0usize;

        for (idx, &(start, _slen, body)) in marks.iter().enumerate() {
            let end = marks.get(idx + 1).map(|m| m.0).unwrap_or(data.len());
            let nal_type = data[body] & 0x1F;
            let is_vcl = (1..=5).contains(&nal_type);

            if is_vcl && cur_has_vcl {
                // 上一个访问单元到本 NAL 之前结束
                if let Some(cs) = cur_start {
                    units.push(data[cs..start].to_vec());
                    consumed = start;
                    cur_start = Some(start);
                }
                cur_has_vcl = false;
            }
            if cur_start.is_none() {
                cur_start = Some(start);
            }
            // 缓存参数集：SPS/PPS 在码流里通常只出现一次，
            // 但客户端随时可能接入，因此要记住并给后续关键帧补上。
            match nal_type {
                7 => self.sps = Some(data[start..end].to_vec()),
                8 => self.pps = Some(data[start..end].to_vec()),
                _ => {}
            }
            if is_vcl {
                cur_has_vcl = true;
                last_vcl_end = Some(end);
            }
        }

        // 3) 收尾：若末尾 VCL 之后已有后续字节，说明该 VCL 已完整，可立即输出
        if cur_has_vcl {
            if let (Some(cs), Some(lv_end)) = (cur_start, last_vcl_end) {
                if lv_end < data.len() {
                    units.push(data[cs..lv_end].to_vec());
                    consumed = lv_end;
                }
            }
        }

        if consumed > 0 {
            self.pending.drain(..consumed);
        }

        // 给缺少参数集的关键帧补齐 SPS/PPS
        let sps = self.sps.clone();
        let pps = self.pps.clone();
        let mut fixed: Vec<AccessUnit> = Vec::with_capacity(units.len());
        for u in units {
            let types = nal_types(&u);
            let has_idr = types.iter().any(|t| *t == 5);
            let has_sps = types.iter().any(|t| *t == 7);
            if has_idr && !has_sps {
                let mut merged = Vec::with_capacity(u.len() + 64);
                if let Some(sp) = &sps {
                    merged.extend_from_slice(sp);
                }
                if let Some(pp) = &pps {
                    merged.extend_from_slice(pp);
                }
                merged.extend_from_slice(&u);
                fixed.push(merged);
            } else {
                fixed.push(u);
            }
        }
        for _ in 0..fixed.len() {
            self.units_emitted += 1;
        }
        fixed
    }

    /// 取出访问单元并标注关键帧（访问单元内含 NAL type 5 = IDR 即为关键帧）。
    pub fn read_chunks(&mut self) -> Result<Vec<FrameChunk>, String> {
        let units = self.read_units()?;
        let mut out = Vec::with_capacity(units.len());
        for u in units {
            let key = nal_types(&u).iter().any(|t| *t == 5);
            out.push(FrameChunk { data: u, key });
        }
        Ok(out)
    }

    /// 结束编码并收集剩余输出。
    pub fn finish(&mut self) -> Result<Vec<AccessUnit>, String> {
        self.stdin.take();
        let mut units = Vec::new();
        if let Some(stdout) = self.stdout.as_mut() {
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        self.pending.extend_from_slice(&chunk[..n]);
                    }
                    Err(e) => return Err(format!("读 ffmpeg stdout 失败: {e}")),
                }
            }
        }
        // 收尾：把剩余 pending 作为一个单元吐出
        if !self.pending.is_empty() {
            let unit = std::mem::take(&mut self.pending);
            if unit.len() > 4 {
                units.push(unit);
                self.units_emitted += 1;
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(units)
    }
}


/// 扫描 Annex B 数据中的 NAL 类型序列（用于判断关键帧/参数集）。
pub fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                types.push(data[i + 3] & 0x1F);
                i += 3;
                continue;
            }
            if data[i + 2] == 0 && data[i + 3] == 1 {
                if i + 4 < data.len() {
                    types.push(data[i + 4] & 0x1F);
                }
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    types
}
