//! F9 端到端探针（N3 画中画）：动窗驱动 SCStream 稳定 30fps 交付 + 相机/注入源
//! 合成 → 两段真实录制（无 PiP 基线 5s / PiP 10s）→ pts 连续性（stts 逐帧时长）
//! + PiP 位置/尺寸中途可调证据。stdout 输出机读协议行，tests/real_pip_recording.rs
//! 据此断言并驱动 swift 抽帧工具（scripts/pip_frames.swift / pip_region_motion.swift）。
//!
//! 为什么经子进程（与 record-selfexcl-probe 同理）：NSWindow/NSApplication 是
//! MainThreadOnly，测试线程不能合法建窗；且动窗 + 录制需要主线程跑 runloop。
//!
//! 相机源阶梯（plan：相机缺席 = PiP 静默缺席，绝不阻塞录制）：
//! 1. TCC Authorized → 真实相机（AVCaptureSession 采集道，F9 全真证据）；
//! 2. 相机不可用（headless CLI 环境 requestAccess=false）→ **注入模式**：合成
//!    动画帧经 [`PipLane::inject_frame`] 进入与 delegate 同型的相机槽，走通
//!    「槽 → 合成 → writer → MP4」全程（诚实边界，任务报告记录）。
//!
//! 运行（需屏幕录制权限）：`cargo run -p hawksnap-platform-mac --bin pip-f9-probe`
//! 非 macOS 平台：探针的运行时依赖（AppKit 窗口服务 / ScreenCaptureKit）不存在，
//! 直接跳过（打印说明 + 正常退出）——保证 `cargo test --workspace` 在非 macOS
//! 上照常构建本 bin（探针的集成测试经子进程调用它）。
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("SKIP: 该探针仅 macOS 可用（当前平台无 AppKit/ScreenCaptureKit）");
}

#[cfg(target_os = "macos")]
use hawksnap_platform_mac::record::{
    display_pixels_wide, main_display_id, mp4_duration_secs, mp4_track_dims,
    mp4_video_sample_deltas, pip_rect_px, recording_output_path, LogicalRect, PipLane,
    PipState, PipUpdate, ScreenRecorder,
};
#[cfg(target_os = "macos")]
use objc2::{MainThreadMarker, MainThreadOnly};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSColor, NSWindow, NSWindowStyleMask};
// CoreVideo 直调仅注入模式（test-util）使用——门控与 make_synthetic_frame 同步。
#[cfg(feature = "test-util")]
#[cfg(target_os = "macos")]
use objc2_core_foundation::CFRetained;
#[cfg(feature = "test-util")]
#[cfg(target_os = "macos")]
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{mpsc, Arc};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

/// 区域录制选区（800×600 @ (50,50)，与 N1 端到端同款）。
#[cfg(target_os = "macos")]
const REGION: LogicalRect = LogicalRect { x: 50.0, y: 50.0, w: 800.0, h: 600.0 };
#[cfg(target_os = "macos")]
const NOPIP_SECS: u64 = 5;
#[cfg(target_os = "macos")]
const PIP_SECS: u64 = 10;

