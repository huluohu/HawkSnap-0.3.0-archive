//! N2 GIF 编码（gifski）：录制旁路帧队列的消费侧。
//!
//! ## 分工（plan Task N2 裁定）
//!
//! 抽帧在平台侧（`hawksnap-platform-mac::record`：SCStream 视频回调 15fps 节拍 →
//! 最长边 ≤800 降采样 → 有界 32MB 队列丢最旧）；本模块只做**停止后编码**：
//! drain 队列 → `gifski::new` + `Collector::add_frame_rgba × N` → `Writer::write`
//! 落盘 `~/Movies/HawkSnap/<ts>.gif`。gifski 的 `Collector`/`Writer` 必须并行使用
//! （其内部 crossbeam 有界通道在 write 未运行时会背压阻塞投喂）——`encode_gif`
//! 用 `thread::scope` 双线程闭环（writer 线程 + 投喂主线程）。
//!
//! ## gifski API 对齐（1.34.0，registry 源码核对）
//!
//! `gifski::new(Settings) -> (Collector, Writer)`；`Collector::add_frame_rgba
//! (frame_index, ImgVec<RGBA8>, presentation_timestamp)`（注意：非 C API 的
//! `add_rgba_frame`——任务简报中的签名是 C 绑定形态，Rust 库形态如上）；
//! `Writer::write(self, W: Write, &mut dyn ProgressReporter)` 阻塞至 Collector
//! drop。类型 `ImgVec`/`RGBA8` 由 `gifski::collector` 再导出；`Repeat` 是
//! `gif::Repeat` 的别名（枚举变体可经别名构造）。
//!
//! ## 编码裁定
//!
//! - `repeat: Repeat::Infinite`——录屏动图语义（Snipaste/截图工具惯例：GIF 循环）。
//! - `quality: 90`（gifski 文档有效区间 50-100；100 为推荐值但量化耗时更高，
//!   F8 预算内取 90：文件更小、色彩损失在录屏场景不可感）。
//! - `width/height: None`——帧已由抽帧通道钳到最长边 ≤800（gifski 的 None/None
//!   自动缩放因子对 ≤800×800 面积恒为 1，不会再缩）。
//! - PTS 归零：首帧平移到 0（gifski 契约「first frame should start at 0」）；
//!   帧 PTS 为平移后值（PauseClock 出口），GIF 时间线与 MP4 一致（暂停共同空洞）。
//! - 超时预算：F8 = 150 帧 ≤10s；硬超时 60s 由 `ProgressReporter::increase`
//!   返回 false 实现（gifski 原生中止路径，无泄漏线程）。

// —— 非 macOS 平台放行约定（M4 P0 评审修复轮）——
//
// 本文件含 macOS 专属能力（录屏 / 画中画 / 贴图 / 按键显示 / 长截图 等）。非 macOS
// 构建下这些项**无人调用**——这是「能力缺席」的正常结果，不是代码缺陷。故按项逐个
// 放行：`#[cfg_attr(not(target_os = "macos"), allow(dead_code))]`（仅 macOS 分支才用
// 的局部变量用 `unused_variables`，仅 macOS 才用的导入用 `unused_imports`）。
//
// **逐项**放行而非 crate 级 blanket allow：crate 级 `allow(dead_code, unused_*)`
// 会把**未来新增**的同类问题一并吞掉；逐项放行后，新增项若在非 macOS 上真的无人用，
// 依然照报。macOS 构建下这些属性恒不生效（`not(target_os = "macos")` 为假）——
// macOS 的 lint 纪律零变化。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gifski::collector::{ImgVec, RGBA8};
use gifski::progress::ProgressReporter;
use gifski::{Repeat, Settings};

use hawksnap_platform_mac::record::GifFrame;

/// gifski 量化质量（50-100 有效区间，见模块注裁定）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const GIF_QUALITY: u8 = 90;

/// 编码硬超时（F8 预算的 6 倍兜底：正常 150 帧 ≤10s，超时 = gifski 异常 → 走
/// MP4 降级路径而不是卡死 stop）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const GIF_ENCODE_DEADLINE: Duration = Duration::from_secs(60);

/// 编码产物（入库 + record_stop 返回值的数据源）。
#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct GifOutcome {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// GIF 总时长（秒）：末帧归零 PTS（与 history.duration 同口径）。
    pub duration: f64,
    pub frames: usize,
}

/// 超时中止报告器：`increase` 越线返回 false → gifski 走原生 Aborted 中止，
/// writer 线程退出 → Collector 通道关闭 → 投喂循环随之收尾（无孤儿线程）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct DeadlineReporter {
    deadline: Instant,
}

