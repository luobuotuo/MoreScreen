//! H.264 编码（POC 版本：借助 ffmpeg 子进程）。
//!
//! 为什么先用 ffmpeg：它的编码质量与实时性足够验证"链路是否成立"，
//! 而 Media Foundation 的 H.264 MFT 需要几百行 FFI、且在虚拟屏场景下
//! 还要额外处理 D3D11 输入绑定。先把链路打通，再把编码器换成自研 MFT，
//! 上层接口（`Encoder`）保持不变。
//!
//! 数据流：NV12 帧 → ffmpeg stdin → H.264 (Annex B) stdout → 访问单元切分。

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

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
    /// 送帧通道：独立写线程负责阻塞写 ffmpeg stdin，主循环只做非阻塞发送。
    /// 否则 1080p BGRA 每帧 8.3MB 的管道写入会把采集循环堵死。
    frame_tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    /// 写线程句柄（结束时要等它把残余帧写完）
    writer: Option<std::thread::JoinHandle<()>>,
    /// 编码输出通道：独立读线程持续读取 ffmpeg stdout，主循环只做非阻塞收取。
    ///
    /// 必须解耦：硬件编码器（如 QSV async_depth=1）常常"等到下一帧才吐数据"，
    /// 若在推帧后阻塞等待输出，就会与编码器互相等待而死锁。
    rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    /// 累积的 Annex B 字节缓冲（等待切分成访问单元）
    pending: Vec<u8>,
    /// 输入像素格式（bgra / nv12）
    pix_fmt: String,
    /// 单帧的期望字节数
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
    /// 输入像素格式："bgra"（默认，由 ffmpeg SIMD 转 NV12）或 "nv12"
    pub pix_fmt: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// 是否强制关键帧间隔（帧数）
    pub gop: u32,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 30, bitrate_kbps: 6000, gop: 30, pix_fmt: "bgra".into() }
    }
}

/// 探测本机可用的 H.264 编码器（按硬件优先）。
///
/// 返回编码器名；`RS_ENCODER` 环境变量可强制指定。
pub fn detect_encoder(ffmpeg_path: &str) -> String {
    if let Ok(pref) = std::env::var("RS_ENCODER") {
        let pref = pref.trim().to_string();
        if !pref.is_empty() {
            return pref;
        }
    }
    // 硬件编码优先：Intel QSV -> NVIDIA NVENC -> AMD AMF -> 软件 libx264
    for cand in ["h264_qsv", "h264_nvenc", "h264_amf", "libx264"] {
        if encoder_works(ffmpeg_path, cand) {
            return cand.to_string();
        }
    }
    "libx264".to_string()
}

/// 用一小段合成画面验证编码器是否真的能跑。
fn encoder_works(ffmpeg_path: &str, enc: &str) -> bool {
    let out = Command::new(ffmpeg_path)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=320x240:r=5",
            "-frames:v",
            "2",
            "-c:v",
            enc,
            "-f",
            "null",
            "-",
        ])
        .output();
    matches!(out, Ok(o) if o.status.success())
}

