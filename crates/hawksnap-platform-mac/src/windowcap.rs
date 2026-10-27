//! 长截图采集源：**窗口区域抓取**（CGWindowListCreateImage）+ **屏幕抓取兜底**。
//!
//! ## 选型（plan Task N5 裁定）
//!
//! plan 原文：「SCScrollWindowImage 不可用，用 **CGWindowListCreateImage 定时 500ms**
//! 简单可靠」。本模块即该裁定的落地：
//!
//! - **主路径 = 窗口抓取**：先用 `CGWindowListCopyWindowInfo` 找出**选区中心所在
//!   的最上层窗口**（layer==0，排除自身进程），再对该窗口调
//!   `CGWindowListCreateImage(rect, kCGWindowListOptionIncludingWindow, wid, …)`，
//!   只取选区那一块。好处：拿到的是**窗口自身内容**（即使被别的窗口压住一部分
//!   也照样拿到窗口的渲染结果），且不必抓整屏。
//! - **兜底 = 屏幕抓取**：窗口路径任一环节不成立（选区跨出窗口、CG 返回空、
//!   macOS 新版本移除/限制该 API、尺寸异常）时，改用既有 xcap 显示器抓取 +
//!   物理像素裁剪（截图主链路同一套代码，已久经使用）。**长截图不因单一 API
//!   不可用而整体不可用**。
//!
//! ## 坐标系
//!
//! 全局 **Quartz 逻辑坐标（points，y 轴向下，原点 = 主显示器左上角）**——与
//! `CGWindowListCopyWindowInfo` 的 `kCGWindowBounds`、xcap 的 `Monitor::x/y`
//! 同一套坐标。物理像素 = 逻辑 × 显示器缩放（Retina 2x），由抓取结果自身的
//! 像素尺寸反推（不硬编码 2）。
//!
//! ## 权限
//!
//! 与截图主链路同门槛：**屏幕录制权限**。未授权时 macOS 14+ 静默返回壁纸/空图
//! ——调用方（app 层）先用 `permissions::screen_recording_prechecked()` 预检。

// M4 P0：非 macOS 平台无 CG 抓取路径——c_void 仅供 macOS 实现内的裸 extern
// 使用，故随实现同门控（否则非 macOS 构建报 unused import）。
#[cfg(target_os = "macos")]
use std::os::raw::c_void;

