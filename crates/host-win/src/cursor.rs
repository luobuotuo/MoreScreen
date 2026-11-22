//! 自绘光标合成（2026-11-22 新增）。
//!
//! **为什么不用 WGC 自带的 `IsCursorCaptureEnabled`**：
//! 1. 系统在**帧捕获瞬间**合成光标，采样时刻与帧内容时刻不严格对齐，
//!    运动时（拖动窗口）光标相对窗口漂移 → 接收端观感"晃动"；
//! 2. 与静止保活冲突：保活帧复用上一帧，光标冻结在旧位置，画面静止时
//!    光标不能动；
//! 3. 虚拟显示器上系统合成的光标位置有抖动（历史上用 `--no-cursor` 对照实证过）。
//!
//! 本模块改为**发送前一刻**查询 `GetCursorInfo`，把光标按当前真实位置画到
//! BGRA 帧上：光标与画面的时间差压缩到采集→发送的几毫秒；静止保活帧
//! 每次都按当前位置重画 → 画面静止时鼠标照样 30fps 流畅移动。
//!
//! 坐标系：`GetCursorInfo` 与采集 bounds 同为全局虚拟屏坐标（物理像素），
//! 宿主进程已设 per-monitor DPI aware v2（`input::init_dpi_awareness`），
//! 物理像素对物理像素，无需换算。

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HICON, ICONINFO,
};

/// 像素 alpha 约定。
const A_OPAQUE: u8 = 255; // 直接画
const A_TRANSPARENT: u8 = 0; // 不画
const A_INVERT: u8 = 254; // 反转目标色（单色光标 AND=1 XOR=1 的经典反色区）

/// 解码归一化后的光标位图（BGRA + alpha 约定编码）。
struct DecodedCursor {
    w: i32,
    h: i32,
    hotspot: (i32, i32),
    /// BGRA × (w×h)，alpha 用上面的约定编码。
    pixels: Vec<u8>,
}

/// 查询当前全局光标。返回 (句柄, 虚拟屏坐标)。不可见/失败返回 None。
fn current_cursor() -> Option<(isize, POINT)> {
    unsafe {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut ci).is_err() {
            return None;
        }
        if (ci.flags.0 & CURSOR_SHOWING.0) == 0 {
            return None;
        }
        let handle = ci.hCursor.0 as isize;
        if handle == 0 {
            return None;
        }
        Some((handle, ci.ptScreenPos))
    }
}

/// 解码一个光标句柄。失败返回 None（该帧不画光标，不致命）。
///
/// ICONINFO 的 hbmMask/hbmColor 是**调用方负责删除**的临时位图，必须释放。
unsafe fn decode(handle: isize) -> Option<DecodedCursor> {
    let mut info = ICONINFO::default();
    // HCURSOR 与 HICON 是同一底层句柄，按 GetIconInfo 要求的 HICON 传。
    if GetIconInfo(HICON(handle as *mut _), &mut info).is_err() {
        return None;
    }
    let result = decode_inner(&info);
    let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
    if !info.hbmColor.0.is_null() {
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
    }
    result
}

unsafe fn decode_inner(info: &ICONINFO) -> Option<DecodedCursor> {
    // 尺寸来自 mask 位图：彩色光标 mask 高度 = 光标高；单色光标 = 2×光标高
    // （单色 mask 是双平面位图：上半 AND、下半 XOR）。
    let mut bm = BITMAP::default();
    if GetObjectW(
        HGDIOBJ(info.hbmMask.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut BITMAP as _),
    ) == 0
    {
        return None;
    }
    let mask_total_h = bm.bmHeight;
    let w = bm.bmWidth;
    if w <= 0 || mask_total_h <= 0 {
        return None;
    }
    let color = !info.hbmColor.0.is_null();
    let h = if color { mask_total_h } else { mask_total_h / 2 };
    if h <= 0 {
        return None;
    }
    let (w, h) = (w as usize, h as usize);
    let mask_total_h = mask_total_h as usize;

    let hdc = GetDC(None);
    if hdc.is_invalid() {
        return None;
    }

    // ---- 1. AND/XOR mask（1bpp，top-down，行按 4 字节对齐）----
    let mask_stride = (w + 31) / 32 * 4;
    let mut mask = vec![0u8; mask_stride * mask_total_h];
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(mask_total_h as i32), // 负数 = top-down
            biPlanes: 1,
            biBitCount: 1,
            biCompression: DIB_RGB_COLORS.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let ok = GetDIBits(
        hdc,
        info.hbmMask,
        0,
        mask_total_h as u32,
        Some(mask.as_mut_ptr() as _),
        &mut bmi,
        DIB_RGB_COLORS,
    ) != 0;
    if !ok {
        ReleaseDC(None, hdc);
        return None;
    }

    // ---- 2. 彩色平面（32bpp BGRA，top-down）----
    let mut color_px = vec![0u8; w * h * 4];
    if color {
        let mut bmi2 = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: DIB_RGB_COLORS.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let ok = GetDIBits(
            hdc,
            info.hbmColor,
            0,
            h as u32,
            Some(color_px.as_mut_ptr() as _),
            &mut bmi2,
            DIB_RGB_COLORS,
        ) != 0;
        if !ok {
            ReleaseDC(None, hdc);
            return None;
        }
    }
    ReleaseDC(None, hdc);

    // ---- 3. 归一化为 BGRA + alpha 约定 ----
    // Win32 光标混合规则：
    //   AND=0              → 画 color（彩色） / 黑或白（单色 XOR=0/1）
    //   AND=1, XOR=0/color → 透明
    //   AND=1, XOR≠0       → 反转目标色
    let mask_bit = |x: usize, y: usize| -> bool {
        let byte = mask[y * mask_stride + x / 8];
        (byte >> (7 - (x % 8))) & 1 == 1
    };
    let mut pixels = vec![0u8; w * h * 4];
    let mut any_opaque = false;
    for y in 0..h {
        for x in 0..w {
            let and = mask_bit(x, y);
            let o = (y * w + x) * 4;
            if color {
                if and {
                    pixels[o + 3] = A_TRANSPARENT;
                    continue;
                }
                pixels[o] = color_px[o];
                pixels[o + 1] = color_px[o + 1];
                pixels[o + 2] = color_px[o + 2];
                pixels[o + 3] = color_px[o + 3];
            } else {
                // 单色：XOR 平面 = mask 的第二个平面（彩色高度之外那半）。
                let xor = mask_bit(x, y + h);
                match (and, xor) {
                    (false, false) => {
                        pixels[o] = 0;
                        pixels[o + 1] = 0;
                        pixels[o + 2] = 0;
                        pixels[o + 3] = A_OPAQUE;
                    }
                    (false, true) => {
                        pixels[o] = 255;
                        pixels[o + 1] = 255;
                        pixels[o + 2] = 255;
                        pixels[o + 3] = A_OPAQUE;
                    }
                    (true, false) => {
                        pixels[o + 3] = A_TRANSPARENT;
                    }
                    (true, true) => {
                        pixels[o + 3] = A_INVERT;
                    }
                }
            }
            if pixels[o + 3] == A_OPAQUE {
                any_opaque = true;
            }
        }
    }
    // 彩色光标的 alpha 全 0 但 RGB 有效（部分 GDI 生成的光标）→ 按不透明处理。
    if color && !any_opaque {
        let any_rgb = pixels.chunks_exact(4).any(|p| p[0] | p[1] | p[2] != 0);
        if any_rgb {
            for p in pixels.chunks_exact_mut(4) {
                p[3] = A_OPAQUE;
            }
        }
    }

    Some(DecodedCursor {
        w: w as i32,
        h: h as i32,
        hotspot: (info.xHotspot as i32, info.yHotspot as i32),
        pixels,
    })
}

