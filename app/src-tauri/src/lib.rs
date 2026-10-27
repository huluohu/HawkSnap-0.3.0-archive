// M4 P0（跨平台编译重构）：非 macOS 平台的功能面按 `cfg(target_os = "macos")`
// 门控后，个别类型/函数/导入在桩构建下**无人使用**——这是「能力缺席」的正常
// 结果（录屏/画中画/贴图/按键显示等能力当前仅 macOS 提供），不是代码缺陷。
//
// 纪律（评审修复轮）：**不再用 crate 级 blanket allow**（`allow(dead_code,
// unused_variables, unused_imports)` 会把未来的真实告警一并吞掉）。改为**逐项
// 放行**——每一处因平台门控而无人使用的项，在其**定义处**加
// `#[cfg_attr(not(target_os = "macos"), allow(dead_code))]`（导入类加
// `unused_imports`），且放行范围精确到该项。
//
// macOS 构建的告警纪律零变化：所有 `cfg_attr` 均以 `not(target_os = "macos")`
// 为条件，macOS 下这些 `allow` 不生效——该报 dead_code / unused 依然照报。

mod bench_mode;
mod commands;
mod editor;
mod g4;
mod gif;
mod history;
mod idle_reaper;
mod keycast;
mod native_protocol;
pub mod native_messaging;
mod ocr;
mod overlay;
mod panel;
mod perf;
mod pins;
mod recording;
mod scrollshot;
mod settings;
mod single_instance;
mod updater;

use tauri::AppHandle;

