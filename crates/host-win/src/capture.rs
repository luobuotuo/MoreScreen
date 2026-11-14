
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

use crate::readback::{analyze_surface, FrameContent};
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
    pub first_frame_delay_ms: f64,
    pub avg_interval_ms: f64,
    pub max_interval_ms: f64,
    pub content_width: u32,
    pub content_height: u32,
    pub min_interval_ms: f64,
    /// 首帧内容自检（亮度）
    pub content: Option<FrameContent>,
    /// 导出的 NV12 帧路径（若请求）
    pub nv12_path: Option<String>,
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
                            if let Ok((content, nv12)) =
                                analyze_surface(&d3d_device, &d3d_context, &surface, 4, want_nv12)
                            {
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


/// 实时采集并把每帧转成 NV12 交给回调（供编码器消费）。
///
/// 与 `probe_capture` 的区别：本函数每帧都做回读与色彩转换，
/// 因此可以真正做到"采集 → 编码"连续流式处理。
pub fn capture_stream<F>(
    bounds_x: i32,
    bounds_y: i32,
    width: u32,
    height: u32,
    duration_ms: u64,
    target_fps: u32,
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
        session
            .StartCapture()
            .map_err(|e| format!("StartCapture 失败: {e}"))?;

        let start = Instant::now();
        let mut stats = CaptureStats::default();
        let mut last_ts: Option<Instant> = None;
        // 帧率限速：采集频率（显示器刷新率）常高于编码目标帧率，
        // 不限速会让编码器按错误的时间基播放，画面节奏失真。
        let min_interval = if target_fps > 0 {
            std::time::Duration::from_micros(1_000_000 / target_fps as u64)
        } else {
            std::time::Duration::from_micros(0)
        };
        let mut last_push: Option<Instant> = None;

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
                        let (_, nv12) = analyze_surface(&d3d_device, &d3d_context, &surface, 16, true)?;
                        if let Some(buf) = nv12 {
                            last_push = Some(now);
                            on_frame(&buf)?;
                        }
                    }
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
        Ok(stats)
    }
}