impl ProgressReporter for DeadlineReporter {
    fn increase(&mut self) -> bool {
        Instant::now() < self.deadline
    }
}

/// 编码帧序列到 `out`（.gif）。帧需非空且按 PTS 升序（抽帧通道的天然产出）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn encode_gif(frames: &[GifFrame], out: &Path) -> Result<GifOutcome, String> {
    let Some(first) = frames.first() else {
        return Err("GIF 编码无帧（录制过短或抽帧失败）".into());
    };
    let (w, h) = (first.width, first.height);
    let first_pts = first.pts;
    let (collector, writer) = gifski::new(Settings {
        width: None,
        height: None,
        quality: GIF_QUALITY,
        fast: false,
        repeat: Repeat::Infinite,
    })
    .map_err(|e| format!("gifski 初始化失败: {e}"))?;

    let file = std::fs::File::create(out)
        .map_err(|e| format!("创建 GIF 输出文件 {} 失败: {e}", out.display()))?;
    let deadline = Instant::now() + GIF_ENCODE_DEADLINE;

    let write_result: Result<(), String> = std::thread::scope(|scope| {
        let writer_handle = scope.spawn(move || {
            writer.write(std::io::BufWriter::new(file), &mut DeadlineReporter { deadline })
        });
        let feed: Result<(), String> = (|| {
            for (i, f) in frames.iter().enumerate() {
                // RGBA 字节 → RGBA8 像素结构（gifski 输入形态；一次拷贝 ~1.4MB/帧，
                // 相对量化编码成本可忽略）。
                let pixels: Vec<RGBA8> = f
                    .rgba
                    .chunks_exact(4)
                    .map(|c| RGBA8::new(c[0], c[1], c[2], c[3]))
                    .collect();
                collector
                    .add_frame_rgba(i, ImgVec::new(pixels, f.width as usize, f.height as usize), f.pts - first_pts)
                    .map_err(|e| format!("GIF 帧投递失败: {e}"))?;
            }
            Ok(())
        })();
        drop(collector); // write 只在 collector drop 后收尾（gifski 契约）
        let written = writer_handle
            .join()
            .map_err(|_| "GIF writer 线程 panic".to_string())?;
        feed.and(written.map_err(|e| format!("GIF 写入失败: {e}")))?;
        Ok(())
    });
    write_result?;

    let last = frames.last().expect("frames 非空已验");
    Ok(GifOutcome {
        path: out.to_path_buf(),
        width: w,
        height: h,
        duration: last.pts - first_pts,
        frames: frames.len(),
    })
}

