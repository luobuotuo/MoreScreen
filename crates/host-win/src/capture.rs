
//! Windows 屏幕采集（Windows.Graphics.Capture / D3D11）。
//!
//! 目标：只抓"指定的那一块显示器"，并且从第一帧开始就带时间戳，
//! 这样后面的编码、网络、解码耗时才能分段归因。
//!
//! 关键点：
//! - 每块屏独立 CaptureItem / FramePool / Session，不共享；
//! - 用 `TryGetNextFrame` 轮询而非事件回调，避免跨线程事件复杂度（首版）；
//! - 帧面保持在 GPU（D3D11 表面），需要像素时才回读；
//! - 记录 `SystemRelativeTime`（系统时钟）与本地 QPC 两套时间戳。

use std::path::Path;
use std::time::Instant;

use crate::readback::{analyze_surface, read_surface_bgra, FrameContent};
use windows::core::{factory, Interface};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const D3D11_SDK_VERSION: u32 = 7;

/// 采集统计：一次采集会话的实测结果。
#[derive(Debug, Clone, Default)]
pub struct CaptureStats {
    pub frames: u64,
    /// 静止保活补帧数：画面无变化时按目标帧率重复上一帧，**不计入 frames**，
    /// 以免「采集结束: N 帧 / X fps」被保活帧虚高。
    pub keepalive_frames: u64,
    pub first_frame_delay_ms: f64,
    pub avg_interval_ms: f64,
    pub max_interval_ms: f64,
    pub content_width: u32,
    pub content_height: u32,
    pub min_interval_ms: f64,
    /// 首帧内容自检（亮度）
    pub content: Option<FrameContent>,
    /// 实时链路的平均亮度（抽样）
    pub luma_avg: f64,
    /// 实时链路的非黑占比（抽样）
    pub non_black_ratio: f64,
    /// 导出的 NV12 帧路径（若请求）
    pub nv12_path: Option<String>,
}

