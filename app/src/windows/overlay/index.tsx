import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { listen } from "@tauri-apps/api/event";
import {
  t,
  setLanguage,
  getLanguage,
  subscribe,
  type LanguageSetting,
} from "../../lib/i18n";
import { useSelection, type SelRect, type ConfirmAction } from "./useSelection";
import type { Rect } from "./geometry";
import { toLocalHover, type HoverFrame } from "./hover";
import { barPosition } from "./actionBar";
import { hexOf, rgbOf, rulerTicks, samplePixel, type Rgb } from "./picker";

type Display = { displayId: number; x: number; y: number; width: number; height: number; scale: number; frameUrl: string; flow: number; intent: string };

// window.__LABEL__ 由 Rust 侧建窗后 eval 注入（overlay.rs），用于认领自己的 DisplayPayload
declare global {
  interface Window {
    __LABEL__?: string;
  }
}

// 冻结帧（原始物理 RGBA）加载：fetch 为主通道；若 WKWebView 对 asset 协议 URL 的 fetch
// 出现异常，降级 XHR responseType=arraybuffer（两者能力的最低公因数）。字节长度校验
// 由调用方按 w*h*4 断言，两通道共用同一防线。
async function fetchRaw(url: string): Promise<ArrayBuffer> {
  try {
    const resp = await fetch(url);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    return await resp.arrayBuffer();
  } catch (err) {
    console.warn("fetch raw frame failed, falling back to XHR", err);
    return await new Promise<ArrayBuffer>((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      xhr.open("GET", url);
      xhr.responseType = "arraybuffer";
      xhr.onload = () => {
        // 自定义 scheme 下 status 可能为 0；字节长度校验兜底，故 0 且有响应也算成功
        if ((xhr.status >= 200 && xhr.status < 300) || xhr.status === 0) {
          resolve(xhr.response as ArrayBuffer);
        } else {
          reject(new Error(`XHR HTTP ${xhr.status}`));
        }
      };
      xhr.onerror = () => reject(new Error("XHR network error"));
      xhr.send();
    });
  }
}