/// 停止收口 GIF（record_stop 的 gif 分支）：drain 旁路队列 → 编码 → .gif 落盘。
/// 产物路径 = MP4 路径换扩展名（同秒时间戳茎）；调用方在成功后删除 MP4 中间产物。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn finish_gif_recording(
    lane: Option<&Arc<hawksnap_platform_mac::record::GifLane>>,
    mp4_path: &Path,
) -> Result<GifOutcome, String> {
    let Some(lane) = lane else {
        return Err("GIF 通道缺失（会话状态异常）".into());
    };
    let frames = lane.drain();
    if frames.is_empty() {
        return Err("GIF 通道无帧（录制过短或抽帧已禁用）".into());
    }
    let out = mp4_path.with_extension("gif");
    let outcome = encode_gif(&frames, &out)?;
    eprintln!(
        "[hawksnap] GIF 导出完成: {} ({} 帧 {}×{}, {:.2}s)",
        outcome.path.display(),
        outcome.frames,
        outcome.width,
        outcome.height,
        outcome.duration
    );
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// 测试：F8 编码计时（#[ignore]，开发机直跑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hawksnap_platform_mac::record::GifLane;

    /// 合成帧（渐变色块：gifski 量化器对纯色序列会高度优化，渐变更接近真实
    /// 屏幕内容的熵——计时证据更诚实）。
    fn synth_frame(i: usize, w: u32, h: u32) -> GifFrame {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = ((x + y + (i as u32) * 7) % 256) as u8;
                rgba.extend_from_slice(&[v, v.wrapping_mul(3), v.wrapping_add(i as u8), 255]);
            }
        }
        GifFrame { pts: i as f64 / 15.0, width: w, height: h, rgba }
    }

    /// F8 编码计时（plan 验收口径）：合成 150 帧 800×600（=10s@15fps）→ 计时
    /// ≤10s + GIF 魔数 "GIF8" + 非空体积。debug 构建的 gifski（依赖默认 opt-0）
    /// 计时失真 → 预算断言仅 release 生效（F7 CPU 断言同口径先例），debug 只打印。
    #[test]
    #[ignore = "F8 计时基准；cargo test -p hawksnap --lib gif_encode --release -- --ignored 运行"]
    fn gif_encode_150_frames_800x600_within_f8_budget() {
        let frames: Vec<GifFrame> = (0..150).map(|i| synth_frame(i, 800, 600)).collect();
        let out = std::env::temp_dir().join(format!("hawksnap-n2-f8-{}.gif", std::process::id()));
        let started = Instant::now();
        let outcome = encode_gif(&frames, &out).expect("encode");
        let elapsed = started.elapsed();
        println!(
            "[n2] F8 encode: {} frames 800×600 in {:.2}s (budget 10s), file {} bytes",
            outcome.frames,
            elapsed.as_secs_f64(),
            std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0)
        );
        let data = std::fs::read(&out).expect("read gif");
        assert!(data.len() > 4 && &data[..3] == b"GIF", "GIF 魔数（GIF87a/GIF89a 前缀 GIF8 形态）");
        assert!(data.len() > 10_000, "150 帧产物应非空壳，实际 {} 字节", data.len());
        assert_eq!(outcome.frames, 150);
        assert!((outcome.duration - 149.0 / 15.0).abs() < 1e-9, "时长 = 末帧归零 PTS");
        if !cfg!(debug_assertions) {
            assert!(elapsed <= Duration::from_secs(10), "F8 预算：{elapsed:?} ≤10s");
        }
        let _ = std::fs::remove_file(&out);
    }

    /// 真实端到端（F8 现场证据）：800×600 区域 GIF 通道录 10s → stop → drain →
    /// 编码 → 文件魔数 + 编码计时 + 抽帧计数报告。需屏幕录制权限。
    #[test]
    #[ignore = "需要 macOS 屏幕录制权限 + 真实显示器；cargo test -p hawksnap --lib real_region_gif --release -- --ignored 运行"]
    fn real_region_gif_lane_10s_encodes_and_plays() {
        use hawksnap_platform_mac::record::{LogicalRect, ScreenRecorder};
        let dir = std::env::temp_dir().join(format!("hawksnap-n2-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("n2-region.gif-test.mp4"); // 茎任意；编码路径走 with_extension
        let region = LogicalRect { x: 50.0, y: 50.0, w: 800.0, h: 600.0 };
        let lane = Arc::new(GifLane::new());
        let rec = ScreenRecorder::start_region_gif(
            mp4.clone(),
            hawksnap_platform_mac::record::main_display_id(),
            region,
            lane.clone(),
            None, // GIF 证据基线不含 PiP（N3 的 F9 端到端在 pip-f9-probe）
        )
        .expect("start_region_gif");
        std::thread::sleep(Duration::from_secs(10));
        let info = rec.stop().expect("stop");
        let started = Instant::now();
        let outcome = finish_gif_recording(Some(&lane), &mp4).expect("gif encode");
        let elapsed = started.elapsed();
        println!(
            "[n2] real 10s region gif: mp4 {}×{}, tapped {} frames (≤150), \
             encode {:.2}s (budget 10s), gif {} bytes, duration {:.2}s → {}",
            info.width,
            info.height,
            outcome.frames,
            elapsed.as_secs_f64(),
            std::fs::metadata(&outcome.path).map(|m| m.len()).unwrap_or(0),
            outcome.duration,
            outcome.path.display()
        );
        let data = std::fs::read(&outcome.path).expect("read gif");
        assert!(data.len() > 6 && &data[..3] == b"GIF", "GIF 魔数");
        if !cfg!(debug_assertions) {
            assert!(elapsed <= Duration::from_secs(10), "F8 预算：{elapsed:?} ≤10s");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无进度报告器路径（NoProgress 常量性）：极小帧序列可编码（契约冒烟）。
    #[test]
    fn encode_tiny_sequence_smoke() {
        let frames = vec![
            GifFrame { pts: 0.0, width: 4, height: 4, rgba: vec![10; 4 * 4 * 4] },
            GifFrame { pts: 1.0 / 15.0, width: 4, height: 4, rgba: vec![200; 4 * 4 * 4] },
        ];
        let out = std::env::temp_dir().join(format!("hawksnap-n2-tiny-{}.gif", std::process::id()));
        let outcome = encode_gif(&frames, &out).expect("encode tiny");
        assert_eq!(outcome.frames, 2);
        let data = std::fs::read(&out).unwrap();
        assert!(&data[..3] == b"GIF");
        let _ = std::fs::remove_file(&out);
    }
}