/// 自绘光标渲染器。按光标句柄缓存解码结果，句柄变化（箭头↔手型↔文本）才重建。
pub struct CursorPainter {
    handle: isize,
    decoded: Option<DecodedCursor>,
}

impl Default for CursorPainter {
    fn default() -> Self {
        Self {
            handle: 0,
            decoded: None,
        }
    }
}

impl CursorPainter {
    /// 把当前光标画到 `clean`（不含光标的 BGRA 帧）上。
    ///
    /// 返回 `Some(painted)`：光标可见且落在采集区域内，`painted` 是写入
    /// `scratch` 的合成帧；返回 `None`：光标不可见/不在区域，直接用 `clean`。
    ///
    /// `origin` = 采集区域在虚拟屏坐标系的原点（物理像素）。
    pub fn render<'a>(
        &mut self,
        scratch: &'a mut Vec<u8>,
        clean: &[u8],
        w: usize,
        h: usize,
        origin_x: i32,
        origin_y: i32,
    ) -> Option<&'a [u8]> {
        let (handle, pos) = current_cursor()?;
        if handle != self.handle {
            unsafe { self.decoded = decode(handle) };
            self.handle = handle;
            // 解码失败也要记住句柄，避免每帧重试 GDI 调用。
        }
        let d = self.decoded.as_ref()?;

        // 光标位图左上角（屏幕坐标 → 区域局部 → 减热点）。
        let dx = pos.x - origin_x - d.hotspot.0;
        let dy = pos.y - origin_y - d.hotspot.1;
        let dw = d.w;
        let dh = d.h;
        // 完全在区域外 → 无需合成（也避免无意义的整帧拷贝）。
        if dx + dw <= 0 || dy + dh <= 0 || dx >= w as i32 || dy >= h as i32 {
            return None;
        }

        if scratch.len() != clean.len() {
            scratch.clear();
            scratch.resize(clean.len(), 0);
        }
        scratch.copy_from_slice(clean);

        for cy in 0..dh {
            for cx in 0..dw {
                let o = (cy as usize * d.w as usize + cx as usize) * 4;
                let a = d.pixels[o + 3];
                if a == A_TRANSPARENT {
                    continue;
                }
                let tx = dx + cx;
                let ty = dy + cy;
                if tx < 0 || ty < 0 || tx >= w as i32 || ty >= h as i32 {
                    continue; // 部分越界裁剪
                }
                let di = (ty as usize * w + tx as usize) * 4;
                match a {
                    A_OPAQUE => {
                        scratch[di] = d.pixels[o];
                        scratch[di + 1] = d.pixels[o + 1];
                        scratch[di + 2] = d.pixels[o + 2];
                    }
                    A_INVERT => {
                        scratch[di] = !scratch[di];
                        scratch[di + 1] = !scratch[di + 1];
                        scratch[di + 2] = !scratch[di + 2];
                    }
                    a => {
                        // 带真实 alpha 的 source-over（彩色光标半透明边缘）。
                        let inv = 255 - a;
                        for c in 0..3 {
                            let src = d.pixels[o + c] as u32 * a as u32;
                            let dst = scratch[di + c] as u32 * inv as u32;
                            scratch[di + c] = ((src + dst) / 255) as u8;
                        }
                    }
                }
            }
        }
        Some(&scratch[..])
    }
}
