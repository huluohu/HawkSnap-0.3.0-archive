//! 录屏自排除端到端探针（终审修复 1 的 ==1 断言载体）。
//!
//! 为什么存在：`ScreenRecorder::start` 的 excludingApplications 匹配依赖
//! SCShareableContent.applications 含本进程，而 applications 只列出「名下有窗口」
//! 的进程——`cargo test` 测试二进制零窗口，恒走降级路径（excluded_apps=0，见
//! record.rs 单测注）。本探针以**主线程**建 NSWindow 后再录制，复现生产 app
//! 的前置条件，断言 `excluded_apps == 1`（本应用已在排除列表）。
//!
//! 运行（需屏幕录制权限）：`cargo run -p hawksnap-platform-mac --bin record-selfexcl-probe`
//! 经 `tests/real_record_selfexcl.rs`（#[ignore]）以子进程方式纳入测试口径。
//! NSWindow/NSApplication 均是 MainThreadOnly——探针必须在主线程跑（bin 的
//! main 即主线程；测试不能直接跑，测试线程非主线程，故经子进程）。
//! 非 macOS 平台：探针的运行时依赖（AppKit 窗口服务 / ScreenCaptureKit）不存在，
//! 直接跳过（打印说明 + 正常退出）——保证 `cargo test --workspace` 在非 macOS
//! 上照常构建本 bin（探针的集成测试经子进程调用它）。
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("SKIP: 该探针仅 macOS 可用（当前平台无 AppKit/ScreenCaptureKit）");
}

#[cfg(target_os = "macos")]
use hawksnap_platform_mac::record::{ScreenRecorder, recording_output_path};
#[cfg(target_os = "macos")]
use objc2::{MainThreadMarker, MainThreadOnly};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSWindow, NSWindowStyleMask};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSPoint, NSRect, NSSize};

#[cfg(target_os = "macos")]
fn main() {
    // 1. 主线程（MainThreadOnly 契约）+ AppKit 连接（窗口服务注册的前提）。
    let mtm = MainThreadMarker::new().expect("探针必须在主线程运行");
    let _app = NSApplication::sharedApplication(mtm);

    // 2. 建一个最小 NSWindow（120×40，屏外坐标，从不 orderFront）——对应生产路径
    //    「record_start 先建悬浮条再启动流」的前置（record.rs start() 注②的实测：
    //    名下有任意 NSWindow 即被 applications 列出，无需在屏）。
    let rect = NSRect {
        origin: NSPoint { x: -2000.0, y: -2000.0 },
        size: NSSize { width: 120.0, height: 40.0 },
    };
    // SAFETY: designated initializer，参数均为本地合法值；MainThreadOnly 类在主线程。
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // 刻意不 orderFront（降级实证口径：仅「名下有窗口」即可）。
    println!("[probe] window created: {:?}", window.frame());

    // 3. 录 3 秒（真显示器 + 系统声，与生产管线同一入口）。
    let secs: u64 = std::env::var("PROBE_RECORD_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let dir = std::env::temp_dir().join(format!("hawksnap-selfexcl-probe-{}", std::process::id()));
    let path = recording_output_path(&dir, chrono::Local::now()).expect("输出路径");
    let rec = ScreenRecorder::start(path.clone(), None).expect("ScreenRecorder::start");
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let info = rec.stop().expect("ScreenRecorder::stop");
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&dir);

    println!(
        "[probe] recorded {secs}s: {}x{} audio={} excluded_apps={} mp4={size}B",
        info.width, info.height, info.has_audio, info.excluded_apps
    );
    if size < 5_000 {
        eprintln!("[probe] FAIL: MP4 过小（{size}B），录制管线异常");
        std::process::exit(2);
    }
    if info.excluded_apps != 1 {
        eprintln!(
            "[probe] FAIL: excludingApplications 期望恰含本应用 1 条，实际 {}（悬浮条将入画）",
            info.excluded_apps
        );
        std::process::exit(1);
    }
    println!("[probe] PASS: excluded_apps=1（本应用已自排除）");
}