#[cfg(target_os = "macos")]
fn main() {
    let mtm = MainThreadMarker::new().expect("探针必须在主线程运行");
    let _app = NSApplication::sharedApplication(mtm);

    let dir = std::env::temp_dir().join(format!("hawksnap-n3-pip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    // —— 动窗：400×300 无边框窗口，落在录区内，录制期间 30Hz+ 平移（SCStream 按
    //    内容变化交付——窗口移动强制逐帧交付，pts 连续性断言的源侧前提）。
    let screen_h = {
        // SAFETY: MainThreadOnly 类在主线程查询。
        let s = objc2_app_kit::NSScreen::mainScreen(mtm).expect("主屏");
        s.frame().size.height
    };
    // 显示器坐标（左上原点）(100, 120) → AppKit 坐标（左下原点）翻转。
    let win_rect = NSRect {
        origin: NSPoint { x: 100.0, y: screen_h - 120.0 - 300.0 },
        size: NSSize { width: 400.0, height: 300.0 },
    };
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            win_rect,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setBackgroundColor(Some(&NSColor::systemOrangeColor()));
    window.orderFrontRegardless();

    // —— Phase A：无 PiP 基线（动窗运动，5s）。CPU 采样线程同期运行（F7 增量
    //    证据：cpu_pip − cpu_nopip ≤ 2%，G 预算口径按 release——debug 构建只打印）。
    let pid = std::process::id();
    let nopip_path = recording_output_path(&dir.join("a"), chrono::Local::now()).expect("path A");
    let nopip_for_thread = nopip_path.clone();
    let sampler_a = start_cpu_sampler(pid);
    let nopip_bytes = run_motion_loop(&window, move || {
        // 区域录制（800×600 @ (50,50)）：动窗落在录区内，产物小、PiP 几何受控。
        let rec = ScreenRecorder::start_region(
            nopip_for_thread.clone(),
            main_display_id(),
            REGION,
            None,
        )
        .expect("start A");
        std::thread::sleep(Duration::from_secs(NOPIP_SECS));
        rec.stop().expect("stop A");
        std::fs::metadata(&nopip_for_thread).map(|m| m.len()).unwrap_or(0)
    });
    let cpu_nopip = stop_cpu_sampler(sampler_a);

    // —— Phase B：PiP 10s（相机阶梯：真机优先，缺席注入）。 ——
    let pip_path = recording_output_path(&dir.join("b"), chrono::Local::now()).expect("path B");
    let lane = Arc::new(PipLane::new());
    let mode = match lane.update(PipUpdate { visible: Some(true), ..Default::default() }) {
        Ok(()) => {
            println!("[probe] mode=camera");
            "camera"
        }
        Err(e) => {
            // 相机缺席 → 注入模式（test-util 门控的合成帧槽——评审清单④）。
            #[cfg(feature = "test-util")]
            {
                println!("[probe] camera unavailable ({e}) → injected synthetic source");
                lane.set_state_for_probe(PipState { visible: true, ..Default::default() });
                start_injector(lane.clone());
                println!("[probe] mode=injected");
                "injected"
            }
            #[cfg(not(feature = "test-util"))]
            {
                let _ = e;
                eprintln!(
                    "[probe] FAIL: 相机不可用且未启用 test-util（注入模式缺席）——\
                     以 --features test-util 重跑（F9 注入模式载体，评审清单④）"
                );
                std::process::exit(3);
            }
        }
    };
    let pip_path_for_thread = pip_path.clone();
    let sampler_b = start_cpu_sampler(pid);
    let pip_bytes = run_motion_loop(&window, move || {
        let rec = ScreenRecorder::start_region(
            pip_path_for_thread.clone(),
            main_display_id(),
            REGION,
            Some(lane.clone()),
        )
        .expect("start B");
        // F9「可调」证据：t=3s 改位置（下一帧生效 ≤33ms），t=6s 改回默认位 + 改尺寸。
        std::thread::sleep(Duration::from_secs(3));
        lane.update(PipUpdate { x: Some(Some(40.0)), y: Some(Some(40.0)), ..Default::default() })
            .expect("pip move");
        std::thread::sleep(Duration::from_secs(3));
        lane.update(PipUpdate { x: Some(None), y: Some(None), size: Some(160.0), ..Default::default() })
            .expect("pip resize");
        std::thread::sleep(Duration::from_secs(PIP_SECS - 6));
        rec.stop().expect("stop B");
        std::fs::metadata(&pip_path_for_thread).map(|m| m.len()).unwrap_or(0)
    });
    let cpu_pip = stop_cpu_sampler(sampler_b);

    // —— 证据解析（机读协议行）。 ——
    let pip_data = std::fs::read(&pip_path).expect("read pip mp4");
    let dims = mp4_track_dims(&pip_data).expect("tkhd");
    let dur = mp4_duration_secs(&pip_data).expect("mvhd");
    let scale = display_scale();
    let rect = pip_rect_px(dims.0, dims.1, scale, &PipState { visible: true, ..Default::default() })
        .expect("pip rect");
    let deltas = mp4_video_sample_deltas(&pip_data).expect("stts");
    let stats = cadence_stats(&deltas);
    println!("[probe] nopip={}", nopip_path.display());
    println!("[probe] pip={}", pip_path.display());
    println!(
        "[probe] dims={}x{} scale={scale:.2} rect={},{},{} mode={mode}",
        dims.0, dims.1, rect.x, rect.y, rect.size
    );
    println!(
        "[probe] stats_pip: n={} median_ms={:.2} gap={:.1}% inband={:.1}% dur={:.2}s bytes={}",
        deltas.len(),
        stats.median * 1000.0,
        stats.gap_pct,
        stats.inband_pct,
        dur,
        pip_bytes
    );
    println!("[probe] hist_ms: {}", stats.histogram);

    // —— 断言 1：F9 主画面无掉帧。口径（实证校准，见任务报告）：SCStream 按
    //     内容变化交付，PTS 源自主机时钟——帧间天然抖动（实测 ±10ms 级），±10%
    //     带内占比只作信息量打印；**管线掉帧的指纹是 delta 呈 2×/3× 主节奏的
    //     离散跳变**——断言：中位数 ≈ 1/30s（节奏达标）+ 大间隙（>1.8×median，
    //     即至少缺 1 帧）≤ 5%（源侧良性跳帧 + ready-gate 良性丢帧的预算；
    //     持续合成失败会是 50%+ 级别的指纹）。
    assert!(
        (1.0 / 32.0..=1.0 / 28.0).contains(&stats.median),
        "主节奏异常：median {:.1}ms 应 ≈ 1/30s",
        stats.median * 1000.0
    );
    assert!(
        stats.gap_pct <= 5.0,
        "pts 连续性失败：大间隙占比 {:.1}% > 5%（管线掉帧指纹）",
        stats.gap_pct
    );
    // —— 断言 2：PiP 产物大于无 PiP 基线（动窗运动相同，PiP 贴片新增运动熵）。
    let growth = pip_bytes as f64 / nopip_bytes.max(1) as f64;
    println!("[probe] size growth: pip={pip_bytes}B vs nopip={nopip_bytes}B (x{growth:.2})");
    assert!(pip_bytes > nopip_bytes, "PiP 产物应大于同动窗无 PiP 基线（贴片新增运动熵）");
    println!(
        "[probe] frames={} over {dur:.2}s → avg {:.1}fps",
        deltas.len(),
        deltas.len() as f64 / dur.max(0.001)
    );
    // —— F7 增量（合成 ≤2% 预算；release 口径，debug 打印不断言——未优化 memcpy
    //    计时失真，N1 同款纪律）。ps %cpu 是近期衰减均值，两段相邻串行，差值即
    //    合成增量的工程估计（同屏同动窗，唯一差异 = PiP 合成路径）。
    let delta = cpu_pip - cpu_nopip;
    println!("[probe] cpu: nopip={cpu_nopip:.2}% pip={cpu_pip:.2}% delta={delta:+.2}%");
    if !cfg!(debug_assertions) {
        assert!(delta <= 2.0, "F7 预算：PiP 合成增量 {delta:+.2}% > 2%");
    }
    println!("[probe] PASS");
    if std::env::var("HAWKSNAP_RECORD_KEEP").is_ok() {
        println!("[probe] 产物保留: {}", dir.display());
    } else {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 主线程动窗 + 录制交替泵：录制闭包在独立线程跑，动窗泵在主线程 60Hz 运行
/// （窗口移动 → 窗口服务器重绘 → SCStream 内容变化逐帧交付），闭包返回即停泵。
#[cfg(target_os = "macos")]
fn run_motion_loop<R: Send + 'static>(
    window: &NSWindow,
    f: impl FnOnce() -> R + Send + 'static,
) -> R {
    let runloop = NSRunLoop::currentRunLoop();
    // SAFETY: 常量 extern static 读取。
    let mode = unsafe { NSDefaultRunLoopMode };
    let (tx, rx) = mpsc::channel::<R>();
    let handle = std::thread::spawn(move || tx.send(f()).expect("record thread send"));
    let start = Instant::now();
    let base = window.frame().origin;
    let mut i: i64 = 0;
    let mut result: Option<R> = None;
    loop {
        // ±4px 交替平移（内容变化 → SCStream 逐帧交付）。
        window.setFrameOrigin(NSPoint {
            x: base.x + if i % 2 == 0 { 4.0 } else { -4.0 },
            y: base.y + if i % 4 < 2 { 2.0 } else { -2.0 },
        });
        i += 1;
        runloop.runMode_beforeDate(mode, &NSDate::dateWithTimeIntervalSinceNow(1.0 / 60.0));
        if let Ok(r) = rx.try_recv() {
            result = Some(r);
            break;
        }
        if start.elapsed() > Duration::from_secs(120) {
            break; // 录制线程卡死护栏（正常不可达；join 收尸后 expect 报错）
        }
    }
    let _ = handle.join();
    result.expect("录制线程未返回结果（超时/panic，见上方日志）")
}

/// 注入模式：30fps 合成动画帧（7 色轮转 + 游走白方块）→ 相机槽；线程在
/// PIP_SECS+5s 后自停（录制收尾后无消费方，不留残）。
#[cfg(feature = "test-util")]
#[cfg(target_os = "macos")]
fn start_injector(lane: Arc<PipLane>) {
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut n: u64 = 0;
        while t0.elapsed() < Duration::from_secs(PIP_SECS + 5) {
            let f0 = Instant::now();
            if let Some(buf) = make_synthetic_frame(640, 480, n) {
                lane.inject_frame(buf);
            }
            n += 1;
            let period = Duration::from_secs_f64(1.0 / 30.0);
            let spent = f0.elapsed();
            if spent < period {
                std::thread::sleep(period - spent);
            }
        }
    });
}

/// 合成一帧 BGRA（移动方块 + 色移——帧间必变，运动熵源）。
#[cfg(feature = "test-util")]
#[cfg(target_os = "macos")]
fn make_synthetic_frame(w: usize, h: usize, n: u64) -> Option<CFRetained<CVPixelBuffer>> {
    unsafe {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = CVPixelBufferCreate(
            None,
            w,
            h,
            kCVPixelFormatType_32BGRA as u32,
            None,
            std::ptr::NonNull::from(&mut raw),
        );
        if status != 0 {
            return None;
        }
        let buf = match std::ptr::NonNull::new(raw) {
            Some(p) => CFRetained::from_raw(p),
            None => return None,
        };
        CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags(0));
        let base = CVPixelBufferGetBaseAddress(&buf) as *mut u8;
        let stride = CVPixelBufferGetBytesPerRow(&buf);
        let height = CVPixelBufferGetHeight(&buf);
        let width = CVPixelBufferGetWidth(&buf);
        // 背景：7 色逐帧轮转（运动熵信号——任意 ≥1 帧的采样间隔都落在不同色上，
        // 相邻色差 ~100 灰阶 → 盒式降采样后帧间差仍 ~50，使 PiP 区域运动熵显著
        // 高于任何桌面背景噪声，F9 断言鲁棒；取 7 是刻意避开与采样间隔 6 帧的
        // 公倍数对齐——2 色翻转在偶数帧间隔上会回到同色、差值归零的教训）；
        // 方块：48×48 白色，按帧水平/垂直游走（位置变化的可见指纹）。
        const PALETTE: [[u8; 4]; 7] = [
            [30, 30, 30, 255],
            [40, 40, 200, 255],
            [40, 200, 40, 255],
            [200, 40, 40, 255],
            [40, 200, 200, 255],
            [200, 200, 40, 255],
            [200, 40, 200, 255],
        ];
        let bg: [u8; 4] = PALETTE[(n % 7) as usize];
        let bx = ((n * 7) % (width as u64 - 48)) as usize;
        let by = ((n * 13) % (height as u64 - 48)) as usize;
        for row in 0..height {
            // SAFETY: 锁定窗口内的行切片写入；长度 width*4 ≤ stride。
            let line = std::slice::from_raw_parts_mut(base.add(row * stride), width * 4);
            let in_rows = row >= by && row < by + 48;
            for (ci, px) in line.chunks_exact_mut(4).enumerate() {
                let in_square = in_rows && ci >= bx && ci < bx + 48;
                let color: [u8; 4] = if in_square { [255, 255, 255, 255] } else { bg };
                px.copy_from_slice(&color);
            }
        }
        CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags(0));
        Some(buf)
    }
}