/// 全局逻辑矩形（Quartz points，y 轴向下）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobalRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl GlobalRect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
    /// 选区中心点（找「中心所在窗口」的探测点）。
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// 抓取结果：物理像素 RGBA。
#[derive(Debug, Clone)]
pub struct RegionImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 抓取全局逻辑矩形区域（主路径 + 兜底）。
///
/// 返回物理像素 RGBA；失败返回中文错误串（面向用户可见的错误横幅）。
#[cfg(target_os = "macos")]
pub fn capture_global_rect(rect: GlobalRect) -> Result<RegionImage, String> {
    match capture_via_window(rect) {
        Ok(img) => Ok(img),
        Err(e) => {
            // 兜底：窗口路径不成立 → 屏幕抓取（xcap）+ 裁剪。
            eprintln!("[hawksnap] 窗口抓取不可用（{e}），改用屏幕抓取");
            capture_via_screen(rect)
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn capture_global_rect(_rect: GlobalRect) -> Result<RegionImage, String> {
    Err("仅 macOS 支持长截图抓取".into())
}

// ---------------------------------------------------------------------------
// 主路径：CGWindowListCreateImage（窗口自身内容）
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn capture_via_window(rect: GlobalRect) -> Result<RegionImage, String> {
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};
    use core_graphics::window::{
        create_image, kCGWindowImageBestResolution, kCGWindowImageBoundsIgnoreFraming,
        kCGWindowListOptionIncludingWindow,
    };

    let (cx, cy) = rect.center();
    let Some((wid, bounds)) = topmost_window_at(cx, cy) else {
        return Err("选区中心没有可抓取的窗口".into());
    };
    // 窗口必须完整覆盖选区：否则窗口抓取会缺边（内容不完整）→ 交兜底处理。
    let (bx, by, bw, bh) = bounds;
    if bx > rect.x + 0.5
        || by > rect.y + 0.5
        || bx + bw < rect.x + rect.w - 0.5
        || by + bh < rect.y + rect.h - 0.5
    {
        return Err("选区超出窗口边界".into());
    }
    let image = create_image(
        CGRect::new(&CGPoint::new(rect.x, rect.y), &CGSize::new(rect.w, rect.h)),
        kCGWindowListOptionIncludingWindow,
        wid,
        kCGWindowImageBoundsIgnoreFraming | kCGWindowImageBestResolution,
    )
    .ok_or("CGWindowListCreateImage 返回空（未授权或窗口不可见）")?;
    let (rgba, w, h) = cgimage_to_rgba(&image)?;
    // 尺寸自检：物理宽高必须是选区逻辑尺寸的同一比例（1x 或 2x）。比例不一致
    // 说明返回的不是我们要的那块 → 交兜底处理，绝不产出错位内容。
    let sx = w as f64 / rect.w;
    let sy = h as f64 / rect.h;
    if !(0.5..=4.0).contains(&sx) || (sx - sy).abs() > 0.2 {
        return Err(format!("窗口抓取尺寸异常（{w}×{h} 对应选区 {rect:?}）"));
    }
    Ok(RegionImage { width: w, height: h, rgba })
}

/// 选区中心所在的最上层窗口（layer==0，排除自身进程）。
///
/// `CGWindowListCopyWindowInfo` 的返回顺序是**前到后**（z 序），故第一个命中
/// 点位的即最上层窗口。
#[cfg(target_os = "macos")]
fn topmost_window_at(x: f64, y: f64) -> Option<(u32, (f64, f64, f64, f64))> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
        kCGWindowListOptionOnScreenOnly,
    };

    fn number_of(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<f64> {
        let value = dict.find(CFString::new(key))?;
        value.downcast::<CFNumber>().and_then(|n| n.to_f64())
    }

    let array = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    let own_pid = f64::from(std::process::id());
    for i in 0..array.len() {
        let Some(item) = array.get(i) else { continue };
        let dict =
            unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(*item as *const _) };
        // layer==0 = 普通应用窗口（菜单栏/Dock/状态项排除）。
        if number_of(&dict, "kCGWindowLayer") != Some(0.0) {
            continue;
        }
        // 排除自身进程：悬浮条/覆盖层是本应用的窗口，绝不能当成被拍对象。
        if number_of(&dict, "kCGWindowOwnerPID") == Some(own_pid) {
            continue;
        }
        let Some(num) = number_of(&dict, "kCGWindowNumber") else { continue };
        let Some(bounds_ref) = dict.find(CFString::new("kCGWindowBounds")) else { continue };
        let bounds = unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(
                bounds_ref.as_CFTypeRef() as *const _,
            )
        };
        let (Some(bx), Some(by), Some(bw), Some(bh)) = (
            number_of(&bounds, "X"),
            number_of(&bounds, "Y"),
            number_of(&bounds, "Width"),
            number_of(&bounds, "Height"),
        ) else {
            continue;
        };
        if bw <= 0.0 || bh <= 0.0 {
            continue;
        }
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            return Some((num as u32, (bx, by, bw, bh)));
        }
    }
    None
}

/// CGImage → 物理像素 RGBA。
///
/// 手法：`CGBitmapContext`（RGBA8 / premultipliedLast / 32Big）+ 垂直翻转 CTM
/// 后 `draw_image`。**上下文直接以调用方缓冲为后备存储**（`CGBitmapContextCreate`
/// 的 data 参数）——无需二次拷贝。
#[cfg(target_os = "macos")]
fn cgimage_to_rgba(image: &core_graphics::image::CGImage) -> Result<(Vec<u8>, u32, u32), String> {
    use core_graphics::base::kCGImageAlphaPremultipliedLast;
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};

    // kCGBitmapByteOrder32Big = 4 << 12（注意不是 1<<12——那是 16 位小端，
    // 与 32 位上下文不兼容 → CGBitmapContextCreate 返回 NULL）。
    // 32 位大端 + alpha=PremultipliedLast → 内存字节序 R,G,B,A。
    const BYTE_ORDER_32_BIG: u32 = 4 << 12;
    let w = image.width();
    let h = image.height();
    if w == 0 || h == 0 {
        return Err("CGImage 尺寸为空".into());
    }
    let mut buf = vec![0u8; w * h * 4];
    let space = CGColorSpace::create_device_rgb();
    let ctx = CGContext::create_bitmap_context(
        Some(buf.as_mut_ptr() as *mut c_void),
        w,
        h,
        8,
        w * 4,
        &space,
        kCGImageAlphaPremultipliedLast | BYTE_ORDER_32_BIG,
    );
    // 位图上下文的原点在左下，CGImage 的原点在左上 → 先翻转 CTM 再绘制，
    // 否则整幅图上下颠倒。
    // 无翻转 CTM（见单测结论：CGContextDrawImage 在位图上下文里已按
    // 「图像首行 = 缓冲首行」落位，额外翻转会上下颠倒）。
    ctx.draw_image(
        CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(w as f64, h as f64),
        ),
        image,
    );
    Ok((buf, w as u32, h as u32))
}