/// 托盘菜单构建（M3 继承项⑦）：setup 与 set_settings 共用的**单一代码路径**。
/// 菜单项文案随 settings 动态——热键组合变更后菜单即时反映（如「截图 (F1)」→
/// 「截图 (F9)」）；恒定项（主面板/关闭全部贴图/延时子菜单/退出）不依赖设置。
/// 菜单项 **id 恒定**（panel/shot/pin-close-all/delay-*/quit）——on_menu_event 的
/// 分派挂在 TrayIcon 上、与菜单本体解耦，重建后同一处理器继续命中。
pub fn build_tray_menu(
    app: &AppHandle,
    settings: &settings::Settings,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, Submenu};
    // C6 主面板：启动零窗口后唯一的常驻面板入口（热键失败路径会自动拉起，
    // 但用户主动打开面板的常规路径在这里）。
    let panel = MenuItem::with_id(app, "panel", "主面板", true, None::<&str>)?;
    let shot_label = format!("截图 ({})", settings.hotkey_capture);
    let shot = MenuItem::with_id(app, "shot", shot_label, true, None::<&str>)?;
    // N5：长截图（滚动捕获 + 拼接）。无独立热键——托盘入口 + 覆盖层选区。
    let scrollshot = MenuItem::with_id(app, "scrollshot", "长截图", true, None::<&str>)?;
    // N5：停止长截图（I4：⌘S 注册失败 / 用户想提前结束时的托盘出口）。
    // 恒显示（与「取消倒计时」同款口径）：无会话时调用返回 Err → 横幅提示。
    let scrollshot_stop = MenuItem::with_id(app, "scrollshot-stop", "停止长截图", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 HawkSnap", true, None::<&str>)?;
    // C5 贴图：关闭全部贴图恒可用（无贴图时调用为空操作，不做动态
    // enable/disable——省去菜单项状态的跨线程同步，与延时子菜单同口径）。
    let pin_close_all =
        MenuItem::with_id(app, "pin-close-all", "关闭全部贴图", true, None::<&str>)?;
    // spec §3.1：延时截图 1/2/3/5 秒可选 + 「取消倒计时」恒可用（非活动期
    // 调用返回 Err，调用方 eprintln 吞掉——不做动态 enable/disable，省去
    // 菜单项状态的跨线程同步）。
    let delay_1 = MenuItem::with_id(app, "delay-1", "1 秒", true, None::<&str>)?;
    let delay_2 = MenuItem::with_id(app, "delay-2", "2 秒", true, None::<&str>)?;
    let delay_3 = MenuItem::with_id(app, "delay-3", "3 秒", true, None::<&str>)?;
    let delay_5 = MenuItem::with_id(app, "delay-5", "5 秒", true, None::<&str>)?;
    let delay_cancel =
        MenuItem::with_id(app, "delay-cancel", "取消倒计时", true, None::<&str>)?;
    let delay_menu = Submenu::with_id_and_items(
        app, "delay-submenu", "延时截图", true,
        &[&delay_1, &delay_2, &delay_3, &delay_5, &delay_cancel],
    )?;
    Menu::with_items(
        app,
        &[&panel, &shot, &scrollshot, &scrollshot_stop, &pin_close_all, &delay_menu, &quit],
    )
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // perf CSV 落盘闸（M3 继承项④）：仅 bench（HAWKSNAP_BENCH）/G4（HAWKSNAP_G4）
    // 模式写 CSV，生产运行零写入（0.1.0/0.2.0 的「CSV 恒写」是 M2 终审 ledgered
    // debt，本项收口）。必须先于 mark_process_start——process_start/tray_ready 是
    // G4 冷启动计时的两端，闸晚置会丢行。bench-latency.sh / bench-g4.sh 两个脚本
    // 消费的行只在本闸开启（对应模式）下存在，脚本口径不变、不做改动。
    crate::perf::set_enabled(crate::bench_mode::requested() || crate::g4::requested());
    // G4 冷启动 t0（plan D5）：run() 首行、先于 Builder 打点 process_start——
    // 进程级「冷启动 → 托盘就绪」计时的起点（CSV 落行 + 进程内缓存，perf.rs）。
    crate::perf::mark_process_start();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // 2.4.x 起插件不再提供自由函数 init()，改用 Builder::new().build()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // C7 开机自启：autostart 2.7.0 同样无自由函数 init()（计划所写
        // init(MacosLauncher::LaunchAgent, None) 为旧版签名），Builder.build() 适配；
        // macOS 用 LaunchAgent（登录项 plist，仅写本地）。macos_launcher 仅 macOS 编译，
        // 非 macOS 用 Builder 默认值（文档默认即 LaunchAgent，跨平台语义不变）。
        .plugin({
            let builder = tauri_plugin_autostart::Builder::new();
            #[cfg(target_os = "macos")]
            let builder = builder.macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent);
            builder.build()
        })
        // D3 自动更新：updater 2.13.x 的插件注册是纯状态管理（UpdaterState manage），
        // 不解析 pubkey——真实公钥已随 0.1.0 填入 tauri.conf.json（D7 = 8123f6c），
        // 无效/缺失公钥也不会让注册或启动失败（非致命，同 hotkey/历史库裁定）。通道
        // 未配置（占位/空钥）时的用户面行为由 updater.rs 的 channel_configured 短路
        // 兜底（检查返回「未配置」语义）。
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::capture_screens, commands::finish_capture, commands::annotate_crop,
            commands::hide_overlays_after_annotate,
            overlay::cancel_capture, overlay::emit_capture_done,
            overlay::hotkey_error, overlay::snap_candidates,
            overlay::start_delayed_capture, overlay::cancel_delayed_capture,
            overlay::countdown_remaining,
            settings::get_settings, settings::set_settings,
            history::history_list, history::history_search, history::history_delete,
            editor::editor_open, editor::editor_image_dataurl, editor::editor_save,
            pins::pin_latest, pins::pin_from_path, pins::pin_set_opacity, pins::pin_zoom,
            pins::pin_close, pins::pin_close_all, pins::pin_set_image,
            panel::panel_trigger, panel::open_path,
            ocr::ocr_recognize, ocr::ocr_export_txt,
            recording::record_start, recording::record_pause, recording::record_resume,
            recording::record_stop, recording::record_status, recording::record_start_region,
            recording::pip_update,
            scrollshot::scrollshot_start, scrollshot::scrollshot_stop, scrollshot::scrollshot_status,
            scrollshot::scrollshot_abort,
            keycast::keycast_set,
            updater::check_for_updates, updater::install_update, updater::restart_app,
            native_messaging::native_messaging_status, native_messaging::native_ingest
        ])
        .setup(|app| {
            // 单实例守卫（Task 13）：setup 第一步。锁文件在临时目录，flock 独占；
            // 守卫泄漏为 'static，锁覆盖进程全生命周期（进程退出由内核释放）。
            // 第二实例拿不到锁 → 提示并立即退出，不再初始化任何窗口/托盘。
            match single_instance::acquire_lock_at(&std::env::temp_dir().join("hawksnap.lock")) {
                Ok(guard) => std::mem::forget(guard),
                Err(_) => {
                    eprintln!("[hawksnap] 另一实例正在运行");
                    app.handle().exit(0);
                    return Ok(());
                }
            }

            // G3 基准模式（bench_mode）：单实例守卫之后第一时间接管。环境变量
            // HAWKSNAP_BENCH 缺席时零成本返回，正常启动路径不受任何影响。
            crate::bench_mode::maybe_run(app.handle());

            // Task 11 埋点：前端首帧画出冻结帧后回报 hawk://overlay-shown（无 payload）。
            // 多显示器时每窗口各发一次，mark_overlay_shown_once 只记每轮首个（评审 Finding 2a）。
            {
                use tauri::Listener;
                let _id = app.listen("hawk://overlay-shown", move |_event| {
                    crate::perf::mark_overlay_shown_once();
                });
            }

            // 历史库（C3）：旁路功能，init 失败仅记录不阻塞启动（裁定：不 `?` 退出）；
            // 此后各 history 命令自行开连接，失败以 Err 返回给前端。
            if let Err(e) = history::init(app.handle()) {
                eprintln!("[hawksnap] 历史库初始化失败: {e}");
            }

            // M0 历史网格：保存目录动态放行 asset 协议（缩略图 convertFileSrc 加载，
            // 见 commands::allow_asset_scope 注）。启动按当前 settings 放行一次；
            // save_dir 变更路径在 set_settings 成功后补放行。
            crate::commands::allow_asset_scope(app.handle());

            // M4 P1：Native Messaging 回环监听（浏览器插件 → 桌面端）。
            // 监听 127.0.0.1:<端口>，端口与一次性令牌写入 0600 宿主配置文件
            // （Chrome 拉起的 hawksnap-host 读它转发帧）。**失败不阻塞启动**
            // （旁路功能，裁定同历史库初始化）：插件链路不可用，其余功能照常。
            if let Err(e) = crate::native_messaging::start_listener(app.handle().clone()) {
                eprintln!("[hawksnap] Native Messaging 监听启动失败: {e}");
            }

            // 热键注册失败不中止启动（spec §9：允许无热键启动，但必须给用户可见反馈）。
            if let Err(err) = overlay::register_hotkeys(app.handle()) {
                eprintln!("[hawksnap] 热键注册失败: {err}");
                use tauri::Emitter;
                let _ = app.emit("hawk://hotkey-error", err);
                // spec §9 收口（C6）：启动零窗口后，此处的 emit 无窗可显（push 通道
                // 且前端尚未挂载）——立即拉起主面板让横幅有处可显。setup 运行在主线程
                // （register_hover_probe 的 AppKit 监听依赖此事实），而 webview 建窗
                // 禁止主线程（与 start_capture_flow 同一纪律，panel.rs 模块注）——
                // 故既不能直调也不能 run_on_main_thread，spawn 独立线程直呼。
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    if let Err(e) = panel::ensure_main_panel(&handle) {
                        eprintln!("[hawksnap] 热键失败反馈面板拉起失败: {e}");
                    }
                });
            }

            // 媒体键兼容（Task 13）：macOS 默认 F1=亮度键（fnState 未开启时物理 F1 不发
            // keycode 122），监听 systemDefined 的亮度下键走与 F1 相同的截图流程；
            // 监听不可用（未授予辅助功能）时降级并按需经 hotkey-error 通道发「提示：」，
            // 绝不阻塞启动。
            #[cfg(target_os = "macos")]
            if let Err(err) = overlay::register_media_key_f1(app.handle()) {
                eprintln!("[hawksnap] 媒体键监听注册失败: {err}");
            }

            // 智能吸附悬停探测（Plan B Task 5）：辅助功能未授予时整条链路缺席（不装监听、
            // 不弹窗，拖拽框选不受影响）；注册失败只 eprintln，绝不阻塞启动。
            #[cfg(target_os = "macos")]
            if let Err(err) = overlay::register_hover_probe(app.handle()) {
                eprintln!("[hawksnap] 悬停探测注册失败: {err}");
            }

            // 托盘常驻：截图入口 + 延时截图子菜单（Plan B Task 8）+ 退出。
            {
                use tauri::tray::TrayIconBuilder;
                // M3 继承项⑦：菜单构建抽 build_tray_menu（settings 驱动文案），
                // setup 与 set_settings 单一代码路径。热键注册失败路径同样用
                // settings 文案——settings 在热键注册前后都可加载（load 失败回退
                // Default），菜单展示的是「配置的组合」；注册成败另有 hotkey-error
                // 横幅反馈，不在菜单里表达。
                let settings = settings::Settings::load(app.handle());
                let menu = build_tray_menu(app.handle(), &settings)?;
                TrayIconBuilder::with_id("main")
                    .icon(
                        app.default_window_icon()
                            .expect("bundle icons missing: tauri.conf must provide a window icon")
                            .clone(),
                    )
                    .menu(&menu)
                    // on_menu_event 在主线程回调；start_capture_flow 建窗/关窗不能在主线程做，
                    // 必须丢到独立线程（其内部自行 block_on），保持本回调体最小。
                    .on_menu_event(|app, ev| match ev.id.as_ref() {
                        // C6 主面板：线程派发同 shot——on_menu_event 在主线程回调，而
                        // ensure_main_panel 建 wry webview 窗口禁主线程（与贴图原生
                        // NSWindow 的 run_on_main_thread 相反，见 panel.rs 模块注）。
                        "panel" => {
                            let app = app.clone();
                            std::thread::spawn(move || {
                                if let Err(e) = panel::ensure_main_panel(&app) {
                                    eprintln!("[hawksnap] 主面板打开失败: {e}");
                                }
                            });
                        }
                        "shot" => {
                            let app = app.clone();
                            std::thread::spawn(move || {
                                // bench 守卫（D0-③，Plan C 终审）：基准模式进行中忽略外部
                                // 触发（防一轮被污染）。与 F1/面板入口同款。
                                if crate::bench_mode::is_running() {
                                    return;
                                }
                                // 与 F1 热键路径同口径：失败走 hawk://capture-error，不静默吞掉。
                                // emit 前确保主面板存在（懒创建）：启动零窗口且用户未开面板时
                                // push 事件无窗可显——横幅会静默丢失（panel::show_capture_error）。
                                if let Err(e) = overlay::start_capture_flow(&app) {
                                    panel::show_capture_error(&app, e);
                                }
                            });
                        }
                        // N5 长截图入口：线程派发同 shot（建覆盖层禁主线程）；
                        // 失败走 capture-error 横幅（与 shot 同口径，不静默）。
                        "scrollshot" => {
                            let app = app.clone();
                            std::thread::spawn(move || {
                                if crate::bench_mode::is_running() {
                                    return;
                                }
                                if let Err(e) = overlay::start_scrollshot_selection_flow(&app) {
                                    panel::show_capture_error(&app, e);
                                }
                            });
                        }
                        // N5：停止长截图（托盘出口——⌘S 注册失败时的兜底；也是
                        // **强制停止**路径：必须能把卡住的会话清掉，否则僵尸会话
                        // 挡住后续截图/录屏）。
                        "scrollshot-stop" => {
                            let app = app.clone();
                            std::thread::spawn(move || {
                                if !scrollshot::scrollshot_active() {
                                    // **静默**（评审 Minor：托盘噪音）：没有进行中的
                                    // 长截图时点「停止长截图」是**无害的空操作**
                                    // → 不弹横幅（弹了只是噪音，用户什么都没做错）。
                                    eprintln!("[hawksnap] 托盘「停止长截图」：没有进行中的长截图（静默忽略）");
                                    return;
                                }
                                if let Err(e) = scrollshot::scrollshot_stop(app.clone()) {
                                    // 失败**已由 scrollshot_stop 自行投递**（提示条行内 +
                                    // 横幅）→ 这里不重复弹横幅；但**必须清掉会话**
                                    // （托盘语义 = 停止；失败也不留僵尸会话）。
                                    eprintln!("[hawksnap] 托盘停止长截图失败（强制收尾）: {e}");
                                    // **并发收尾中**（第一次还在跑）→ 不要强制清会话
                                    // （会把正在进行的收尾打断）。
                                    if !scrollshot::scrollshot_stopping() {
                                        scrollshot::scrollshot_force_stop(&app);
                                    }
                                }
                            });
                        }
                        // 延时入口：线程派发同 shot（start_delayed_capture 建窗禁主线程）；
                        // 失败（门闩占用/建窗失败）与 shot 同口径走 capture-error。
                        id @ ("delay-1" | "delay-2" | "delay-3" | "delay-5") => {
                            let secs = id.trim_start_matches("delay-").parse::<u64>().unwrap_or(0);
                            let app = app.clone();
                            std::thread::spawn(move || {
                                if let Err(e) = overlay::start_delayed_capture(app.clone(), secs) {
                                    let _ =
                                        tauri::Emitter::emit(&app, "hawk://capture-error", e);
                                }
                            });
                        }
                        "delay-cancel" => {
                            // 纯原子置位，主线程直调安全（不建窗/关窗）；非活动期为良性
                            // 竞态（裁定 5），eprintln 吞掉。
                            if let Err(e) = overlay::cancel_delayed_capture() {
                                eprintln!("[hawksnap] 取消倒计时: {e}");
                            }
                        }
                        // C5 贴图：关闭全部贴图。线程派发同 shot——on_menu_event 在主线程
                        // 回调，而 pin_close_all 内部经 run_on_main_thread 触碰窗口对象，
                        // 派发到独立线程保持回调体最小（与 shot 同一纪律）。
                        "pin-close-all" => {
                            let app = app.clone();
                            std::thread::spawn(move || {
                                if let Err(e) = pins::pin_close_all(app.clone()) {
                                    eprintln!("[hawksnap] 关闭全部贴图失败: {e}");
                                }
                            });
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    })
                    .build(app)?;
            }

            // G4（plan D5）：托盘就绪打点 + 测量模式接线。tray_ready 无条件打点
            // （正常启动零成本：一行 CSV 追加 + 一次原子写，打点失败静默）；打点
            // 失败一律 let _ 吞掉（perf.rs 纪律）。HAWKSNAP_G4=1 时 g4 自动流程
            // 在此接管：开面板 → 打点 → stderr 汇总 → 退出（见 g4.rs）。
            crate::perf::mark_tray_ready();
            crate::g4::maybe_run(app.handle());

            // Plan B Task 9 应急（G2）：空闲覆盖层 webview 看护线程。Plan B Task 1 的
            // 常驻复用让每个空闲 overlay-*/countdown webview 常驻 ~30MB，fresh-idle
            // 实测 100.7MB 吃穿 G2 预算（spec §5 ≤80MB）——隐藏满 5 分钟未复用即
            // close()，下次截图经 start_capture_flow 首建分支重建（一轮 webview 冷
            // 加载离群值，不计入基准 P90 口径）。看护自身吞错，绝不影响截图主链路。
            idle_reaper::spawn_watchdog(app.handle().clone());

            // M7（G2，idle_reaper 同款模式）：OCR 引擎空闲卸载看护。实测装载+首推理
            // 常驻 +349MB（release 365.9MB，scripts/sample-ocr-mem.sh 归档）——用后
            // 常驻会吃穿 G2 空载预算一个数量级；空闲满 10 分钟即 drop，下次
            // ocr_recognize 秒级重装。看护 try_lock 不阻塞推理，绝不影响 OCR 主链路。
            crate::ocr::spawn_engine_idle_unloader();

            // macOS 专属 API：引用点与 API 同门控，非 macOS 平台无激活策略概念。
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // 托盘常驻：最后一个窗口关闭（用户点红绿灯叉）只应关窗，不应退出整个应用。
    // ExitRequested 的 code=None 表示“最后一个窗口关闭”这类用户交互触发的退出请求，
    // 一律阻止；托盘“退出”走 app.exit(0)，以 code=Some(0) 到达，不匹配本分支，正常退出。
    // （ExitRequested 标注 #[non_exhaustive]，模式需带 `..`。）
    // C6 零窗口启动口径：code=None 仅由「最后一个窗口销毁」引发——启动期无窗口不会
    // 自发产生（托盘 + 事件循环独立于窗口存活）；用户关掉懒创建的主面板时恰由本分支
    // 兜住（进程存活，下次托盘「主面板」重建）。
    app.run(|_app, event| {
        if let tauri::RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
}
