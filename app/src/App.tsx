import { useCallback, useEffect, useState, useSyncExternalStore, type CSSProperties } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  t,
  setLanguage,
  getLanguage,
  subscribe,
  type LanguageSetting,
} from "./lib/i18n";
import { formatShotDate, shotKind, canReedit, type ShotRow } from "./lib/history";

// —— 设置模型（C7）：与 Rust Settings（serde camelCase）逐字段对齐 ——
interface Settings {
  hotkeyCapture: string;
  hotkeyPin: string;
  hotkeyRecord: string;
  outputFormat: string; // "png" | "jpeg"（服务端 sanitize 兜底）
  jpegQuality: number; // 1-100（服务端 clamp）
  saveDir: string | null; // null/空 = 默认 ~/Pictures/HawkSnap
  launchAtLogin: boolean;
  appearance: string; // "system" | "light" | "dark"
  language: string; // "system" | "zh-CN" | "en-US"（服务端 sanitize 兜底，D1）
  updateEnabled: boolean; // 自动更新开关（D3；服务端不 sanitize，bool 无脏值）
}

// 表单态：saveDir 归一为 string（null ↔ "" 双向转换），便于受控输入。
type SettingsForm = Omit<Settings, "saveDir"> & { saveDir: string };

const DEFAULTS: SettingsForm = {
  hotkeyCapture: "F1",
  hotkeyPin: "F2",
  hotkeyRecord: "F6",
  outputFormat: "png",
  jpegQuality: 90,
  saveDir: "",
  launchAtLogin: false,
  appearance: "system",
  language: "system",
  updateEnabled: true,
};

const toForm = (s: Settings): SettingsForm => ({ ...s, saveDir: s.saveDir ?? "" });
const toPayload = (f: SettingsForm): Settings => ({
  ...f,
  saveDir: f.saveDir.trim() === "" ? null : f.saveDir.trim(),
});

type HotkeySlot = "hotkeyCapture" | "hotkeyPin" | "hotkeyRecord";

// —— 自动更新（D3）—— 与后端 updater.rs 对齐 ——
interface UpdateInfo {
  version: string;
  notes: string | null;
  currentVersion: string;
}

// hawk://update-progress 载荷（serde camelCase）：total 可能缺失（Content-Length 未知）。
interface UpdateProgress {
  downloaded: number;
  total: number | null;
}

// 更新区状态机：idle → checking → latest | available → downloading → installed。
type UpdateState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "latest" }
  | { phase: "available"; info: UpdateInfo }
  | { phase: "downloading"; progress: UpdateProgress | null }
  | { phase: "installed" };

// 后端哨兵（updater.rs ERR_NOT_CONFIGURED）：更新通道未配置（占位公钥期常态）——
// 映射为 i18n 文案而非报错。双端字符串一致由 updater.rs 单测 + i18n.test.ts 兜底。
const ERR_UPDATER_NOT_CONFIGURED = "updater_not_configured";

// 启动静默检查去重：面板会被 idle_reaper 销毁重建（webview 重挂载），每次重挂载
// 都打一次更新接口是惊扰——本模块级标记保证每次应用运行（webview 生命周期）只自动
// 检查一次；用户点「检查更新」不受此限（显式动作）。
let autoCheckFired = false;

// 字节 → MB 展示（一位小数；进度文本里的数字不是文案，不入词典）。
const fmtMb = (bytes: number) => (bytes / 1024 / 1024).toFixed(1);

// 热键注册失败载荷（D0-⑤）：与后端 HotkeyError（serde camelCase）对齐——kind 供横幅
// 归因，message 为人类可读文案（注册失败原因或媒体键降级提示）。kind 的展示名走
// t(`hotkey.kind.${kind}`)（唯一约定的动态 key 族，单测断言双语在盘——检查器扫不到
// 模板字面量，scripts/check-i18n.mjs 对此类调用报 warn 不 fail）。
type HotkeyKind = "capture" | "pin" | "record";
interface HotkeyErrorPayload {
  kind: HotkeyKind;
  message: string;
}

// 纯修饰键按下（无主键）：不算组合，继续等待。
const MODIFIER_ONLY_KEYS = new Set([
  "Meta", "Shift", "Alt", "Control", "CapsLock", "Fn", "FnLock", "Hyper", "Symbol",
]);