/// 主显示器缩放（物理/逻辑）——与 begin() 同一 CG 口径。
#[cfg(target_os = "macos")]
fn display_scale() -> f64 {
    let id = main_display_id();
    let (pw, bounds) = (display_pixels_wide(id), hawksnap_platform_mac::record::display_bounds(id));
    if bounds.size.width > 0.0 {
        pw as f64 / bounds.size.width
    } else {
        1.0
    }
}

#[cfg(target_os = "macos")]
type CpuSampler = Arc<(AtomicBool, std::sync::Mutex<Vec<f64>>)>;

/// CPU 采样线程（N1 real_region 同款 ps %cpu 口径）：500ms 一次入共享槽；
/// stop_cpu_sampler 置停 + 取均值。
#[cfg(target_os = "macos")]
fn start_cpu_sampler(pid: u32) -> CpuSampler {
    let shared: CpuSampler = Arc::new((AtomicBool::new(false), std::sync::Mutex::new(Vec::new())));
    let s2 = shared.clone();
    std::thread::spawn(move || {
        while !s2.0.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            if let Some(v) = sample_cpu(pid) {
                s2.1.lock().unwrap().push(v);
            }
        }
    });
    shared
}

#[cfg(target_os = "macos")]
fn stop_cpu_sampler(sampler: CpuSampler) -> f64 {
    sampler.0.store(true, Ordering::Relaxed);
    // 等一个采样周期，保证末段样本落账（500ms 周期 + 余量）。
    std::thread::sleep(Duration::from_millis(600));
    let samples = std::mem::take(&mut *sampler.1.lock().unwrap());
    if samples.is_empty() {
        0.0
    } else {
        samples.iter().sum::<f64>() / samples.len() as f64
    }
}