export default function OverlayWindow() {
  // 语言（D1 + 评审修正）：订阅 i18n 语言态——面板保存新语言后，此处的 setLanguage
  // 触发本组件重渲染，下一轮捕获的动作浮条文案即新语言（覆盖层常驻复用、无挂载时机）。
  useSyncExternalStore(subscribe, getLanguage);
  const [display, setDisplay] = useState<Display | null>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  // 覆盖层窗口常驻复用（Plan B Task 1）：页面生命周期跨越多轮捕获，此标志每轮 payload
  // 到达时复位，保证 hawk://overlay-shown 每轮捕获重发一次（bench 门闩按轮与 hotkey 配对）。
  const shownRef = useRef(false);
  // 捕获轮次（代际守卫）：每轮 payload 到达即自增。confirm 链（finish_crop 数百 ms）在途时
  // 用户新触发一轮会复用本窗口，旧链的迟到延续若不拦截，会用 emit_capture_done 把新一轮
  // 刚 show 的窗口藏掉（用户看到窗口闪现即消失），其失败也会误报在新一轮上。
  const flowRef = useRef(0);
  // 已见最大 payload 代数（终审 wave）：Rust 每轮流程分配单调递增的 flow 写入 payload，
  // 300ms 补发线程可能在新一轮已启动后才发射旧代 payload——旧代迟到补发若放行，会用
  // raw 文件已被新一轮删除的陈旧 payload 重绘（画布冻结）并 reset() 杀掉在途拖拽。
  // payload.flow < maxFlow 直接丢弃；等值（同代补发）放行（首发射丢时的补显仍依赖它）。
  const maxFlowRef = useRef(0);
  // Task 4 磁吸候选（display 本地坐标）：每轮 payload 到达时经 snap_candidates 拉取一次
  // （窗口在两轮之间会变化，必须逐轮刷新），首帧置空避免用到上一轮的陈旧候选。
  const candidatesRef = useRef<Rect[]>([]);
  // Task 6 hover 高亮（display 本地坐标）：hawk://hover 帧经 toLocalHover 换算后的
  // 当前悬停框。state 而非 ref——重绘 effect 依赖它触发；selected 态仍更新但不绘制
  // （绘制按 phase 门控，见重绘 effect）。
  const [hoverRect, setHoverRect] = useState<Rect | null>(null);

  // —— N1 录制选区模式 ——
  // payload.intent === "record"（F6 触发的流程）：选区 UI 与截图完全同款（同冻结帧/
  // 十字准线/遮罩/磁吸/hover 直选），差异只有四件事——提示横幅文案不同、动作浮条
  // 缺席、无选区时 Enter/双击 = 整屏快捷确认、确认路由到 record_start_region。
  // 录制启动失败的行内错误（覆盖层不是横幅宿主、主面板可能不存在——错误就地可见，
  // 覆盖层保留可重试/Esc 退出，Rust 侧失败路径不 hide）。
  const recordMode = display?.intent === "record";
  // N5：长截图选区模式（托盘「长截图」进入）。选区 UI 与截图/录制完全同款，差异：
  // 提示横幅文案、确认路由到 scrollshot_start（进入滚动采集会话）、动作浮条缺席。
  const scrollshotMode = display?.intent === "scrollshot";
  const [recordError, setRecordError] = useState<string | null>(null);
  // N2 输出格式（录制横幅分段开关，默认 MP4）。**每轮选区流程复位**（终审 review
  // wave I3）：覆盖层窗口常驻复用 → 该 state 跨轮存活；旧实现把它留到下一轮，
  // 导致「上一轮选过的格式」无声地套用到本次录制——尤其在长截图轮里用户**根本
  // 看不到**这个开关（该轮只渲染长截图自己的提示），它的值却决定了下一条录像是
  // MP4 还是 GIF。现在每轮 payload 到达即复位为默认 MP4 → 格式永远是**本轮显式**
  // 的选择，不可能泄漏到用户没看见它的轮次。
  const [recordFormat, setRecordFormat] = useState<"mp4" | "gif">("mp4");

  // —— M2 Task M5 取色器（Snipaste 式）——
  // C 进入/退出拾色模式；移动 = 浮签跟随光标实时显示光标下像素（冻结帧缓冲直采，
  // 零 IPC）；点击 = 复制 HEX + 退出拾色 + 「已复制」闪样；Esc 仅退出拾色
  // （useSelection 的 keysSuppressed 挡住 hook 内的 Esc-cancel/Enter-confirm，
  // 否则第一次 Esc 会连截图一起取消）。
  const [pickerMode, setPickerMode] = useState(false);
  const [copied, setCopied] = useState<string | null>(null);
  const copiedTimerRef = useRef<number | null>(null);
  // 冻结帧原始字节（重绘 effect 校验字节长后缓存）：拾色采样的同步数据源。
  // 新一轮 payload 开始装载时先置空（陈旧帧不可采），画成功即回填。
  const frameBufRef = useRef<ArrayBuffer | null>(null);
  // 浮签 DOM refs：位置/内容由 pointermove 直写（同十字准线的零 state 模式，
  // 60fps 移动不触发 React 重渲染、更不触发 canvas 重绘）。
  const pickerChipRef = useRef<HTMLDivElement>(null);
  const pickerSwatchRef = useRef<HTMLDivElement>(null);
  const pickerHexRef = useRef<HTMLSpanElement>(null);
  const pickerRgbRef = useRef<HTMLSpanElement>(null);
  // 拾色中的按下起点：区分「按下发生在拾色内」（up = 拾取）与「拖拽中按 C 后松开」
  // （up 放行原链收尾拖拽态，不拾取）。
  const pickerDownRef = useRef<{ x: number; y: number } | null>(null);

  // Snipaste 式十字准线（0.1 hotfix 3）：透明无边框 WKWebView 上 CSS cursor:crosshair
  // 不可靠（macOS 不保证合成光标生效），改为自绘两条 1px 线随指针移动。零 React state、
  // 零 canvas 重绘：window 级 pointermove 直写 DOM ref 的 style.transform（合成器级
  // 位移，无 layout）。初始化位移 -100px（屏外），首次移动前不显示任何线。
  const crossHRef = useRef<HTMLDivElement>(null);
  const crossVRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onMove = (e: PointerEvent) => {
      if (crossHRef.current) crossHRef.current.style.transform = `translateY(${e.clientY}px)`;
      if (crossVRef.current) crossVRef.current.style.transform = `translateX(${e.clientX}px)`;
    };
    window.addEventListener("pointermove", onMove);
    return () => window.removeEventListener("pointermove", onMove);
  }, []);

  // —— M2 Task M5 取色器：键盘进出 + 浮签跟随 ——

  // C 进入/退出拾色（预确认全程可用：idle/dragging/selected 均为合法时机）；
  // Esc 仅退出拾色态（hook 侧 keysSuppressed 已挡住其 cancel/confirm，这里只管
  // 退出）。修饰键组合（⌘C 等）不劫持；e.repeat 忽略长按连发。
  useEffect(() => {
    if (!display) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && pickerMode) {
        pickerDownRef.current = null;
        setPickerMode(false);
        return;
      }
      if ((e.key === "c" || e.key === "C")
        && !e.metaKey && !e.ctrlKey && !e.altKey && !e.repeat) {
        setPickerMode((m) => !m);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [display, pickerMode]);

  // 浮签跟随（零 React state：位置/颜色直写 DOM，同十字准线模式——60fps 移动既
  // 不触发重渲染也不触发 canvas 重绘）。位置 = 光标 16px 右下，贴近视口右/下缘
  // 翻转到左/上；颜色 = 光标处物理像素（冻结帧缓冲直采，零 IPC）。帧缓冲未就绪
  // （装载中/失败）时只动位置不更新颜色，绝不展示脏色。
  const PICKER_CHIP_W = 178;
  const PICKER_CHIP_H = 46;
  const PICKER_GAP = 16;
  useEffect(() => {
    if (!pickerMode || !display) return;
    const pw = Math.round(display.width * display.scale);
    const onMove = (e: PointerEvent) => {
      const chip = pickerChipRef.current;
      if (!chip) return;
      const flipX = e.clientX + PICKER_GAP + PICKER_CHIP_W > window.innerWidth;
      const flipY = e.clientY + PICKER_GAP + PICKER_CHIP_H > window.innerHeight;
      const left = flipX ? e.clientX - PICKER_GAP - PICKER_CHIP_W : e.clientX + PICKER_GAP;
      const top = flipY ? e.clientY - PICKER_GAP - PICKER_CHIP_H : e.clientY + PICKER_GAP;
      chip.style.transform = `translate(${left}px, ${top}px)`;
      const buf = frameBufRef.current;
      if (!buf) return;
      const rgba = samplePixel(buf, pw, e.clientX * display.scale, e.clientY * display.scale);
      if (!rgba) return;
      const rgb: Rgb = [rgba[0], rgba[1], rgba[2]];
      if (pickerSwatchRef.current) pickerSwatchRef.current.style.background = hexOf(rgb);
      if (pickerHexRef.current) pickerHexRef.current.textContent = hexOf(rgb);
      if (pickerRgbRef.current) pickerRgbRef.current.textContent = rgbOf(rgb);
    };
    window.addEventListener("pointermove", onMove);
    return () => window.removeEventListener("pointermove", onMove);
  }, [pickerMode, display]);

  // 闪样计时器清理（同 editor 的 ocrFlash 模式）：卸载不悬挂。
  useEffect(() => () => {
    if (copiedTimerRef.current !== null) window.clearTimeout(copiedTimerRef.current);
  }, []);

  // cancel 的职责由 hook 内部的 Esc keydown 监听承担（会回调 onCancel），页面无需直接引用。
  // 声明须先于下方 listen effect：其依赖数组在渲染期取 reset（TDZ）。
  const { phase, rect, pointerHandlers, confirm, reset, selectRect, unlockConfirm } = useSelection({
    width: display?.width ?? 0, height: display?.height ?? 0,
    // bounds = 本 display 的逻辑边界（选区坐标是 display 本地系，故 x/y 恒为 0——
    // payload 的 display.x/y 是全局多屏坐标，直接用会破坏副屏上的夹取）
    bounds: display ? { x: 0, y: 0, w: display.width, h: display.height } : undefined,
    candidatesRef,
    // M2 M5：拾色中 Esc/Enter 归拾色器（见 pickerMode 声明处注）
    keysSuppressed: pickerMode,
    // N1：录制模式无选区 Enter/双击 = 整屏快捷确认（rect = 本 display 全边界，
    // Rust 侧按整屏 sourceRect 录制）。截图模式不传——无选区不确认（原语义）。
    emptyConfirmRect: recordMode && display
      ? { x: 0, y: 0, w: display.width, h: display.height }
      : undefined,
    onConfirm: async (r: SelRect, action: ConfirmAction) => {
      if (!display) return;
      const myFlow = flowRef.current;
      // N1：录制模式确认路由——record_start_region（成功由 Rust 收口：hide 全部
      // 覆盖层 + 清意图槽 + 拉起录制条；失败覆盖层保留，行内错误 + 解锁在途守卫）。
      // N5：长截图确认路由——scrollshot_start（成功由 Rust 收口：隐藏全部覆盖层
      // + 拉起长截图提示条 + 注册 ⌘S 完成热键；失败覆盖层保留，行内错误 + 解锁
      // 在途守卫）。
      if (display.intent === "scrollshot") {
        try {
          await invoke("scrollshot_start", {
            req: { displayId: display.displayId, rect: r },
          });
          if (flowRef.current !== myFlow) return;
        } catch (err) {
          if (flowRef.current !== myFlow) return;
          console.error("scrollshot_start failed", err);
          setRecordError(String(err));
          unlockConfirm();
        }
        return;
      }
      if (display.intent === "record") {
        try {
          await invoke("record_start_region", {
            req: { displayId: display.displayId, rect: r, format: recordFormat },
          });
          if (flowRef.current !== myFlow) return;
        } catch (err) {
          if (flowRef.current !== myFlow) return;
          console.error("record_start_region failed", err);
          setRecordError(String(err));
          unlockConfirm();
        }
        return;
      }
      try {
        const outcome = await invoke<{ copied: boolean; savedTo: string | null }>("finish_capture", {
          req: { displayId: display.displayId, rect: r, copy: action === "copy", save: action === "save" },
        });
        // 代际守卫：finish_capture 在途期间新一轮 payload 已到（flowRef 已自增）→ 本链
        // 已被作废，绝不 emit_capture_done（否则 hide 掉新一轮刚显示的窗口）。
        if (flowRef.current !== myFlow) return;
        // 发 capture-done 并隐藏本窗口（Rust 侧按 displayId 隐藏，见 Task 9 裁定 2；
        // Plan B Task 1 起常驻复用：隐藏而非关闭，窗口由 start_capture_flow 复用）
        await invoke("emit_capture_done", { displayId: display.displayId, outcome });
      } catch (err) {
        // 旧轮链的失败静默吞掉：横幅是全局事件，迟到的失败会误导新一轮的用户
        if (flowRef.current !== myFlow) return;
        // spec §9：失败不允许静默。覆盖层窗口不是横幅宿主，经全局事件把错误送到
        // 主窗口的 capture-error 横幅（ACL core:default 含 event emit）。
        console.error("finish_capture failed", err);
        void emit("hawk://capture-error", String(err));
        // 失败解锁在途守卫（0.1 hotfix 2）：否则 confirm 的 in-flight 标志只有
        // cancel/reset 能清，一次 IPC 失败就砖死本轮所有重试（用户被迫 Esc 重选）。
        unlockConfirm();
      }
    },
    onCancel: () => {
      // display 未到（overlay-ready 三连发全丢）时，从建窗注入的 __LABEL__（格式
      // overlay-{id}）同步解析 displayId，保证 Esc 始终能隐藏窗口，不留僵尸覆盖层。
      const labelId = window.__LABEL__?.match(/^overlay-(\d+)$/)?.[1];
      const id = display?.displayId ?? Number(labelId);
      if (!Number.isFinite(id)) return; // 无 display 且 label 缺失，无从取消
      void invoke("cancel_capture", { displayId: id });
    },
  });

  // M2 Task M3「标注」：当前选区直落盘临时 PNG → 编辑器（origin=Capture——保存时才
  // 复制剪贴板 + 存 save_dir 入库，契约在 Rust editor.rs「来源契约」）。
  // 评审 Finding 1：hide 从命令侧拆出——annotate_crop 只裁剪+落盘+开编辑器，成功返回
  // 且本轮未被新流程作废（flowRef 守卫，与 emit_capture_done 同款、权威在前端）后才
  // invoke hide_overlays_after_annotate；否则旧链迟到的 hide 会藏掉新一轮刚 show 的
  // 覆盖层并 release 新一轮 CAPTURES（冷编辑器建窗 ~100-600ms 的窗口期）。
  // 评审 Minor 2：在途守卫（confirm 同款 ref 模式）——标注在途时拒绝第二次 invoke。
  // 失败路径：capture-error 横幅，覆盖层保持可重试（不 hide）。
  const annotateInFlightRef = useRef(false);
  const annotate = async () => {
    if (!display || !rect || annotateInFlightRef.current) return;
    annotateInFlightRef.current = true;
    const myFlow = flowRef.current;
    try {
      await invoke("annotate_crop", {
        req: { displayId: display.displayId, rect, copy: false, save: false },
      });
      if (flowRef.current !== myFlow) return; // 已被新一轮作废：hide 权归新流程
      await invoke("hide_overlays_after_annotate");
    } catch (err) {
      if (flowRef.current !== myFlow) return;
      console.error("annotate_crop failed", err);
      void emit("hawk://capture-error", String(err));
    } finally {
      annotateInFlightRef.current = false;
    }
  };

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen<Display[]>("hawk://overlay-ready", (e) => {
      const mine = e.payload.find((d) => `overlay-${d.displayId}` === window.__LABEL__);
      if (mine) {
        // 语言逐轮对齐（评审修正）：覆盖层常驻复用，挂载期同步会被面板后续的语言切换
        // 冻结在旧值——每轮 payload 到达时拉一次 settings（µs 级小 IPC）重设 i18n 态；
        // 与 setDisplay 的先后序无关，谁后到谁触发订阅重渲染，浮条文案必为新语言。
        void invoke<{ language: string }>("get_settings")
          .then((s) => setLanguage(s.language as LanguageSetting))
          .catch(() => {});
        // 代数守卫（终审 wave）：旧代迟到 payload 直接丢弃（见 maxFlowRef 注），
        // 绝不 setDisplay / reset。等值放行：同代的三连发补显仍生效。
        if (mine.flow < maxFlowRef.current) return;
        maxFlowRef.current = mine.flow;
        setDisplay(mine);          // 新对象身份必触发重绘 effect
        reset();                   // 新一轮捕获：清选区与 in-flight 守卫
        setHoverRect(null);        // 新一轮：清上一轮的悬停框（否则 idle 态会画陈旧高亮）
        // 新一轮：清上一轮的拾色态与闪样（覆盖层常驻复用，state 跨轮存活）
        setPickerMode(false);
        pickerDownRef.current = null;
        setCopied(null);
        // N1：清上一轮的录制启动错误（新一轮 payload 到达 = 覆盖层复活，横幅回到提示态）
        setRecordError(null);
        // N2 输出格式复位为默认 MP4（终审 review wave I3）：格式是**本轮显式**的选择，
        // 上一轮的值不得泄漏到本轮（长截图轮更是完全看不到这个开关）。
        setRecordFormat("mp4");
        if (copiedTimerRef.current !== null) {
          window.clearTimeout(copiedTimerRef.current);
          copiedTimerRef.current = null;
        }
        flowRef.current += 1;      // 代际自增：作废所有在途旧 confirm 链（见 flowRef 注）
        shownRef.current = false;  // 每轮捕获重发 overlay-shown（见上方 shownRef 注）
        // Task 4：每轮流程拉取一次窗口候选（snap_candidates 返回全局逻辑坐标）。
        // 先置空再异步填充；迟到响应（flow 已推进）不得覆盖新一轮的候选。
        candidatesRef.current = [];
        const flow = flowRef.current;
        void invoke<Rect[]>("snap_candidates", { displayId: mine.displayId })
          .then((rects) => {
            if (flowRef.current !== flow) return;
            // 按 display 边界做交集过滤（Rust 侧返回全屏窗口，只有前端知道自身 display），
            // 再换算为 display 本地坐标（与选区/磁吸同一坐标系）；display 边界本身
            // 首位加入，屏幕四边即可磁吸（geometry.test.ts 的语义）。
            candidatesRef.current = [
              { x: 0, y: 0, w: mine.width, h: mine.height },
              ...rects
                .filter((r) => r.x < mine.x + mine.width && r.x + r.w > mine.x
                  && r.y < mine.y + mine.height && r.y + r.h > mine.y)
                .map((r) => ({ x: r.x - mine.x, y: r.y - mine.y, w: r.w, h: r.h })),
            ];
          })
          .catch(() => {
            if (flowRef.current !== flow) return;
            candidatesRef.current = []; // 拉取失败静默降级：仍可 nudge，只是不磁吸
          });
      }
    }).then((un) => {
      if (disposed) un();
      else unlisten = un;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [reset]);

  // Task 6：hawk://hover 监听（与 overlay-ready 同款 disposed-flag 挂/卸模式）。
  // payload = AxFrameJson | null（全局逻辑坐标；null = 光标下无元素且窗口回退未命中，
  // 清除高亮）→ toLocalHover 换算 display 本地并丢弃越界帧 → hoverRect。
  // 依赖 display：换算需要本 display 的全局原点与尺寸；display 每轮换新对象即重挂，
  // 窗口常驻复用下每轮至多重挂一次，成本可忽略。selected 态照常更新 state（保持最新
  // 悬停框）但绘制按 phase 门控——裁定 4 的「keep last hoverRect but don't draw」。
  useEffect(() => {
    if (!display) return;
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen<HoverFrame | null>("hawk://hover", (e) => {
      const next = toLocalHover(e.payload, display);
      // 值相等则保持原引用：Rust 每 16ms 发一帧（同一元素悬停期间坐标不变），
      // 引用去重让 React 直接 bail out，避免同帧全画布重绘以 60fps 空转。
      setHoverRect((prev) => (prev && next
        && prev.x === next.x && prev.y === next.y
        && prev.w === next.w && prev.h === next.h ? prev : next));
    }).then((un) => {
      if (disposed) un();
      else unlisten = un;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [display]);

  // —— Task 6 click-to-select：pointerHandlers 的外层包装 ——
  // press→release 位移双轴均 < 4px 判为 click（裁定 2）；只有 pointerup 才分支
  // click-vs-drag，down 语义不变（不清 rect，保留 dblclick 确认链）：
  //   click + 有悬停框 → selectRect 直选（整体替换 rect，动作浮条出现）；
  //   click + 无悬停框 + 无选区 → reset()（真 no-op 回 idle；若放行原 handler 会进入
  //     selected 空选区态，而 selected 抑制 hover 高亮——一次落空点击就废掉后续悬停）；
  //   click + 无悬停框 + 有旧选区 → 放行原 handler（plan A 语义：维持原选区 = no-op）；
  //   其余（拖拽）→ 原样放行。
  const downPointRef = useRef<{ x: number; y: number } | null>(null);
  // 参数类型与 hook 内部同款最小结构（React.PointerEvent 结构满足之；用 DOM PointerEvent
  // 会因 TS 严格函数逆变被 JSX props 拒绝——React 合成事件并非 DOM PointerEvent）。
  const clickAwareHandlers = {
    ...pointerHandlers,
    onPointerDown: (e: { clientX: number; clientY: number }) => {
      downPointRef.current = { x: e.clientX, y: e.clientY };
      pointerHandlers.onPointerDown(e);
    },
    onPointerUp: (e: { clientX: number; clientY: number }) => {
      const down = downPointRef.current;
      downPointRef.current = null;
      if (down
        && Math.abs(e.clientX - down.x) < 4
        && Math.abs(e.clientY - down.y) < 4) {
        if (hoverRect) {
          selectRect(hoverRect);
          return;
        }
        if (!rect) {
          reset();
          return;
        }
      }
      pointerHandlers.onPointerUp(e);
    },
  };

  // —— M2 Task M5：拾取点击 + 复制 ——

  // 剪贴板写入（与 editor/index.tsx 同款降级策略）：navigator.clipboard 优先
  // （拾取点击的手势内调用），失败降级 execCommand。裁定不走 Rust arboard：
  // 不为纯前端值新增 IPC 命令与 capability 变更——editor 的 OCR/保存复制已在
  // 同一机制上交付，行为一致且采样/复制全链保持前端闭环。
  const copyText = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
  };

  // 拾取：退出拾色 → 光标处采样 → 复制 HEX → 「已复制」闪样 1.4s（同 editor 闪样
  // 模式：state + 定时器）。帧缓冲不可用仅退出（不闪假成功）。flow 守卫：复制在途
  // 时新一轮 payload 已到（窗口可能已隐藏重用），迟到的闪样不得写进新一轮。
  const pickAt = (cx: number, cy: number) => {
    setPickerMode(false);
    if (!display) return;
    const buf = frameBufRef.current;
    if (!buf) return;
    const pw = Math.round(display.width * display.scale);
    const rgba = samplePixel(buf, pw, cx * display.scale, cy * display.scale);
    if (!rgba) return;
    const hex = hexOf([rgba[0], rgba[1], rgba[2]]);
    const myFlow = flowRef.current;
    void copyText(hex).then(() => {
      if (flowRef.current !== myFlow) return;
      setCopied(hex);
      if (copiedTimerRef.current !== null) window.clearTimeout(copiedTimerRef.current);
      copiedTimerRef.current = window.setTimeout(() => setCopied(null), 1400);
    });
  };

  // 拾色点击接管（包在 clickAwareHandlers 外）：down/up 都不放行选区状态机——拾色
  // 期间选区保持原样。up 且按下发生在拾色内 → pickAt；按下早于拾色（拖拽中按 C 后
  // 松开）→ 放行原 up 收尾拖拽态（那次按压不是拾取手势）。
  const pickerAwareHandlers = {
    ...clickAwareHandlers,
    onPointerDown: (e: { clientX: number; clientY: number }) => {
      if (pickerMode) {
        pickerDownRef.current = { x: e.clientX, y: e.clientY };
        return;
      }
      clickAwareHandlers.onPointerDown(e);
    },
    onPointerUp: (e: { clientX: number; clientY: number }) => {
      if (pickerMode) {
        const down = pickerDownRef.current;
        pickerDownRef.current = null;
        if (!down) {
          clickAwareHandlers.onPointerUp(e);
          return;
        }
        pickAt(e.clientX, e.clientY);
        return;
      }
      clickAwareHandlers.onPointerUp(e);
    },
  };

  // 冻结帧 + 遮罩 + 选区：rect/display 变化时重绘。常驻复用下每轮 payload 的 frameUrl
  // 携带后端版本化文件名（capture_screens 按时间戳命名，Plan B Task 1 cache-bust），
  // URL 变化即绕开 Webview 缓存，无需前端追加 query。Task 2b：帧为原始物理 RGBA 文件，
  // fetch 字节后 putImageData 直写，不再经 <img> 的 PNG 解码。
  // 原始字节按 frameUrl 缓存（记 Promise：首帧在途时选区已开始变化的重入共享同一次加载，
  // 拖拽重绘零重复请求）。
  const frameRef = useRef<{ url: string; load: Promise<ArrayBuffer> } | null>(null);
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !display) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    // M2 M5：旧帧字节即刻失效（新一轮装载期拾色无源可采，不展示陈旧颜色）；
    // 校验通过后由 draw 回填。
    frameBufRef.current = null;
    // 物理尺寸 = 逻辑 × scale（与 capture_screens/CaptureMeta 同一公式）
    const pw = Math.round(display.width * display.scale);
    const ph = Math.round(display.height * display.scale);
    let disposed = false;

    let load: Promise<ArrayBuffer>;
    if (frameRef.current && frameRef.current.url === display.frameUrl) {
      load = frameRef.current.load;
    } else {
      load = fetchRaw(convertFileSrc(display.frameUrl.replace("hawk-grab://", "")));
      frameRef.current = { url: display.frameUrl, load };
      // 加载失败清缓存项，下一轮 effect 可重试
      load.catch(() => {
        if (frameRef.current?.url === display.frameUrl) frameRef.current = null;
      });
    }

    const draw = (buf: ArrayBuffer) => {
      if (buf.byteLength !== pw * ph * 4) {
        console.error(`冻结帧字节数不符：got ${buf.byteLength}, expect ${pw * ph * 4}`);
        return;
      }
      // M2 M5：字节长校验通过后缓存原始帧——拾色采样源（零 IPC，picker.ts 直索引）
      frameBufRef.current = buf;
      const frame = new ImageData(new Uint8ClampedArray(buf), pw, ph);
      // 背衬 = 物理像素，CSS 尺寸 = 逻辑像素：Retina 下逐像素清晰（顺带修复旧实现
      // 逻辑画布被浏览器插值放大的发软问题）。重设 width/height 同时重置 ctx 状态。
      canvas.width = pw;
      canvas.height = ph;
      canvas.style.width = `${display.width}px`;
      canvas.style.height = `${display.height}px`;
      // putImageData 不受 transform 影响（恰好物理 1:1 直写）；遮罩/选区/标注仍用逻辑
      // 坐标，经 transform 映射到物理背衬。用 setTransform（而非 ctx.scale）保证 effect
      // 因 rect 变化重跑时 transform 绝对重设，绝不叠加。
      ctx.setTransform(display.scale, 0, 0, display.scale, 0, 0);
      ctx.putImageData(frame, 0, 0);
      ctx.fillStyle = "rgba(0,0,0,0.35)";
      ctx.fillRect(0, 0, display.width, display.height);
      // Task 6：hover 高亮——遮罩之后、选区描边之前（裁定 3）。selected 态不高亮：
      // 动作浮条已锚定选区，再画橙色悬停框会与选区描边混淆。此时 state 照常更新，
      // 回到 idle（新一轮/取消）即恢复。
      if (hoverRect && (phase === "idle" || phase === "dragging")) {
        ctx.fillStyle = "rgba(255,176,32,0.15)";
        ctx.fillRect(hoverRect.x, hoverRect.y, hoverRect.w, hoverRect.h);
        ctx.strokeStyle = "#FFB020";
        ctx.lineWidth = 2;
        ctx.strokeRect(hoverRect.x, hoverRect.y, hoverRect.w, hoverRect.h);
      }
      if (rect && rect.w >= 2 && rect.h >= 2) {
        // 选区内恢复未加暗的原始像素（旧实现 drawImage 的等价物）：putImageData 的
        // dirty-rect 形式同为设备像素，坐标须按 scale 换算
        ctx.putImageData(frame, 0, 0,
          Math.round(rect.x * display.scale), Math.round(rect.y * display.scale),
          Math.round(rect.w * display.scale), Math.round(rect.h * display.scale));
        ctx.strokeStyle = "#FFB020";
        ctx.lineWidth = 2;
        ctx.strokeRect(rect.x + 1, rect.y + 1, rect.w - 2, rect.h - 2);
        const label = `${rect.w}×${rect.h}`;
        ctx.font = "13px system-ui, sans-serif";
        const tw = ctx.measureText(label).width + 12;
        ctx.fillStyle = "#FFFFFF";
        ctx.fillRect(rect.x + rect.w - tw, rect.y + rect.h + 6, tw, 20);
        ctx.fillStyle = "#0D1522";
        ctx.fillText(label, rect.x + rect.w - tw + 6, rect.y + rect.h + 20);
        // M2 M5 像素标尺：上/左边缘外侧 2px 起的刻度线（每 10px 短刻 4px、每 50px
        // 长刻 8px，半透明白）。纯 canvas 按当前 rect 现算、无新 state——拖拽与
        // 选中态都画（rect 非空即此两态），rect 变化天然随本 effect 刷新；选区
        // 过短（<20px）时 rulerTicks 返回空、整体跳过（只剩噪音的标尺不画）。
        if (rect.w >= 20 && rect.h >= 20) {
          ctx.strokeStyle = "rgba(255,255,255,0.5)";
          ctx.lineWidth = 1;
          ctx.beginPath();
          for (const tk of rulerTicks(rect.w)) {
            const x = rect.x + tk.at + 0.5; // +0.5 对齐像素网格（dpr=1 下 1px 线不虚）
            ctx.moveTo(x, rect.y - 2 - (tk.major ? 8 : 4));
            ctx.lineTo(x, rect.y - 2);
          }
          for (const tk of rulerTicks(rect.h)) {
            const y = rect.y + tk.at + 0.5;
            ctx.moveTo(rect.x - 2 - (tk.major ? 8 : 4), y);
            ctx.lineTo(rect.x - 2, y);
          }
          ctx.stroke();
        }
      }
      // 本轮冻结帧画出后回报一次（Rust 侧 mark_overlay_shown_once 只记每轮首个，
      // 门闩与 hotkey 1:1 配对）；shownRef 已在下一轮 payload 到达时复位
      if (!shownRef.current) {
        shownRef.current = true;
        void emit("hawk://overlay-shown");
      }
    };
    load.then((buf) => {
      if (!disposed) draw(buf);
    }).catch((err) => {
      if (!disposed) console.error("冻结帧加载失败", err);
    });
    return () => { disposed = true; };
  }, [rect, display, phase, hoverRect]);

  if (!display) return null;
  return (
    <>
      <canvas ref={canvasRef}
        style={{ position: "fixed", inset: 0, cursor: "crosshair" }}
        {...pickerAwareHandlers}
        onDoubleClick={() => confirm()} />
      {/* 十字准线（0.1 hotfix 3）：横竖两条 1px 线，difference 混合在任意底色可见；
          pointerEvents:none 不拦事件，z 5 低于动作浮条（z 10）。覆盖层常驻复用，
          组件只在 display 就绪后渲染，故线随窗口生命周期天然存在。 */}
      <div ref={crossHRef} style={{ position: "fixed", left: 0, right: 0, top: 0, height: 1,
        background: "rgba(255,255,255,0.8)", mixBlendMode: "difference",
        pointerEvents: "none", zIndex: 5, transform: "translateY(-100px)" }} />
      <div ref={crossVRef} style={{ position: "fixed", top: 0, bottom: 0, left: 0, width: 1,
        background: "rgba(255,255,255,0.8)", mixBlendMode: "difference",
        pointerEvents: "none", zIndex: 5, transform: "translateX(-100px)" }} />
      {/* M2 M5 取色浮签：位置/颜色由 pointermove effect 直写（零 React state）；
          初始 transform 移到屏外，首次移动前不显示。z 9 低于动作浮条（10）。 */}
      {pickerMode && display && (
        <div ref={pickerChipRef} style={{ position: "fixed", left: 0, top: 0,
          transform: "translate(-300px, -300px)", pointerEvents: "none", zIndex: 9,
          display: "flex", gap: 8, alignItems: "center", background: "#0D1522",
          border: "1px solid #22405F", borderRadius: 8, padding: "8px 10px" }}>
          <div ref={pickerSwatchRef} style={{ width: 28, height: 28, borderRadius: 4,
            border: "1px solid rgba(255,255,255,0.4)", background: "#000000" }} />
          <div style={{ display: "flex", flexDirection: "column", gap: 2,
            fontFamily: "ui-monospace, Menlo, monospace", fontSize: 12, lineHeight: "16px" }}>
            <span ref={pickerHexRef} style={{ color: "#FFFFFF" }}>#000000</span>
            <span ref={pickerRgbRef} style={{ color: "#D8E2EE" }}>rgb(0, 0, 0)</span>
          </div>
        </div>
      )}
      {/* 拾色模式提示条 + 已复制闪样（复制即退出拾色，两者天然互斥；同款样式，
          闪样用绿色文字区分成败）。 */}
      {pickerMode && display && (
        <div style={{ position: "fixed", top: 12, left: "50%", transform: "translateX(-50%)",
          zIndex: 10, pointerEvents: "none", background: "#0D1522",
          border: "1px solid #22405F", borderRadius: 8, padding: "4px 12px",
          fontSize: 12, color: "#D8E2EE" }}>{t("overlay.picker.hint")}</div>
      )}
      {copied && (
        <div style={{ position: "fixed", top: 12, left: "50%", transform: "translateX(-50%)",
          zIndex: 10, pointerEvents: "none", background: "#0D1522",
          border: "1px solid #22405F", borderRadius: 8, padding: "4px 12px",
          fontSize: 12, color: "#7ED087" }}>{t("overlay.picker.copied", { color: copied })}</div>
      )}
      {/* N1 录制模式提示横幅：与截图模式唯一的常驻视觉差异（选区视觉刻意一致）。
          N2：横幅右侧追加 MP4/GIF 分段开关（输出格式在录制开始前选择，确认时随
          record_start_region 下发；格式选中态为高亮胶囊）。录制启动失败时整组替换
          为红色行内错误（覆盖层保留可重试）。
          —— 终审 review wave I3：**只在 recordMode 渲染**。旧实现把本组渲染在
          `(recordMode || scrollshotMode)` 上 → 长截图轮里同时出现「录制」横幅和
          长截图横幅（两者都是 fixed top:12px left:50% → 叠字），且那个 MP4/GIF
          开关在长截图轮里**毫无用处**（长截图不产出 MP4/GIF），它改的 state 却会
          顺延到下一轮录制。长截图轮现在只有自己的提示条。 */}
      {/* N5 长截图模式提示横幅：与截图/录制同款视觉，仅文案不同（选区 UI 刻意一致）。 */}
      {scrollshotMode && !recordError && (
        <div style={{ position: "fixed", top: 12, left: "50%", transform: "translateX(-50%)",
          zIndex: 10, pointerEvents: "none", background: "#0D1522",
          border: "1px solid #22405F", borderRadius: 8, padding: "4px 12px",
          fontSize: 12, color: "#D8E2EE" }}>
          {t("scrollshot.hint")}
        </div>
      )}
      {/* 行内错误（录制启动失败 / 长截图启动失败）：两种模式共用同一插槽，
          位置与提示条同（fixed top:12px left:50%）→ 与提示条互斥（同一时刻
          只可能有一个：提示条在无错误时渲染，错误出现时替换它）。 */}
      {(recordMode || scrollshotMode) && recordError && (
        <div style={{ position: "fixed", top: 12, left: "50%", transform: "translateX(-50%)",
          zIndex: 10, pointerEvents: "none", background: "#0D1522",
          border: "1px solid #5B2430", borderRadius: 8, padding: "4px 12px",
          fontSize: 12, color: "#E5484D", maxWidth: "80vw" }}>{recordError}</div>
      )}
      {recordMode && !recordError && (
        <div style={{ position: "fixed", top: 12, left: "50%", transform: "translateX(-50%)",
          zIndex: 10, display: "flex", gap: 8, alignItems: "center" }}>
          <div style={{ background: "#0D1522", border: "1px solid #22405F", borderRadius: 8,
            padding: "4px 12px", fontSize: 12, color: "#D8E2EE", pointerEvents: "none" }}>
            {t("overlay.record.hint")}
          </div>
          <div role="group" style={{ display: "flex", background: "#0D1522",
            border: "1px solid #22405F", borderRadius: 8, overflow: "hidden" }}>
            {([
              { id: "mp4" as const, label: t("record.format.mp4") },
              { id: "gif" as const, label: t("record.format.gif") },
            ]).map(({ id, label }) => (
              <button key={id} type="button" aria-pressed={recordFormat === id}
                onClick={() => setRecordFormat(id)}
                style={{ padding: "4px 10px", fontSize: 12, lineHeight: "16px",
                  cursor: "pointer", border: "none",
                  background: recordFormat === id ? "#22405F" : "transparent",
                  color: recordFormat === id ? "#FFFFFF" : "#8A8F98" }}>
                {label}
              </button>
              ))}
            </div>
          </div>
        )}
      {/* 选区确认浮条：canvas 的兄弟节点（fixed 定位）。pointer/dblclick 处理器都只挂在
          canvas 元素上，浮条内的 mousedown/双击按构造不进入选区状态机——既不清选区也不
          触发拖拽/双击确认。Enter 缺省 confirm() = "copy"（语义裁定：保存须显式点按钮）。
          N1：录制模式动作浮条**缺席**（确认语义只有「录制这个选区」——Enter/双击/点击，
          无复制/保存/标注分支），选区出现即录即可达。 */}
      {phase === "selected" && rect && !recordMode && !scrollshotMode && (
        <div style={{ position: "fixed", display: "flex", gap: 8, alignItems: "center",
          ...barPosition(rect, display.width, display.height),
          background: "#0D1522", border: "1px solid #22405F", borderRadius: 8,
          padding: "6px 10px", zIndex: 10 }}>
          <button type="button" onClick={() => confirm("copy")}
            style={{ padding: "4px 12px", background: "#22405F", color: "#FFFFFF",
              border: "1px solid #22405F", borderRadius: 6,
              fontSize: 13, cursor: "pointer" }}>{t("overlay.copy")}</button>
          <button type="button" onClick={() => confirm("save")}
            style={{ padding: "4px 12px", background: "transparent", color: "#D8E2EE",
              border: "1px solid #22405F", borderRadius: 6,
              fontSize: 13, cursor: "pointer" }}>{t("overlay.save")}</button>
          {/* M2 Task M3：标注启用——当前选区直进编辑器（annotate_crop，见上方 annotate 注） */}
          <button type="button" onClick={() => void annotate()}
            style={{ padding: "4px 12px", background: "transparent", color: "#D8E2EE",
              border: "1px solid #22405F", borderRadius: 6,
              fontSize: 13, cursor: "pointer" }}>{t("overlay.annotate")}</button>
          {[
            { id: "pin", label: t("overlay.pin") },
            { id: "ocr", label: t("overlay.ocr") },
            { id: "gif", label: t("overlay.gif") },
          ].map(({ id, label }) => (
            <button key={id} type="button" disabled title={t("overlay.comingSoon")}
              style={{ padding: "4px 8px", background: "transparent", color: "#D8E2EE",
                border: "1px solid #22405F", borderRadius: 6, opacity: 0.4,
                fontSize: 13, cursor: "not-allowed" }}>{label}</button>
          ))}
        </div>
      )}
    </>
  );
}