/// 采集帧率诊断：抓取指定时长，把每一帧的到达时刻与帧间隔落盘，
/// 用于定位"丢帧发生在采集侧还是传输侧"。
///
/// 这是独立于 stream 链路的纯采集测量：跳过编码与 WebRTC，
/// 只记录 `TryGetNextFrame` 拿到帧的时刻。若这里就出现大间隔/丢帧，
/// 问题在 WGC 采集；若这里平滑，问题在后面的编码/传输。
pub fn diagnose_capture(
    bounds_x: i32,
    bounds_y: i32,
    width: u32,
    height: u32,
    duration_ms: u64,
    target_fps: u32,
    out_path: &Path,
) -> Result<String, String> {
    unsafe {
        let hmonitor = monitor_from_bounds(bounds_x, bounds_y, width, height);
        if hmonitor.0.is_null() {
            return Err("MonitorFromPoint 未返回有效 HMONITOR".into());
        }
        let (d3d_device, d3d_context, winrt_device) = create_device()?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(|e| format!("获取 IGraphicsCaptureItemInterop 失败: {e}"))?;
        let item: GraphicsCaptureItem = interop
            .CreateForMonitor(hmonitor)
            .map_err(|e| format!("CreateForMonitor 失败: {e}"))?;
        let size = SizeInt32 { Width: width as i32, Height: height as i32 };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| format!("创建帧池失败: {e}"))?;
        let session = pool.CreateCaptureSession(&item).map_err(|e| format!("创建会话失败: {e}"))?;
        session.StartCapture().map_err(|e| format!("StartCapture 失败: {e}"))?;

        let start = Instant::now();
        let mut last_ts: Option<Instant> = None;
        let mut last_push: Option<Instant> = None;
        let mut accepted = 0u64;
        let mut skipped = 0u64;
        let min_interval = std::time::Duration::from_micros(1_000_000 / target_fps.max(1) as u64 * 9 / 10);
        let mut lines: Vec<String> = Vec::new();
        lines.push("idx\tarrival_ms\tinterval_ms\taccepted".to_string());

        while (start.elapsed().as_millis() as u64) < duration_ms {
            match pool.TryGetNextFrame() {
                Ok(_frame) => {
                    let now = Instant::now();
                    let interval = last_ts
                        .map(|t| now.duration_since(t).as_secs_f64() * 1000.0)
                        .unwrap_or(0.0);
                    last_ts = Some(now);
                    let accept = match last_push {
                        Some(lp) if now.duration_since(lp) < min_interval => false,
                        _ => true,
                    };
                    if accept {
                        last_push = Some(now);
                        accepted += 1;
                    } else {
                        skipped += 1;
                    }
                    lines.push(format!(
                        "{}\t{:.2}\t{:.2}\t{}",
                        accepted + skipped,
                        now.duration_since(start).as_secs_f64() * 1000.0,
                        interval,
                        accept
                    ));
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        let _ = session.Close();
        let _ = d3d_context;
        let _ = d3d_device;
        std::fs::write(out_path, lines.join("\n")).map_err(|e| format!("写诊断文件失败: {e}"))?;
        Ok(format!(
            "采集诊断完成：共拿到 {} 帧，接受 {accepted}，限速跳过 {skipped}，已写入 {}",
            accepted + skipped,
            out_path.display()
        ))
    }
}

/// 创建一个 D3D11 设备 + 立即上下文，并包装成 WinRT `IDirect3DDevice`（WGC 需要）。
unsafe fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice), String> {
    let feature_levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_12_0,
    ];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&feature_levels),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .map_err(|e| format!("D3D11CreateDevice 失败: {e}"))?;
    let device = device.ok_or_else(|| "D3D11CreateDevice 未返回设备".to_string())?;
    let context = context.ok_or_else(|| "D3D11CreateDevice 未返回上下文".to_string())?;

    // D3D11 设备 -> DXGI 设备 -> WinRT IDirect3DDevice
    let dxgi_device = device
        .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()
        .map_err(|e| format!("转换为 IDXGIDevice 失败: {e}"))?;
    let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
        .map_err(|e| format!("CreateDirect3D11DeviceFromDXGIDevice 失败: {e}"))?;
    let winrt_device = inspectable
        .cast::<IDirect3DDevice>()
        .map_err(|e| format!("转换为 IDirect3DDevice 失败: {e}"))?;
    Ok((device, context, winrt_device))
}

/// 按显示区域中心点定位 HMONITOR。
pub unsafe fn monitor_from_bounds(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> windows::Win32::Graphics::Gdi::HMONITOR {
    let cx = x + (width / 2) as i32;
    let cy = y + (height / 2) as i32;
    MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST)
}