// ---------------------------------------------------------------------------
// 兜底路径：屏幕抓取（xcap 显示器抓取 + 物理像素裁剪）
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn capture_via_screen(rect: GlobalRect) -> Result<RegionImage, String> {
    use hawksnap_core::geometry::Rect;

    let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
    for m in monitors {
        let (mx, my, mw, mh) = (
            m.x().map_err(|e| e.to_string())? as f64,
            m.y().map_err(|e| e.to_string())? as f64,
            m.width().map_err(|e| e.to_string())? as f64,
            m.height().map_err(|e| e.to_string())? as f64,
        );
        // 选区必须完整落在某块显示器内（跨屏选区不支持——物理像素拼接无意义）。
        if rect.x < mx - 0.5
            || rect.y < my - 0.5
            || rect.x + rect.w > mx + mw + 0.5
            || rect.y + rect.h > my + mh + 0.5
        {
            continue;
        }
        let img = m.capture_image().map_err(|e| e.to_string())?;
        let scale = m.scale_factor().map_err(|e| e.to_string())? as f64;
        let px = ((rect.x - mx) * scale).round() as i32;
        let py = ((rect.y - my) * scale).round() as i32;
        let pw = (rect.w * scale).round().max(1.0) as u32;
        let ph = (rect.h * scale).round().max(1.0) as u32;
        let cropped = hawksnap_core::crop::crop_rgba(
            img.as_raw(),
            img.width(),
            img.height(),
            &Rect { x: px, y: py, w: pw, h: ph },
        )
        .map_err(|e| e.to_string())?;
        return Ok(RegionImage { width: pw, height: ph, rgba: cropped });
    }
    Err("选区不在任何显示器范围内".into())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_rect_center_and_ctor() {
        let r = GlobalRect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.center(), (60.0, 45.0));
        assert_eq!(r, GlobalRect { x: 10.0, y: 20.0, w: 100.0, h: 50.0 });
    }

    /// CGImage → RGBA 的**通道序与上下方向**自检（无需任何权限）：
    /// 造一张 2×2 图（上排红、下排蓝），过位图上下文后逐字节断言。
    /// 这是本模块最容易静默出错的地方（BGRA/RGBA、上下颠倒），故固化为单测。
    #[cfg(target_os = "macos")]
    #[test]
    fn cgimage_to_rgba_keeps_channel_order_and_orientation() {
        use core_graphics::base::kCGImageAlphaPremultipliedLast;
        use core_graphics::color_space::CGColorSpace;
        use core_graphics::data_provider::CGDataProvider;
        use core_graphics::image::CGImage;
        use std::sync::Arc;

        // 源图 2×2：上排红 (255,0,0)，下排蓝 (0,0,255)；RGBA 不透明。
        let src: Vec<u8> = vec![
            255, 0, 0, 255, 255, 0, 0, 255, // 上排
            0, 0, 255, 255, 0, 0, 255, 255, // 下排
        ];
        let provider = CGDataProvider::from_buffer(Arc::new(src));
        let space = CGColorSpace::create_device_rgb();
        let image = CGImage::new(
            2,
            2,
            8,
            32,
            8,
            &space,
            kCGImageAlphaPremultipliedLast,
            &provider,
            false,
            0,
        );
        let (rgba, w, h) = cgimage_to_rgba(&image).expect("转换应成功");
        assert_eq!((w, h), (2, 2));
        // 上排仍应是红（未上下颠倒），且通道序为 R,G,B,A（不是 BGRA）。
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "左上角应为红且通道序 RGBA");
        assert_eq!(&rgba[8..12], &[0, 0, 255, 255], "左下角应为蓝");
    }

    /// 真实抓取（#[ignore]：需要屏幕录制授权 + 可见窗口）。
    ///
    /// 运行：`cargo test -p hawksnap-platform-mac windowcap -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "需要 macOS 屏幕录制权限与可见窗口"]
    fn real_capture_of_primary_screen_region() {
        let monitors = xcap::Monitor::all().expect("显示器枚举");
        let m = monitors.first().expect("至少一块显示器");
        let (mx, my) = (m.x().unwrap() as f64, m.y().unwrap() as f64);
        let (mw, mh) = (m.width().unwrap() as f64, m.height().unwrap() as f64);
        let rect = GlobalRect::new(mx + 10.0, my + 10.0, 200.0, 100.0);
        let img = capture_global_rect(rect).expect("抓取应成功");
        assert!(img.width >= 200 && img.height >= 100);
        assert_eq!(img.rgba.len() as u32, img.width * img.height * 4);
        let _ = (mw, mh);
    }
}
