//! GPU 帧回读与内容自检。
//!
//! 目的有两个：
//! 1. **判断画面是否真的有效**——虚拟屏最常见的坑是"帧数正常但画面全黑"，
//!    只看帧率会被骗过去，必须抽样算亮度。
//! 2. **产出编码器可直接消费的 NV12 帧**——Media Foundation 的 H.264 硬编
//!    走 NV12/I420 输入，先把转换结果准备好，Stage 3 就能直接接上。

use windows::core::Interface;
use windows::Graphics::DirectX::Direct3D11::IDirect3DSurface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;

/// 单帧的内容统计。
#[derive(Debug, Clone, Default)]
pub struct FrameContent {
    /// 平均亮度（0~255，Rec.601 加权）
    pub luma_avg: f64,
    /// 最大亮度
    pub luma_max: u8,
    /// 非黑像素占比（亮度 > 8 的采样点比例，0~1）
    pub non_black_ratio: f64,
    /// 抽样点数
    pub sampled: u64,
    /// 是否判定为"黑画面"
    pub is_black: bool,
}

/// 把 GPU 表面回读为 CPU 像素，并统计亮度；同时可选产出 NV12。
///
/// `sample_step`：每隔多少像素采一次点（用于亮度统计，越大越快越粗糙）。
/// `nv12`：为 `Some` 时输出整帧 NV12（I420 半色度），供编码器使用。
pub unsafe fn analyze_surface(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    surface: &IDirect3DSurface,
    sample_step: u32,
    nv12: bool,
) -> Result<(FrameContent, Option<Vec<u8>>), String> {
    // WinRT 表面 -> D3D11 纹理
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .map_err(|e| format!("转换为 IDirect3DDxgiInterfaceAccess 失败: {e}"))?;
    let texture: ID3D11Texture2D = access
        .GetInterface()
        .map_err(|e| format!("获取 ID3D11Texture2D 失败: {e}"))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut desc);
    let width = desc.Width as usize;
    let height = desc.Height as usize;
    if width == 0 || height == 0 {
        return Err("纹理尺寸为 0".into());
    }

    // 建 staging 纹理并拷贝
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: desc.SampleDesc,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    device
        .CreateTexture2D(&staging_desc, None, Some(&mut staging))
        .map_err(|e| format!("创建 staging 纹理失败: {e}"))?;
    let staging = staging.ok_or_else(|| "staging 纹理为空".to_string())?;

    let src_resource: ID3D11Resource = texture
        .cast()
        .map_err(|e| format!("转换为 ID3D11Resource 失败: {e}"))?;
    let dst_resource: ID3D11Resource = staging
        .cast()
        .map_err(|e| format!("staging 转换为 ID3D11Resource 失败: {e}"))?;
    context.CopyResource(&dst_resource, &src_resource);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    context
        .Map(&dst_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .map_err(|e| format!("Map staging 失败: {e}"))?;

    let result = analyze_pixels(
        mapped.pData as *const u8,
        mapped.RowPitch as usize,
        width,
        height,
        sample_step,
        nv12,
    );

    context.Unmap(&dst_resource, 0);
    result
}

/// 对回读后的 BGRA 像素做亮度统计（可选同时转 NV12）。
///
/// # Safety
/// `data` 必须指向有效的 BGRA 像素缓冲，`row_pitch` 为每行字节数。
unsafe fn analyze_pixels(
    data: *const u8,
    row_pitch: usize,
    width: usize,
    height: usize,
    sample_step: u32,
    want_nv12: bool,
) -> Result<(FrameContent, Option<Vec<u8>>), String> {
    let step = sample_step.max(1) as usize;
    let mut sum: u64 = 0;
    let mut max_luma: u8 = 0;
    let mut sampled: u64 = 0;
    let mut non_black: u64 = 0;

    // NV12 布局：Y 平面 (w*h) + U 平面 (w/2*h/2) + V 平面 (w/2*h/2)
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    let mut nv12: Option<Vec<u8>> = if want_nv12 {
        Some(vec![0u8; width * height + 2 * chroma_w * chroma_h])
    } else {
        None
    };

    for y in 0..height {
        let row = data.add(y * row_pitch);
        for x in 0..width {
            let px = row.add(x * 4);
            let b = *px as f64;
            let g = *px.add(1) as f64;
            let r = *px.add(2) as f64;

            if x % step == 0 && y % step == 0 {
                let luma = (0.299 * r + 0.587 * g + 0.114 * b).round().clamp(0.0, 255.0) as u8;
                sum += luma as u64;
                sampled += 1;
                if luma > max_luma {
                    max_luma = luma;
                }
                if luma > 8 {
                    non_black += 1;
                }
            }

            if let Some(buf) = nv12.as_mut() {
                // Y：BT.601 limited range
                let yy = (16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0)
                    .clamp(0.0, 255.0) as u8;
                buf[y * width + x] = yy;
                // U/V：2x2 下采样（取左上角像素作为色度样本）
                if x % 2 == 0 && y % 2 == 0 {
                    let u = (128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0)
                        .clamp(0.0, 255.0) as u8;
                    let v = (128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0)
                        .clamp(0.0, 255.0) as u8;
                    let cidx = (y / 2) * chroma_w + (x / 2);
                    buf[width * height + cidx] = u;
                    buf[width * height + chroma_w * chroma_h + cidx] = v;
                }
            }
        }
    }

    let luma_avg = if sampled > 0 { sum as f64 / sampled as f64 } else { 0.0 };
    let non_black_ratio = if sampled > 0 { non_black as f64 / sampled as f64 } else { 0.0 };
    let content = FrameContent {
        luma_avg,
        luma_max: max_luma,
        non_black_ratio,
        sampled,
        // 判定为黑画面：平均亮度极低且几乎没有非黑像素
        is_black: luma_avg < 1.0 && non_black_ratio < 0.001,
    };
    Ok((content, nv12))
}
