use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use super::flow::start_capture_flow;

// —— Plan B Task 8：延时截图（托盘子菜单 + 倒计时浮窗） ——

/// 延时进行中门闩（静态状态机，与 HOTKEY_ERROR 同为 static）：try_begin_delay 独占置位，
/// worker 到点/中止时清零。重复启动经此拒绝（「已有延时截图进行中」）。
pub(super) static DELAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 中止旗标：cancel_delayed_capture 置 true，worker 循环每轮（≤50ms）检查——真则隐藏
/// 倒计时窗、清门闩、不截图。每轮 start_delayed_capture 在取门闩前复位（先清后取：
/// 清后至取闩间落下的 cancel 只会留 true 或判非活动期拒绝，不会像先取后清那样被本轮
/// 复位静默抹掉）。
pub(super) static DELAY_ABORT: AtomicBool = AtomicBool::new(false);

/// 最近一次广播的剩余秒数（pull 通道）：倒计时窗口首建时 webview 冷加载（~267ms），
/// 首条 hawk://countdown emit 早于页面 listen 必丢——与 HOTKEY_ERROR 同一问题形态。
/// 页面 mount 时经 countdown_remaining 命令拉取补显（仅 DELAY_ACTIVE 时有效）。
static DELAY_REMAINING: AtomicU64 = AtomicU64::new(0);

/// hawk://countdown payload：剩余整秒数。secs → 1 逐秒广播，{remaining: 0} 在触发
/// 截图前一瞬间发出。单词字段，无需 rename（与 AxFrameJson 同款 derive 保持一致）。
#[derive(Serialize, Clone)]
struct CountdownPayload {
    remaining: u64,
}

/// worker 轮询周期：终止判定误差上界 = 一个周期（实际经「末段向 deadline 对齐」的
/// sleep 裁剪后为亚毫秒过冲，稳拿 spec §3.1 F4 的 ≤100ms 验收）。
const DELAY_POLL: Duration = Duration::from_millis(50);

/// 延时时长白名单（spec §3.1：1/2/3/5 秒可选）：纯逻辑可测，通过则原样返回。
fn validate_delay_secs(secs: u64) -> Result<u64, String> {
    matches!(secs, 1 | 2 | 3 | 5).then_some(secs).ok_or_else(|| "仅支持 1/2/3/5 秒".into())
}