impl FfmpegEncoder {
    /// 启动 ffmpeg 编码进程（低延迟参数）。
    pub fn start(cfg: &EncodeConfig, ffmpeg_path: &str, encoder: &str) -> Result<Self, String> {
        let bps = format!("{}k", cfg.bitrate_kbps);
        let gop = cfg.gop.to_string();
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            cfg.pix_fmt.clone(),
            "-s".into(),
            format!("{}x{}", cfg.width, cfg.height),
            // 恢复 `-r`：WebRTC 层的节拍器已保证 CFR 输出节奏，
            // 编码器侧再用 wallclock 会引入两套独立时钟（wallclock PTS vs RTP 节拍），
            // 反而制造 micro-burst——节拍器排空队列时出现"0 帧/2 帧"的取帧抖动，
            // 这正是 jitterBuffer 判定"迟到 120ms"并持续丢帧的来源。
            // 用固定 -r 让 ffmpeg 按 CFR 打 PTS，与节拍器节奏对齐。
            "-r".into(),
            cfg.fps.to_string(),
            "-i".into(),
            "-".into(),
        ];
        match encoder {
            "h264_qsv" => {
                args.extend([
                    "-c:v".into(), "h264_qsv".into(),
                    "-preset".into(), "veryfast".into(),
                    "-async_depth".into(), "1".into(),   // 低延迟：不排队
                    "-look_ahead".into(), "0".into(),
                    "-forced_idr".into(), "1".into(),
                    "-b:v".into(), bps,
                    "-g".into(), gop.clone(),
                ]);
            }
            "h264_nvenc" => {
                args.extend([
                    "-c:v".into(), "h264_nvenc".into(),
                    "-preset".into(), "p1".into(),
                    "-tune".into(), "ull".into(),
                    "-zerolatency".into(), "1".into(),
                    "-forced_idr".into(), "1".into(),
                    "-b:v".into(), bps,
                    "-g".into(), gop.clone(),
                ]);
            }
            "h264_amf" => {
                args.extend([
                    "-c:v".into(), "h264_amf".into(),
                    "-quality".into(), "speed".into(),
                    "-rc".into(), "cqp".into(),
                    "-qp_i".into(), "24".into(),
                    "-qp_p".into(), "24".into(),
                    "-b:v".into(), bps,
                    "-g".into(), gop.clone(),
                ]);
            }
            _ => {
                args.extend([
                    "-c:v".into(), "libx264".into(),
                    "-preset".into(), "ultrafast".into(),
                    "-tune".into(), "zerolatency".into(),
                    "-x264-params".into(), "nal-hrd=cbr:force-cfr=1".into(),
                    "-b:v".into(), bps,
                    "-g".into(), gop.clone(),
                    "-keyint_min".into(), gop.clone(),
                    "-sc_threshold".into(), "0".into(),
                ]);
            }
        }
        args.extend(["-bsf:v".into(), "h264_mp4toannexb".into(), "-f".into(), "h264".into(), "-".into()]);
        let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut child = Command::new(ffmpeg_path)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动 ffmpeg 失败（{ffmpeg_path}）: {e}"))?;
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (rtx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Some(mut out) = stdout {
            std::thread::spawn(move || {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match out.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if rtx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        // 写线程：独占 stdin，把帧数据阻塞写入 ffmpeg
        // 有界通道：容量 2 帧。满了就丢弃新帧而不是无限排队，
        // 否则延迟会随时间持续增长（实测无界时 p50 达 872ms）。
        let (ftx, frx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        let writer = stdin.take().map(|mut si| {
            std::thread::spawn(move || {
                while let Ok(buf) = frx.recv() {
                    if si.write_all(&buf).is_err() {
                        break;
                    }
                }
                // 通道关闭即输入结束，drop stdin 让 ffmpeg flush
            })
        });
        Ok(Self {
            child,
            frame_tx: Some(ftx),
            writer,
            rx: Some(rx),
            pending: Vec::new(),
            pix_fmt: cfg.pix_fmt.clone(),
            expected_len: if cfg.pix_fmt == "nv12" {
                (cfg.width as usize) * (cfg.height as usize) * 3 / 2
            } else {
                (cfg.width as usize) * (cfg.height as usize) * 4
            },
            sps: None,
            pps: None,
            units_emitted: 0,
            frames_pushed: 0,
        })
    }

    /// 写入一帧原始像素（BGRA 或 NV12，取决于启动时的 pix_fmt）。
    ///
    /// 非阻塞：只把数据送进通道，由独立写线程负责写 ffmpeg stdin。
    /// 返回 `Ok(true)` 表示已入队，`Ok(false)` 表示**因背压被丢弃**
    /// （编码器消费不过来时主动丢帧，避免延迟无限增长）。
    pub fn push_frame(&mut self, frame: &[u8]) -> Result<bool, String> {
        let expected = self.self_expected_len();
        if frame.len() != expected {
            return Err(format!(
                "帧长度不符（{}）：期望 {}，实际 {}",
                self.pix_fmt,
                expected,
                frame.len()
            ));
        }
        let tx = self
            .frame_tx
            .as_ref()
            .ok_or_else(|| "送帧通道已关闭".to_string())?;
        match tx.try_send(frame.to_vec()) {
            Ok(()) => Ok(true),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(false),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                Err("写线程已退出，无法送帧".to_string())
            }
        }
    }

    /// 读取并切分输出访问单元（阻塞直到有数据）。
    fn self_expected_len(&self) -> usize {
        self.expected_len
    }

    /// 非阻塞收取当前已可用的访问单元（不等待编码器）。
    pub fn read_units(&mut self) -> Result<Vec<AccessUnit>, String> {
        let mut got = false;
        if let Some(rx) = self.rx.as_ref() {
            while let Ok(chunk) = rx.try_recv() {
                self.pending.extend_from_slice(&chunk);
                got = true;
            }
        }
        if !got && self.pending.is_empty() {
            return Ok(Vec::new());
        }
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

            if is_vcl {
                // 访问单元边界判定：x264 一帧可能切成多个 slice，
                // 只有 first_mb_in_slice == 0 的 slice 才是新访问单元的开始。
                let new_au = starts_new_au(&data[body + 1..end]);
                if new_au && cur_has_vcl {
                    if let Some(cs) = cur_start {
                        units.push(data[cs..start].to_vec());
                        consumed = start;
                        cur_start = Some(start);
                    }
                    cur_has_vcl = false;
                }
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
        // 关闭送帧通道并等待写线程把残余帧写入 ffmpeg，再 flush
        self.frame_tx.take();
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        let mut units = Vec::new();
        // 等待读线程把剩余输出推完（通道断开即结束）
        if let Some(rx) = self.rx.as_ref() {
            while let Ok(chunk) = rx.recv() {
                self.pending.extend_from_slice(&chunk);
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


/// 去掉防竞争字节（emulation prevention）：00 00 03 -> 00 00。
fn strip_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

/// 读取一个无符号指数哥伦布码 ue(v)。
fn read_ue(data: &[u8], bitpos: &mut usize) -> Option<u32> {
    let mut zeros = 0u32;
    loop {
        let byte = *data.get(*bitpos >> 3)?;
        let bit = (byte >> (7 - (*bitpos & 7))) & 1;
        *bitpos += 1;
        if bit == 1 {
            break;
        }
        zeros += 1;
        if zeros > 32 {
            return None;
        }
    }
    if zeros == 0 {
        return Some(0);
    }
    let mut v: u32 = 1;
    for _ in 0..zeros {
        let byte = *data.get(*bitpos >> 3)?;
        let bit = (byte >> (7 - (*bitpos & 7))) & 1;
        *bitpos += 1;
        v = (v << 1) | bit as u32;
    }
    Some(v - 1)
}

/// 判断该 slice 是否开启一个新的访问单元（first_mb_in_slice == 0）。
///
/// `payload` 为去掉 NAL 头之后的 slice 负载。
fn starts_new_au(payload: &[u8]) -> bool {
    let rbsp = strip_emulation_prevention(payload);
    let mut bitpos = 0usize;
    match read_ue(&rbsp, &mut bitpos) {
        Some(0) => true,
        Some(_) => false,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 slice 负载：first_mb_in_slice 用 ue(v) 编码。
    fn slice_payload(first_mb: u32) -> Vec<u8> {
        // ue(v) 编码：前导零个数 = floor(log2(v+1))
        let v = first_mb + 1;
        let mut bits: Vec<bool> = Vec::new();
        let mut n = v;
        let mut len = 0usize;
        while (1usize << len) < v as usize + 1 {
            len += 1;
        }
        n = v;
        let _ = n;
        // 简化：len 位表示 (v+1) 的二进制长度
        let zeros = len - 1;
        for _ in 0..zeros {
            bits.push(false);
        }
        let mut tmp = v;
        for i in (0..zeros).rev() {
            bits.push((tmp >> i) & 1 == 1);
        }
        let mut out: Vec<u8> = Vec::new();
        let mut cur: u8 = 0;
        let mut nbits = 0u8;
        for b in bits {
            cur = (cur << 1) | (b as u8);
            nbits += 1;
            if nbits == 8 {
                out.push(cur);
                cur = 0;
                nbits = 0;
            }
        }
        if nbits > 0 {
            out.push(cur << (8 - nbits));
        }
        out
    }

    #[test]
    fn first_mb_zero_starts_new_au() {
        assert!(starts_new_au(&slice_payload(0)));
    }

    #[test]
    fn non_zero_first_mb_continues_au() {
        assert!(!starts_new_au(&slice_payload(5)));
        assert!(!starts_new_au(&slice_payload(120)));
    }

    #[test]
    fn emulation_prevention_is_stripped() {
        let raw = [0x00u8, 0x00, 0x03, 0x01];
        assert_eq!(strip_emulation_prevention(&raw), vec![0x00, 0x00, 0x01]);
    }
}
