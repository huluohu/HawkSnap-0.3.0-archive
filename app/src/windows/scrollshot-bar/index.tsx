import { useEffect, useRef, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t } from "../../lib/i18n";

// N5 长截图提示条（360×64 浮窗）。
//
// **可交互**（I4）：条上有「完成」按钮 → 用户始终有鼠标出口（⌘S 完成热键注册
// 失败时它是唯一出口）。Rust 侧不再设点击穿透——代价是条覆盖区域不再鼠标
// 穿透，故条被刻意定位在避开选区的上下边缘。
//
// 帧进度语义单源在 Rust：hawk://scrollshot-tick {session, frames, hotkey?} 每
// 抓一帧广播一次；本页 mount 时经 scrollshot_status 拉一次补显（push 早于
// listen 必丢，与 recording-bar / countdown 同款问题形态）。
//
// **失败必须看得见**（I4 残留，评审必修）：Rust 侧 `scrollshot_stop` 改成
// **先拼接、成功后才收尾**（关条 + 注销热键 + 消费会话）→ 拼接失败时提示条
// **仍然活着**，本行内错误槽才有地方渲染。两个错误来源都渲染到这里：
// ① invoke 失败（Rust 返回 Err）；② Rust 直发 `hawk://scrollshot-error`
// （不依赖 invoke 回流是否完好）。
//
// **「放弃」常驻**（终审 wave Minor d）：强制丢弃会话（Rust `scrollshot_abort`）
// 的出口**始终在条上**——不只是出错后才有。正常进行中用户同样可能反悔
//（「不要了」），那时条上必须有出口，而不是只能「完成」或等它自己停。
export default function ScrollshotBarWindow() {
  const [frames, setFrames] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  // ⌘S 完成热键是否可用（false → 「完成」按钮是唯一出口，条上明示）。
  const [hotkeyOk, setHotkeyOk] = useState(true);
  // **收尾在飞行中**（评审 Minor：并发「完成」）：点「完成」后 Rust 侧要跑
  // 秒级拼接 → 期间禁用按钮（连点会被 Rust 的单飞守卫挡下，但前端禁用能
  // 少一次无谓的失败提示）。
  const [finishing, setFinishing] = useState(false);
  // 上一 tick 的会话号（ref 而非 state：listen 回调闭包不随渲染更新）。
  const lastSessionRef = useRef<number | null>(null);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let unlistenErr: (() => void) | undefined;
    let disposed = false;
    // pull 补显 + push 订阅（窗口常驻复用：监听与页面同寿命）。
    void invoke<{ session: number; frames: number; hotkey?: boolean } | null>(
      "scrollshot_status",
    )
      .then((s) => {
        if (disposed || !s) return;
        lastSessionRef.current = s.session;
        setFrames(s.frames);
        // 热键状态**必须**走 pull 通道：会话启动时那次 push（带 hotkey）几乎
        // 必然早于本页 listen 就绪 → 必丢（push 早于 listen 是本仓老问题形态）。
        if (typeof s.hotkey === "boolean") setHotkeyOk(s.hotkey);
      })
      .catch(() => {});
    // listen 的 promise 拒绝 = 该窗口标签没有对应 capability（ACL 按标签授权）
    // ——必须 catch，否则变成 unhandled rejection（白屏 + 控制台噪音）。
    listen<{ session: number; frames: number; hotkey?: boolean }>(
      "hawk://scrollshot-tick",
      (e) => {
        if (lastSessionRef.current !== e.payload.session) {
          lastSessionRef.current = e.payload.session;
          setError(null); // 新会话 → 清上一会话的残留错误
        }
        setFrames(e.payload.frames);
        if (typeof e.payload.hotkey === "boolean") setHotkeyOk(e.payload.hotkey);
      },
    )
      .then((un) => {
        if (disposed) un();
        else unlisten = un;
      })
      .catch((e) => {
        console.error(
          "[scrollshot-bar] listen(hawk://scrollshot-tick) 失败（窗口 capability 缺失？）",
          e,
        );
      });
    // Rust 直发的失败事件（拼接失败）→ 行内错误槽。**不依赖** invoke 回流：
    // 即使 invoke 通道异常，用户也能看到失败原因（提示条此时不会被关闭）。
    listen<{ session: number; message: string }>("hawk://scrollshot-error", (e) => {
      // 只认当前会话的错误（旧会话的残留不该出现在新会话的条上）。
      if (
        lastSessionRef.current !== null &&
        lastSessionRef.current !== e.payload.session
      ) {
        return;
      }
      lastSessionRef.current = e.payload.session;
      setError(e.payload.message);
    })
      .then((un) => {
        if (disposed) un();
        else unlistenErr = un;
      })
      .catch((e) => {
        console.error(
          "[scrollshot-bar] listen(hawk://scrollshot-error) 失败（窗口 capability 缺失？）",
          e,
        );
      });
    return () => {
      disposed = true;
      unlisten?.();
      unlistenErr?.();
    };
  }, []);

  // 完成：invoke scrollshot_stop（Rust 侧拼接 + 落盘 + 入库）。失败 → 行内错误。
  // **条不会因此消失**：Rust 改成「先拼接、成功后才关条」→ 失败时提示条仍活着，
  // 错误渲染在行内（用户可重试，或点「放弃」丢弃）。
  const finish = () => {
    if (finishing) return; // 收尾在飞行中 → 忽略重复点击
    setFinishing(true);
    void invoke<string>("scrollshot_stop")
      .catch((e) => {
        setError(String(e));
        setFinishing(false); // 失败 → 恢复可点（用户可重试或「放弃」）
      });
  };

  // 放弃：强制丢弃会话（Rust 清槽位 + 停采集 + 注销热键 + 关条）。
  const abort = () => {
    void invoke<void>("scrollshot_abort").catch((e) => setError(String(e)));
  };

  const bar: CSSProperties = {
    position: "fixed", inset: 0, display: "flex", alignItems: "center",
    gap: 10, padding: "0 12px", boxSizing: "border-box", background: "#0D1522",
    fontFamily: "system-ui, sans-serif", userSelect: "none", cursor: "default",
  };
  const dot: CSSProperties = {
    width: 10, height: 10, borderRadius: "50%", background: "#FFB020", flexShrink: 0,
    animation: "hawksnap-ss-pulse 1.2s ease-in-out infinite",
  };
  const hint: CSSProperties = {
    fontSize: 13, lineHeight: 1, color: "#F5F7FA", flex: 1, minWidth: 0,
    whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis",
  };
  const count: CSSProperties = {
    fontSize: 12, lineHeight: 1, color: "#8A8F98", flexShrink: 0,
    fontVariantNumeric: "tabular-nums",
  };
  const err: CSSProperties = {
    fontSize: 10, color: "#E5484D", maxWidth: 110, overflow: "hidden",
    whiteSpace: "nowrap", flexShrink: 1,
    // 评审 Minor：加了 nowrap + overflow 却**没有** textOverflow → 长文案被
    // **硬切**（半个字），读不出来。ellipsis → 至少是「看得出的省略」。
    textOverflow: "ellipsis",
  };
  const btn: CSSProperties = {
    fontSize: 12, padding: "5px 10px", borderRadius: 6, border: "1px solid #2A3648",
    background: "#16202E", color: "#E8ECF1", cursor: "pointer", flexShrink: 0,
  };
  const btnDisabled: CSSProperties = { ...btn, opacity: 0.5, cursor: "default" };
  const btnDanger: CSSProperties = { ...btn, background: "#3A1620", borderColor: "#5B2430" };

  // ⌘S 不可用 → 提示语改口（否则用户会一直等一个永远不来的热键）。
  const hintText = hotkeyOk
    ? t("scrollshot.bar.hint")
    : t("scrollshot.bar.hintNoHotkey");

  return (
    <div style={bar} role="status" aria-label={hintText}>
      <style>{`@keyframes hawksnap-ss-pulse { 50% { opacity: 0.35; } }`}</style>
      <span style={dot} aria-hidden="true" />
      <span style={hint}>{hintText}</span>
      {error && <span style={err} title={error}>{error}</span>}
      <span style={count}>{t("scrollshot.bar.frames", { n: frames ?? 0 })}</span>
      {/* 完成按钮：⌘S 不可用时它是唯一出口 → 高亮提示；收尾在飞行中 → 禁用 */}
      <button
        style={finishing ? btnDisabled : hotkeyOk ? btn : btnDanger}
        onClick={finish}
        disabled={finishing}
        title={hotkeyOk ? t("scrollshot.bar.finish") : t("scrollshot.bar.finishOnly")}
      >
        {t("scrollshot.bar.finish")}
      </button>
      {/* 「放弃」**常驻**（终审 wave Minor d）：旧实现只在出错后才渲染它——
          正常进行中的会话没有任何「不要了」的出口（用户只能完成或等它自己停）。
          条上的放弃 = 强制丢弃会话（Rust scrollshot_abort），常驻才符合「用户
          随时能反悔」。失败后它同样在（同一按钮，语义不变）。 */}
      <button style={btnDanger} onClick={abort} title={t("scrollshot.bar.abort")}>
        {t("scrollshot.bar.abort")}
      </button>
    </div>
  );
}