/// 门闩独占（纯逻辑可测）：swap(true) 前值为 false 才算抢到——进行中重复启动返回 false。
fn try_begin_delay(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

/// 倒计时浮窗（常驻复用，与 overlay 窗口同款「一次创建、show/hide」模式）：
/// 200×120 逻辑尺寸，无边框、置顶、不可缩放、skip_taskbar、非透明（实心深色底，
/// 透明只为 overlay 冻结帧服务）。
///
/// 位置裁定（controller）：不指定 position 先建、再 `center()`——CAPTURES 首entry
/// 定位 / 硬编码分辨率居中均被否，center() 由 OS 在窗口当前所在屏居中，稳妥零假设。
/// 首建以 visible(false) 起建，center 完成再 show，避免窗口先闪现在默认位置再跳心。
/// 必须在非主线程调用（建窗/show 均为投递给主线程事件循环的消息，close 才有主线程
/// 禁忌，但本函数从 start_delayed_capture 进入，本就运行在 worker 线程）。
fn show_countdown_window(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("countdown") {
        // 复用：页面与监听常驻，仅重新居中并显示（窗口 hide 期间事件照常接收）。
        let _ = w.center();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let win = WebviewWindowBuilder::new(app, "countdown", WebviewUrl::App("countdown?window=countdown".into()))
        .inner_size(200.0, 120.0)
        .decorations(false)
        .always_on_top(true)
        .resizable(false)
        .skip_taskbar(true)
        .transparent(false) // 实心深色底（brief：非透明；前端自绘 #0D1522）
        .visible(false)     // 先建后 center 再 show，杜绝默认位置闪现
        .build().map_err(|e| e.to_string())?;
    let _ = win.center();
    let _ = win.show();
    let _ = win.set_focus();
    Ok(())
}

fn hide_countdown_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("countdown") {
        let _ = w.hide();
    }
}

/// 启动延时截图（spec §3.1）：校验 → 门闩独占 → 建/复用倒计时窗 → deadline 轮询。
///
/// 必须在非主线程调用：本命令标记 `async`，前端 invoke 由 tauri 派到异步运行时线程
/// 执行（不带 async 的 sync 命令在 IPC 线程内联运行——macOS/wry 即主线程，整段倒计时
/// 会冻结主线程，且建窗/show 落在禁忌线程）；托盘菜单处理函数本身跑在主线程，故先
/// spawn 平台线程再进入。两路径都满足非主线程约束（start_capture_flow 的主线程禁忌
/// 同源）。
///
/// 时序（controller 裁定 1，deadline-based）：`deadline = Instant::now() + secs`，
/// worker 每 ≤50ms 检查 DELAY_ABORT 与剩余整秒；整秒变化才 emit {remaining: n}
/// （ceil 语义：剩余 2.95s 显示 3——「3、2、1」各占满一秒后归零），{remaining: 0}
/// 在触发截图前一瞬发出。到点顺序固定：emit 0 → 隐藏窗口 → 清门闩 → start_capture_flow
/// （已在 worker 线程，安全；失败走 hawk://capture-error，与托盘 shot 同口径）。
/// 中止（cancel 置 ABORT）：隐藏窗口、清门闩、不截图。
#[tauri::command(async)]
pub fn start_delayed_capture(app: AppHandle, secs: u64) -> Result<(), String> {
    let secs = validate_delay_secs(secs)?;
    // —— 互斥守卫（终审 wave Minor c）——
    // 延时截图到点后会走 start_capture_flow（截图流程）。旧实现**没有**前置守卫：
    // 录屏中 / 长截图会话中启动延时倒计时 → 倒计时归零时 start_capture_flow 被
    // 互斥守卫拒掉（截图/录制两套全屏 UI 不叠屏的规矩），但**用户看到的是**：
    // 倒计时正常跑完、什么都没发生（错误只在进程内 eprintln，用户零反馈）。
    // 现在入口直接拒绝并给出与其它入口同款文案（用户立刻知道为什么不能启动）。
    if crate::recording::recording_active() {
        return Err(super::flow::capture_guard_msg(true)
            .unwrap_or("录屏进行中，请先停止录屏")
            .to_string());
    }
    if crate::scrollshot::scrollshot_active() {
        return Err(super::flow::scrollshot_guard_msg().to_string());
    }
    // 先清 ABORT 再取门闩：若先取闩后清，「取闩到清之间」落下的 cancel 已把 ABORT 置
    // true 却被本轮复位覆盖——cancel 被静默抹掉，倒计时照跑。先清则该窗口内的 cancel
    // （门闩未取，cancel_delayed_capture 判非活动期返回 Err，良性）不会残留到本轮。
    DELAY_ABORT.store(false, Ordering::SeqCst);
    if !try_begin_delay(&DELAY_ACTIVE) {
        return Err("已有延时截图进行中".into());
    }
    // 建窗失败必须归还门闩：worker 尚未存在，无人会在到点/中止时清 DELAY_ACTIVE，
    // 漏还则此后所有延时启动永远撞「已有延时截图进行中」直至重启。
    if let Err(e) = show_countdown_window(&app) {
        DELAY_ACTIVE.store(false, Ordering::SeqCst);
        return Err(e);
    }
    DELAY_REMAINING.store(secs, Ordering::SeqCst);
    // 初始补发（overlay-ready 同款连发）：首建的 webview 冷加载期早于页面 listen，
    // t=0 的 emit 必丢；1 秒档尤其依赖补发——该档整秒变化只在 0 出现，丢了初始值整轮黑屏。
    // 同值重复发射对显示无副作用（隐藏窗口期间监听照常在线）。
    let app2 = app.clone();
    std::thread::spawn(move || {
        for _ in 0..2 {
            std::thread::sleep(Duration::from_millis(150));
            let _ = app2.emit("hawk://countdown", CountdownPayload { remaining: secs });
        }
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut emitted = secs;
    loop {
        if DELAY_ABORT.load(Ordering::SeqCst) {
            hide_countdown_window(&app);
            DELAY_ACTIVE.store(false, Ordering::SeqCst);
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = (deadline - now).as_secs_f64().ceil() as u64;
        if remaining != emitted {
            emitted = remaining;
            DELAY_REMAINING.store(remaining, Ordering::SeqCst);
            let _ = app.emit("hawk://countdown", CountdownPayload { remaining });
        }
        // 末段向 deadline 对齐（sleep = min(50ms, 距 deadline)）：触发点过冲从至多
        // 50ms 收紧到亚毫秒级；saturating 兜底已过 deadline 的极端分支（下一轮即 break）。
        let to_deadline = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(to_deadline.min(DELAY_POLL));
    }
    // 到点：{remaining: 0}（触发前一瞬间）→ 隐藏 → 清门闩 → 触发截图。清门闩先于
    // start_capture_flow：捕获流程自身不碰 DELAY 状态，先放行才不会因流程耗时卡住
    // 用户下一步延时入口。
    DELAY_REMAINING.store(0, Ordering::SeqCst);
    let _ = app.emit("hawk://countdown", CountdownPayload { remaining: 0 });
    hide_countdown_window(&app);
    DELAY_ACTIVE.store(false, Ordering::SeqCst);
    // 与托盘 shot 路径同口径：失败走 hawk://capture-error，不静默吞掉
    if let Err(e) = start_capture_flow(&app) {
        let _ = app.emit("hawk://capture-error", e);
    }
    Ok(())
}

/// 取消进行中的延时截图：仅原子置 ABORT，worker ≤50ms 内隐藏窗口并清门闩（隐藏/清门闩
/// 均由 worker 单点负责，本命令与其无共享可变状态）。非活动期调用返回 Err——托盘
/// 「取消倒计时」恒可用（不做动态 enable/disable），良性竞态由调用方 eprintln 吞掉。
#[tauri::command]
pub fn cancel_delayed_capture() -> Result<(), String> {
    if !DELAY_ACTIVE.load(Ordering::SeqCst) {
        return Err("没有进行中的延时截图".into());
    }
    DELAY_ABORT.store(true, Ordering::SeqCst);
    Ok(())
}

/// 倒计时剩余秒数 pull 通道（页面 mount 时补显，见 DELAY_REMAINING 注）：None = 无
/// 进行中的延时。与 push 事件互补，契约不变。
#[tauri::command]
pub fn countdown_remaining() -> Option<u64> {
    DELAY_ACTIVE
        .load(Ordering::SeqCst)
        .then(|| DELAY_REMAINING.load(Ordering::SeqCst))
}

// —— Plan B Task 8：延时截图纯逻辑测试 ——
#[cfg(test)]
mod delay_tests {
    use super::*;

    #[test]
    fn delay_secs_validated() {
        assert_eq!(validate_delay_secs(3), Ok(3));
        assert!(validate_delay_secs(4).is_err());
        assert!(validate_delay_secs(0).is_err());
    }

    #[test]
    fn delay_gate_is_exclusive() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        assert!(try_begin_delay(&flag));
        assert!(!try_begin_delay(&flag)); // 进行中拒绝
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(try_begin_delay(&flag));
    }
}