// 组合键 → 插件词汇表字符串（global-hotkey parse_hotkey：修饰键 Cmd/Shift/Alt/Ctrl
// 在前 + 主键，"+" 连接；主键大小写不敏感，字母统一大写展示）。返回 null = 忽略
// （纯修饰键）；不认识的键不在此拦截——服务端 set_settings 解析失败即冲突 Err，
// 服务端是唯一权威。
function comboFromEvent(e: KeyboardEvent): string | null {
  if (MODIFIER_ONLY_KEYS.has(e.key)) return null;
  const mods: string[] = [];
  if (e.metaKey) mods.push("Cmd");
  if (e.shiftKey) mods.push("Shift");
  if (e.altKey) mods.push("Alt");
  if (e.ctrlKey) mods.push("Ctrl");
  // 数字键带修饰时 e.key 会变成符号（Shift+1 → "!"），按 e.code 取回数字本键；
  // 其余单字符统一大写（插件 parse_key 大小写不敏感，展示习惯大写字母）。
  let key: string;
  if (/^Digit\d$/.test(e.code)) key = e.code.slice(5);
  else if (e.key === " ") key = "Space";
  else if (e.key.length === 1) key = e.key.toUpperCase();
  else key = e.key;
  return [...mods, key].join("+");
}

function App() {
  // 订阅 i18n 语言态：语言切换触发本组件重渲染，全部 t() 文案即时换新（不重载）。
  useSyncExternalStore(subscribe, getLanguage);
  const [hotkeyError, setHotkeyError] = useState<HotkeyErrorPayload | null>(null);
  const [captureError, setCaptureError] = useState<string | null>(null);
  const [recordError, setRecordError] = useState<string | null>(null);
  const [form, setForm] = useState<SettingsForm>(DEFAULTS);
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const [capturing, setCapturing] = useState<HotkeySlot | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveOk, setSaveOk] = useState(false);
  const [updateState, setUpdateState] = useState<UpdateState>({ phase: "idle" });
  const [updateError, setUpdateError] = useState<string | null>(null);
  // —— 历史分区（M0）—— 懒加载：挂载即 history_list(50)（面板打开时填充）；输入
  // 经 250ms 防抖切换到 history_search（计划接口）。确认删除是瓦片内两步按钮
  // （[删除]→[确认删除]，避免原生 dialog 管线）。
  const [shots, setShots] = useState<ShotRow[]>([]);
  const [historyQuery, setHistoryQuery] = useState("");
  const [debouncedQuery, setDebouncedQuery] = useState("");
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [confirmDeleteId, setConfirmDeleteId] = useState<number | null>(null);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    // pull 通道：setup 期的 hawk://hotkey-error emit 早于 listen 注册必丢，挂载即向后端
    // 拉取（后端读取即清除）。非 Tauri 环境下降级为无提示。
    invoke<HotkeyErrorPayload | null>("hotkey_error")
      .then((err) => {
        if (!disposed && err) setHotkeyError(err);
      })
      .catch(() => {});
    // push 通道：覆盖挂载后发生的注册失败（spec §9：不允许静默失败）。
    listen<HotkeyErrorPayload>("hawk://hotkey-error", (e) => setHotkeyError(e.payload))
      .then((un) => {
        if (disposed) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 截图失败横幅：F1/托盘两条路径的 start_capture_flow 失败均 emit hawk://capture-error
  // （spec §9：不允许静默失败）。
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen<string>("hawk://capture-error", (e) => setCaptureError(e.payload))
      .then((un) => {
        if (disposed) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 录屏失败横幅（M6）：F6 toggle / 悬浮条操作的录屏错误经 hawk://record-error 回流
  // （panel::show_record_error 懒拉主面板后 emit——与 capture-error 同款不静默收尾）。
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen<string>("hawk://record-error", (e) => setRecordError(e.payload))
      .then((un) => {
        if (disposed) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 挂载即拉取设置填充表单（面板由 idle_reaper 销毁后重建时会重挂载 → 总能拿到最新值）。
  // 语言（D1）：i18n 模块默认跟随系统，显式设置以此处为准对齐（本窗口与覆盖层窗口同源）。
  useEffect(() => {
    invoke<Settings>("get_settings")
      .then((s) => {
        setForm(toForm(s));
        setLanguage(s.language as LanguageSetting);
      })
      .catch((e) => setSettingsError(String(e)));
  }, []);

  // 热键捕获模式：捕获态挂 window 捕获相 keydown——下一个组合键写入对应槽位；Esc 取消；
  // 纯修饰键忽略。capture 相 + stopPropagation 避免页面其他快捷逻辑收到该事件。
  useEffect(() => {
    if (!capturing) return;
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape") {
        setCapturing(null);
        return;
      }
      const combo = comboFromEvent(e);
      if (!combo) return;
      setForm((f) => ({ ...f, [capturing]: combo }));
      setCapturing(null);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [capturing]);

  // —— 历史分区（M0）—— 搜索输入 250ms 防抖（计划接口）；空查询走 history_list，
  // 非空走 history_search。加载失败入 historyError（分区内横幅，不打扰设置区）。
  const refreshShots = useCallback((query: string) => {
    const req =
      query.trim() === ""
        ? invoke<ShotRow[]>("history_list", { limit: 50, offset: 0 })
        : invoke<ShotRow[]>("history_search", { query, limit: 50 });
    req
      .then((rows) => {
        setShots(rows);
        setHistoryError(null);
      })
      .catch((e) => setHistoryError(String(e)));
  }, []);

  useEffect(() => {
    const h = window.setTimeout(() => setDebouncedQuery(historyQuery), 250);
    return () => window.clearTimeout(h);
  }, [historyQuery]);

  useEffect(() => {
    refreshShots(debouncedQuery);
  }, [debouncedQuery, refreshShots]);

  const pinShot = (path: string) => {
    invoke<number>("pin_from_path", { path }).catch((e) => setHistoryError(String(e)));
  };
  const openShot = (path: string) => {
    invoke<void>("open_path", { path }).catch((e) => setHistoryError(String(e)));
  };
  const removeShot = (id: number) => {
    invoke<void>("history_delete", { id })
      .then(() => {
        setConfirmDeleteId(null);
        refreshShots(debouncedQuery);
      })
      .catch((e) => setHistoryError(String(e)));
  };
  // M2 Task M3：重新编辑 → editor_open（origin=null：保存只覆盖原文件，无剪贴板/
  // 回写/重复入库副作用——契约在 Rust editor.rs「来源契约」）。
  const reeditShot = (path: string) => {
    invoke<void>("editor_open", { path, origin: null }).catch((e) =>
      setHistoryError(String(e)),
    );
  };

  // 自动更新（D3）：下载进度 push 通道。仅 downloading 相消费；其他相收到的
  // 迟到事件（上一次安装的尾巴）直接忽略，防止进度文本串台。
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen<UpdateProgress>("hawk://update-progress", (e) => {
      setUpdateState((s) =>
        s.phase === "downloading" ? { phase: "downloading", progress: e.payload } : s,
      );
    })
      .then((un) => {
        if (disposed) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 启动静默检查（评审 Fix 1：让 update_enabled 真正门控自动检查）。manual=false 时
  // 门控全在命令侧：开关关闭 → Ok(None)；占位公钥 → 同样 Ok(None)——两者都渲染为无。
  // 只有真的发现新版本才出「发现新版本 vX」状态；失败静默降级（spec §9 精神：不打扰，
  // 用户可随时手动检查获知失败原因）。挂载即发（命令自读服务端设置，无需等表单水合）。
  useEffect(() => {
    if (autoCheckFired) return;
    autoCheckFired = true;
    invoke<UpdateInfo | null>("check_for_updates", { manual: false })
      .then((info) => {
        if (info) setUpdateState((s) => (s.phase === "idle" ? { phase: "available", info } : s));
      })
      .catch(() => {});
  }, []);

  // 手动检查更新（manual=true 覆盖 update_enabled 开关——用户显式动作）。
  const checkForUpdates = () => {
    setUpdateError(null);
    setUpdateState({ phase: "checking" });
    invoke<UpdateInfo | null>("check_for_updates", { manual: true })
      .then((info) =>
        setUpdateState(info ? { phase: "available", info } : { phase: "latest" }),
      )
      .catch((e) => {
        setUpdateState({ phase: "idle" });
        if (String(e) === ERR_UPDATER_NOT_CONFIGURED) {
          setUpdateError(t("settings.updates.notConfigured"));
        } else {
          setUpdateError(t("banner.updateError", { message: String(e) }));
        }
      });
  };

  // 下载并安装：进度经 hawk://update-progress（上方 listener 写入 downloading 相）。
  // Ok → installed（提示重启，不自动重启）；Err → 横幅 + 回 idle 可重试。
  const installUpdate = () => {
    setUpdateError(null);
    setUpdateState((s) =>
      s.phase === "available" ? { phase: "downloading", progress: null } : s,
    );
    invoke<void>("install_update")
      .then(() => setUpdateState({ phase: "installed" }))
      .catch((e) => {
        setUpdateState({ phase: "idle" });
        if (String(e) === ERR_UPDATER_NOT_CONFIGURED) {
          setUpdateError(t("settings.updates.notConfigured"));
        } else {
          setUpdateError(t("banner.updateError", { message: String(e) }));
        }
      });
  };

  // 立即重启（AppHandle::restart 不返回，进程直接换成新版本）。
  const restartApp = () => {
    invoke<void>("restart_app").catch((e) =>
      setUpdateError(t("banner.updateError", { message: String(e) })),
    );
  };

  // 面板入口（C6）：与 F1/F2 热键、托盘同链路（panel_trigger → start_capture_flow /
  // pin_latest），失败经 hawk://capture-error 横幅回流。热键名随设置回显（C7）。
  const trigger = (action: "capture" | "pin") => {
    invoke<void>("panel_trigger", { action }).catch((e) => setCaptureError(String(e)));
  };

  // 保存：客户端只做非空校验，冲突/解析权威在服务端——set_settings Err → 横幅 +
  // 重新拉取服务端设置回显（Err 不携带设置，且文件保持旧值）。
  const save = () => {
    if (
      form.hotkeyCapture.trim() === "" ||
      form.hotkeyPin.trim() === "" ||
      form.hotkeyRecord.trim() === ""
    ) {
      setSettingsError(t("settings.error.emptyHotkey"));
      return;
    }
    setSaving(true);
    invoke<Settings>("set_settings", { s: toPayload(form) })
      .then((effective) => {
        setForm(toForm(effective)); // 回显服务端生效值（sanitize 后）
        setLanguage(effective.language as LanguageSetting); // 语言随保存即时生效
        setSettingsError(null);
        setSaveOk(true);
        window.setTimeout(() => setSaveOk(false), 1600);
      })
      .catch((e) => {
        setSettingsError(String(e));
        return invoke<Settings>("get_settings").then((s) => setForm(toForm(s)));
      })
      .finally(() => setSaving(false));
  };

  const setField = <K extends keyof SettingsForm>(key: K, value: SettingsForm[K]) =>
    setForm((f) => ({ ...f, [key]: value }));

  return (
    <main className="container">
      {hotkeyError && (
        <div role="alert" style={banner}>
          {t("banner.hotkeyError", {
            kind: t(`hotkey.kind.${hotkeyError.kind}`),
            message: hotkeyError.message,
          })}
        </div>
      )}
      {captureError && (
        <div role="alert" style={banner}>
          {t("banner.captureError", { message: captureError })}
        </div>
      )}
      {recordError && (
        <div role="alert" style={banner}>
          {t("banner.recordError", { message: recordError })}
        </div>
      )}
      {settingsError && (
        <div role="alert" style={banner}>
          {t("banner.settingsError", { message: settingsError })}
        </div>
      )}
      {updateError && (
        <div role="alert" style={banner}>
          {updateError}
        </div>
      )}
      <h1>HawkSnap</h1>
      <section style={{ display: "flex", gap: 12, justifyContent: "center", margin: "16px 0" }}>
        <button style={entryBtn} onClick={() => trigger("capture")}>
          {t("panel.capture", { key: form.hotkeyCapture })}
        </button>
        <button style={entryBtn} onClick={() => trigger("pin")}>
          {t("panel.pin", { key: form.hotkeyPin })}
        </button>
      </section>
      {/* —— 历史分区（M0）：替代 C6 的禁用「历史」入口行 —— */}
      <section style={historySection}>
        <h2 style={{ fontSize: 16, margin: "0 0 8px" }}>{t("history.title")}</h2>
        <input
          type="search"
          style={{ ...entryBtn, cursor: "text", width: "100%", boxSizing: "border-box" }}
          placeholder={t("history.searchPlaceholder")}
          aria-label={t("history.searchPlaceholder")}
          value={historyQuery}
          onChange={(e) => setHistoryQuery(e.target.value)}
        />
        {historyError && (
          <div role="alert" style={historyBanner}>
            {t("history.actionError", { message: historyError })}
          </div>
        )}
        {shots.length === 0 ? (
          <p style={{ color: "#666", textAlign: "center" }}>
            {debouncedQuery.trim() === "" ? t("history.empty") : t("history.emptySearch")}
          </p>
        ) : (
          <div style={grid}>
            {shots.map((shot) => {
              const title = `${formatShotDate(shot.createdAt, getLanguage())} · ${shot.app}`;
              return (
                <div key={shot.id} style={tile} title={title}>
                  {shot.thumbPath ? (
                    <img
                      src={convertFileSrc(shot.thumbPath)}
                      alt={title}
                      loading="lazy"
                      style={thumb}
                    />
                  ) : shotKind(shot) === "recording" ? (
                    // 录屏条目（M6）：无缩略图 → 🎬 占位（贴图/重新编辑对 MP4 无意义，
                    // 下方动作区隐藏；打开目录/删除照常）。按 kind 判定（不再靠 app
                    // 列复用——kind 已是契约字段）。
                    <div style={{ ...thumb, background: "#e9e9ee", display: "flex", alignItems: "center", justifyContent: "center", fontSize: 28 }} aria-hidden="true">🎬</div>
                  ) : (
                    <div style={{ ...thumb, background: "#e9e9ee" }} aria-hidden="true" />
                  )}
                  <div style={tileMeta}>{title}</div>
                  <div style={tileActions}>
                    <button style={tileBtn} onClick={() => pinShot(shot.path)}>
                      {t("history.pin")}
                    </button>
                    <button style={tileBtn} onClick={() => openShot(shot.path)}>
                      {t("history.openFolder")}
                    </button>
                    {confirmDeleteId === shot.id ? (
                      // 长hand 边框（不与 tileBtn 的 border 简写混用，避免 React 样式合并告警）
                      <button style={tileBtnDanger} onClick={() => removeShot(shot.id)}>
                        {t("history.confirmDelete")}
                      </button>
                    ) : (
                      <button style={tileBtn} onClick={() => setConfirmDeleteId(shot.id)}>
                        {t("history.delete")}
                      </button>
                    )}
                    {/* M2 Task M3：重新编辑启用——editor_open（origin=null）；录屏条目
                        不提供（MP4 非编辑器输入，M6 裁定）；**长截图条目同样不提供**
                        （终审 wave I7：长图可达 32MP，编辑器画布/导出路径按巨幅位图
                        走会出内存尖峰与导出超时）。按 kind 门控。 */}
                    {canReedit(shot) && (
                      <button style={tileBtn} onClick={() => reeditShot(shot.path)}>
                        {t("history.reedit")}
                      </button>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </section>
      <section style={settingsSection}>
        <h2 style={{ fontSize: 16, margin: "0 0 12px" }}>{t("settings.title")}</h2>

        <div style={fieldRow}>
          <label style={fieldLabel}>{t("settings.hotkeyLabel")}</label>
          <div style={{ display: "grid", gap: 6 }}>
            {([
              { key: "hotkeyCapture" as const, label: t("settings.slot.capture") },
              { key: "hotkeyPin" as const, label: t("settings.slot.pin") },
              { key: "hotkeyRecord" as const, label: t("settings.slot.record") },
            ]).map(({ key, label }) => (
              <div key={key} style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <span style={{ width: 90, color: "#666" }}>{label}</span>
                <button
                  type="button"
                  style={{ ...entryBtn, minWidth: 140, fontFamily: "monospace" }}
                  onClick={() => setCapturing(key)}
                >
                  {form[key]}
                </button>
                {capturing === key && (
                  <span style={{ color: "#396cd8" }}>{t("settings.pressCombo")}</span>
                )}
              </div>
            ))}
          </div>
        </div>

        <div style={fieldRow}>
          <label style={fieldLabel}>{t("settings.outputFormat")}</label>
          <div style={{ display: "flex", alignItems: "center", gap: 16 }}>
            <label>
              <input
                type="radio"
                name="output-format"
                checked={form.outputFormat === "png"}
                onChange={() => setField("outputFormat", "png")}
              />{" "}
              PNG
            </label>
            <label>
              <input
                type="radio"
                name="output-format"
                checked={form.outputFormat === "jpeg"}
                onChange={() => setField("outputFormat", "jpeg")}
              />{" "}
              JPEG
            </label>
            {form.outputFormat === "jpeg" && (
              <label style={{ display: "flex", alignItems: "center", gap: 8 }}>
                {t("settings.quality", { n: form.jpegQuality })}
                <input
                  type="range"
                  min={1}
                  max={100}
                  value={form.jpegQuality}
                  onChange={(e) => setField("jpegQuality", Number(e.target.value))}
                />
              </label>
            )}
          </div>
        </div>

        <div style={fieldRow}>
          <label style={fieldLabel} htmlFor="save-dir">{t("settings.saveDir")}</label>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              id="save-dir"
              type="text"
              style={{ ...entryBtn, flex: 1, cursor: "text" }}
              placeholder={t("settings.saveDirPlaceholder")}
              value={form.saveDir}
              onChange={(e) => setField("saveDir", e.target.value)}
            />
            <button
              type="button"
              style={entryBtn}
              onClick={() => setField("saveDir", "")}
              title={t("settings.restoreDefaultTip")}
            >
              {t("settings.restoreDefault")}
            </button>
          </div>
        </div>

        <div style={fieldRow}>
          <label style={fieldLabel} htmlFor="launch-at-login">{t("settings.launchAtLogin")}</label>
          <label style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <input
              id="launch-at-login"
              type="checkbox"
              checked={form.launchAtLogin}
              onChange={(e) => setField("launchAtLogin", e.target.checked)}
            />
            {t("settings.launchAtLoginDesc")}
          </label>
        </div>

        <div style={fieldRow}>
          <span style={fieldLabel}>{t("settings.appearance")}</span>
          <div>
            {(["system", "light", "dark"] as const).map((mode) => (
              <label key={mode} style={{ marginRight: 16 }}>
                <input
                  type="radio"
                  name="appearance"
                  checked={form.appearance === mode}
                  onChange={() => setField("appearance", mode)}
                />{" "}
                {mode === "system"
                  ? t("settings.appearanceSystem")
                  : mode === "light"
                    ? t("settings.appearanceLight")
                    : t("settings.appearanceDark")}
              </label>
            ))}
          </div>
        </div>

        <div style={fieldRow}>
          <label style={fieldLabel} htmlFor="language">{t("settings.language")}</label>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            {/* D1：语言选择随表单保存生效（保存后回显生效值并即时切换文案）。 */}
            <select
              id="language"
              style={{ ...entryBtn }}
              value={form.language}
              onChange={(e) => setField("language", e.target.value)}
            >
              <option value="system">{t("settings.langSystem")}</option>
              <option value="zh-CN">{t("settings.langZh")}</option>
              <option value="en-US">{t("settings.langEn")}</option>
            </select>
          </div>
        </div>

        <div style={fieldRow}>
          <label style={fieldLabel} htmlFor="update-enabled">{t("settings.updates")}</label>
          <div style={{ display: "grid", gap: 8 }}>
            <label style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <input
                id="update-enabled"
                type="checkbox"
                checked={form.updateEnabled}
                onChange={(e) => setField("updateEnabled", e.target.checked)}
              />
              {t("settings.updatesAutoDesc")}
            </label>
            <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
              <button
                type="button"
                style={entryBtn}
                onClick={checkForUpdates}
                disabled={updateState.phase === "checking" || updateState.phase === "downloading"}
              >
                {t("settings.checkUpdates")}
              </button>
              {updateState.phase === "checking" && (
                <span style={{ color: "#666" }}>{t("settings.updates.checking")}</span>
              )}
              {updateState.phase === "latest" && (
                <span style={{ color: "#3c763d" }}>{t("settings.updates.latest")}</span>
              )}
              {updateState.phase === "available" && (
                <span style={{ color: "#3c763d" }}>
                  {t("settings.updates.available", { version: updateState.info.version })}
                </span>
              )}
              {updateState.phase === "downloading" && (
                <span style={{ color: "#666" }}>
                  {updateState.progress == null
                    ? t("settings.updates.downloading")
                    : updateState.progress.total != null
                      ? t("settings.updates.progressKnown", {
                          a: fmtMb(updateState.progress.downloaded),
                          b: fmtMb(updateState.progress.total),
                        })
                      : t("settings.updates.progressUnknown", {
                          a: fmtMb(updateState.progress.downloaded),
                        })}
                </span>
              )}
              {updateState.phase === "installed" && (
                <span style={{ color: "#3c763d" }}>{t("settings.updates.installed")}</span>
              )}
              {updateState.phase === "available" && (
                <button type="button" style={entryBtn} onClick={installUpdate}>
                  {t("settings.updates.downloadInstall")}
                </button>
              )}
              {updateState.phase === "installed" && (
                <button type="button" style={entryBtn} onClick={restartApp}>
                  {t("settings.updates.restart")}
                </button>
              )}
            </div>
            {/* 更新说明简版（计划裁定：下载进度级占位，完整 changelog 渲染归 M2）。 */}
            {updateState.phase === "available" && updateState.info.notes && (
              <div style={{ color: "#666", fontSize: 13, whiteSpace: "pre-wrap" }}>
                {updateState.info.notes}
              </div>
            )}
          </div>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 12, marginTop: 16 }}>
          <button type="button" style={{ ...entryBtn, padding: "8px 32px" }} onClick={save} disabled={saving}>
            {saving ? t("settings.saving") : t("settings.save")}
          </button>
          {saveOk && <span style={{ color: "#3c763d" }}>{t("settings.saved")}</span>}
        </div>
      </section>
    </main>
  );
}

const banner: CSSProperties = {
  margin: "12px",
  padding: "10px 14px",
  border: "1px solid #d9534f",
  borderRadius: 6,
  background: "#fdecea",
  color: "#a94442",
  fontSize: 14,
};

const entryBtn: CSSProperties = {
  padding: "10px 20px",
  fontSize: 15,
  borderRadius: 6,
  border: "1px solid #ccc",
  background: "#fff",
  cursor: "pointer",
};

const settingsSection: CSSProperties = {
  margin: "24px auto 40px",
  maxWidth: 560,
  textAlign: "left",
  fontSize: 14,
};

// —— 历史分区（M0）—— 网格 ~120px 瓦片（auto-fill 适配面板宽度），日期+应用做标题。
const historySection: CSSProperties = {
  margin: "0 auto 24px",
  maxWidth: 720,
  textAlign: "left",
  fontSize: 14,
};

const historyBanner: CSSProperties = {
  margin: "8px 0",
  padding: "8px 12px",
  border: "1px solid #d9534f",
  borderRadius: 6,
  background: "#fdecea",
  color: "#a94442",
  fontSize: 13,
};

const grid: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "repeat(auto-fill, minmax(120px, 1fr))",
  gap: 10,
  marginTop: 12,
};

const tile: CSSProperties = {
  border: "1px solid #ddd",
  borderRadius: 6,
  padding: 6,
  display: "grid",
  gap: 6,
  alignContent: "start",
};

const thumb: CSSProperties = {
  width: "100%",
  aspectRatio: "4 / 3",
  objectFit: "cover",
  borderRadius: 4,
  display: "block",
};

const tileMeta: CSSProperties = {
  fontSize: 12,
  color: "#555",
  overflow: "hidden",
  textOverflow: "ellipsis",
  whiteSpace: "nowrap",
};

const tileActions: CSSProperties = {
  display: "flex",
  flexWrap: "wrap",
  gap: 4,
};

const tileBtn: CSSProperties = {
  padding: "3px 6px",
  fontSize: 12,
  borderRadius: 4,
  border: "1px solid #ccc",
  background: "#fff",
  cursor: "pointer",
};

const tileBtnDanger: CSSProperties = {
  padding: "3px 6px",
  fontSize: 12,
  borderRadius: 4,
  borderStyle: "solid",
  borderColor: "#d9534f",
  borderWidth: 1,
  background: "#fff",
  cursor: "pointer",
  color: "#a94442",
};

const fieldRow: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "90px 1fr",
  alignItems: "start",
  gap: 8,
  marginBottom: 14,
};

const fieldLabel: CSSProperties = { color: "#666", paddingTop: 6 };

export default App;