/// 对指定显示器做一次采集探测：抓 N 秒，统计帧率与内容尺寸，并自检首帧亮度。
///
/// `dump_nv12`：若指定路径，则把首帧转成 NV12 落盘（供编码器输入）。
pub fn probe_capture(
    bounds_x: i32,
    bounds_y: i32,
    width: u32,
    height: u32,
    duration_ms: u64,
    dump_nv12: Option<&Path>,
) -> Result<CaptureStats, String> {
    unsafe {
        let hmonitor = monitor_from_bounds(bounds_x, bounds_y, width, height);
        if hmonitor.0.is_null() {
            return Err("MonitorFromPoint 未返回有效 HMONITOR".into());
        }

        let (d3d_device, d3d_context, winrt_device) = create_device()?;

        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(|e| format!("获取 IGraphicsCaptureItemInterop 失败: {e}"))?;
        let item: GraphicsCaptureItem = interop
            .CreateForMonitor(hmonitor)
            .map_err(|e| format!("CreateForMonitor 失败: {e}"))?;

        let size = SizeInt32 { Width: width as i32, Height: height as i32 };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| format!("创建 Direct3D11CaptureFramePool 失败: {e}"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| format!("创建 GraphicsCaptureSession 失败: {e}"))?;
        session
            .StartCapture()
            .map_err(|e| format!("StartCapture 失败: {e}"))?;

        let start = Instant::now();
        let mut stats = CaptureStats::default();
        let mut last_ts: Option<Instant> = None;
        let mut last_content = (width, height);

        while (start.elapsed().as_millis() as u64) < duration_ms {
            match pool.TryGetNextFrame() {
                Ok(frame) => {
                    let now = Instant::now();
                    if let Some(prev) = last_ts {
                        let interval = now.duration_since(prev).as_secs_f64() * 1000.0;
                        stats.avg_interval_ms = if stats.frames == 0 {
                            interval
                        } else {
                            stats.avg_interval_ms
                                + (interval - stats.avg_interval_ms) / (stats.frames as f64)
                        };
                        stats.max_interval_ms = stats.max_interval_ms.max(interval);
                        stats.min_interval_ms = if stats.min_interval_ms == 0.0 {
                            interval
                        } else {
                            stats.min_interval_ms.min(interval)
                        };
                    } else {
                        stats.first_frame_delay_ms = now.duration_since(start).as_secs_f64() * 1000.0;
                    }
                    last_ts = Some(now);
                    stats.frames += 1;
                    if let Ok(content) = frame.ContentSize() {
                        last_content = (content.Width as u32, content.Height as u32);
                    }

                    // 首帧做一次内容自检（亮度 + 可选导出 NV12）
                    if stats.content.is_none() {
                        if let Ok(surface) = frame.Surface() {
                            let want_nv12 = dump_nv12.is_some();
                            if let Ok((content, nv12, _bgra)) = analyze_surface(
                                &d3d_device,
                                &d3d_context,
                                &surface,
                                4,
                                want_nv12,
                                false,
                            ) {
                                stats.content = Some(content);
                                if let (Some(path), Some(buf)) = (dump_nv12, nv12) {
                                    if let Some(parent) = path.parent() {
                                        let _ = std::fs::create_dir_all(parent);
                                    }
                                    std::fs::write(path, &buf)
                                        .map_err(|e| format!("写 NV12 失败: {e}"))?;
                                    stats.nv12_path = Some(path.display().to_string());
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    // 没有新帧时短暂休眠，避免空转烧 CPU
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }

        stats.content_width = last_content.0;
        stats.content_height = last_content.1;
        Ok(stats)
    }
}


/// 实时采集并把每帧的 **BGRA 原始像素**交给回调（供编码器消费）。
///
/// 与 `probe_capture` 的区别：本函数每帧都做回读，并附带亮度抽样统计，
/// 因此可以真正做到"采集 → 编码"连续流式处理。
/// **不在 Rust 侧做 BGRA→NV12 转换**——那曾是 175ms 的瓶颈，现由 ffmpeg SIMD 完成。
pub fn capture_stream<F>(
    bounds_x: i32,
    bounds_y: i32,
    width: u32,
    height: u32,
    duration_ms: u64,
    target_fps: u32,
    include_cursor: bool,
    mut on_frame: F,
) -> Result<CaptureStats, String>
where
    F: FnMut(&[u8]) -> Result<(), String>,
{
    unsafe {
        let hmonitor = monitor_from_bounds(bounds_x, bounds_y, width, height);
        if hmonitor.0.is_null() {
            return Err("MonitorFromPoint 未返回有效 HMONITOR".into());
        }
        let (d3d_device, d3d_context, winrt_device) = create_device()?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(|e| format!("获取 IGraphicsCaptureItemInterop 失败: {e}"))?;
        let item: GraphicsCaptureItem = interop
            .CreateForMonitor(hmonitor)
            .map_err(|e| format!("CreateForMonitor 失败: {e}"))?;
        let size = SizeInt32 { Width: width as i32, Height: height as i32 };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| format!("创建 Direct3D11CaptureFramePool 失败: {e}"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| format!("创建 GraphicsCaptureSession 失败: {e}"))?;
        // 是否把鼠标光标合成进画面（WGC 默认 true）。
        // 光标是独立图层，在帧捕获瞬间合成；虚拟显示器上可能出现位置抖动，
        // 用 `--no-cursor` 关闭可做对照排查。老系统不支持该方法时忽略。
        if let Err(e) = session.SetIsCursorCaptureEnabled(include_cursor) {
            eprintln!("[capture] 设置光标捕获失败（忽略）: {e}");
        }
        session
            .StartCapture()
            .map_err(|e| format!("StartCapture 失败: {e}"))?;

        let start = Instant::now();
        let mut stats = CaptureStats::default();
        let mut last_ts: Option<Instant> = None;
        // 帧率限速：采集频率（显示器刷新率）常高于编码目标帧率，
        // 不限速会让编码器按错误的时间基播放，画面节奏失真。
        let min_interval = if target_fps > 0 {
            // 阈值必须留容差。60Hz 源 + 50ms（20fps）阈值太贴边：第 3 帧恰好落在
            // 50.0ms，抖动一点点就判不过，只能等第 4 帧（66.7ms）→ 实测掉到 16.7fps。
            // 打九折成 45ms，第 3 帧稳定通过，实际帧率回到 20fps。
            std::time::Duration::from_micros(1_000_000 / target_fps as u64 * 9 / 10)
        } else {
            std::time::Duration::from_micros(0)
        };
        let mut last_push: Option<Instant> = None;
        // 静止保活用的上一帧缓存（BGRA）。只有成功送出一帧后才有值。
        let mut last_bgra: Option<Vec<u8>> = None;
        // 保活间隔：未限帧（target_fps=0）时 min_interval 为 0，
        // 若直接拿它当保活阈值会每 2ms 补一帧，故退回 ~30fps。
        let keepalive_interval = if target_fps > 0 {
            min_interval
        } else {
            std::time::Duration::from_millis(33)
        };

        while (start.elapsed().as_millis() as u64) < duration_ms {
            match pool.TryGetNextFrame() {
                Ok(frame) => {
                    let now = Instant::now();
                    if let Some(prev) = last_ts {
                        let interval = now.duration_since(prev).as_secs_f64() * 1000.0;
                        stats.avg_interval_ms = if stats.frames == 0 {
                            interval
                        } else {
                            stats.avg_interval_ms
                                + (interval - stats.avg_interval_ms) / (stats.frames as f64)
                        };
                        stats.max_interval_ms = stats.max_interval_ms.max(interval);
                        stats.min_interval_ms = if stats.min_interval_ms == 0.0 {
                            interval
                        } else {
                            stats.min_interval_ms.min(interval)
                        };
                    } else {
                        stats.first_frame_delay_ms = now.duration_since(start).as_secs_f64() * 1000.0;
                    }
                    last_ts = Some(now);
                    stats.frames += 1;
                    if let Ok(content) = frame.ContentSize() {
                        stats.content_width = content.Width as u32;
                        stats.content_height = content.Height as u32;
                    }
                    // 限速：到点才送帧
                    if let Some(lp) = last_push {
                        if now.duration_since(lp) < min_interval {
                            continue;
                        }
                    }
                    if let Ok(surface) = frame.Surface() {
                        // BGRA 直出：色彩转换交给 ffmpeg 的 SIMD 路径，
                        // 这里只做回读 + 亮度抽样统计。
                        let (content, bgra) =
                            read_surface_bgra(&d3d_device, &d3d_context, &surface, 16)?;
                        if content.sampled > 0 {
                            stats.luma_avg = content.luma_avg;
                            stats.non_black_ratio = content.non_black_ratio;
                        }
                        last_push = Some(now);
                        on_frame(&bgra)?;
                        last_bgra = Some(bgra);   // 缓存供静止保活重复送帧
                    }
                }
                Err(_) => {
                    // 静止保活（2026-11-21 实测修复）：
                    // WGC 只在画面内容变化时给帧，完全静止时永不回调。
                    // 没有保活的话编码→RTP→接收端上屏全部停摆，接收端 fps 随画面动静
                    // 在 0~30 之间大幅波动，还会反复触发低帧自愈造成周期性黑屏。
                    // 这里按目标帧率重复上一帧：编码器对相同帧只输出极小的 skip P 帧，
                    // 码率几乎不增，但接收端能维持恒定帧率与即时的交互回显。
                    if let (Some(buf), Some(lp)) = (&last_bgra, last_push) {
                        if lp.elapsed() >= keepalive_interval {
                            on_frame(buf)?;
                            stats.keepalive_frames += 1;
                            last_push = Some(Instant::now());
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
        Ok(stats)
    }
}