#[cfg(target_os = "macos")]
fn sample_cpu(pid: u32) -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "%cpu=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// 逐帧时长统计：主节奏（中位数）+ 大间隙占比（>1.8×median = 至少缺 1 帧，
/// 管线掉帧指纹）+ ±10% 带内占比（信息量）+ 直方图（证据留痕）。
#[cfg(target_os = "macos")]
struct CadenceStats {
    median: f64,
    gap_pct: f64,
    inband_pct: f64,
    histogram: String,
}

#[cfg(target_os = "macos")]
fn cadence_stats(deltas: &[f64]) -> CadenceStats {
    if deltas.is_empty() {
        return CadenceStats { median: 0.0, gap_pct: 0.0, inband_pct: 0.0, histogram: "n/a".into() };
    }
    let mut sorted = deltas.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let inband = deltas
        .iter()
        .filter(|d| (**d - median).abs() <= median * 0.10)
        .count();
    let gaps = deltas.iter().filter(|d| **d > median * 1.8).count();
    // 直方图：按 10ms 桶聚类（1/30s 主峰 + 缺帧倍峰一目了然）。
    let mut hist: std::collections::BTreeMap<usize, usize> = Default::default();
    for d in deltas {
        *hist.entry((d * 1000.0 / 10.0).round() as usize).or_default() += 1;
    }
    let histogram = hist
        .iter()
        .map(|(k, v)| format!("{}ms:{}", k * 10, v))
        .collect::<Vec<_>>()
        .join(" ");
    CadenceStats {
        median,
        gap_pct: gaps as f64 / deltas.len() as f64 * 100.0,
        inband_pct: inband as f64 / deltas.len() as f64 * 100.0,
        histogram,
    }
}
