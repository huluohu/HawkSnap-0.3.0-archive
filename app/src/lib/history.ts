// 历史网格（M0）前端侧纯逻辑：ShotRow 契约与展示格式化。与 Rust history::ShotRow
// （serde camelCase）逐字段对齐；窗口/UI 状态留在 App.tsx，此文件保持纯函数可单测。
export interface ShotRow {
  id: number;
  createdAt: string;
  path: string;
  thumbPath: string;
  w: number;
  h: number;
  app: string;
  /**
   * 条目种类（终审 wave 暴露）：`region`（截图）/ `recording`（录屏 MP4/GIF）/
   * `scrollshot`（长截图）。Rust 侧 kind 列此前**只写不读**，前端靠 app 列冒充
   * （录制的 app 列写死 "recording"）；现在 kind 进契约，按种类的门控（重新编辑
   * 资格、录屏占位图）不再依赖那个复用列。
   */
  kind?: string;
  /** 录制时长（秒）。M3 继承项①新增：截图为 null，仅录制行有值。 */
  duration?: number | null;
}

/** 条目种类的规范取值（与 Rust 侧 kind 列取值一一对应）。 */
export type ShotKind = "region" | "recording" | "scrollshot";

/**
 * 读条目种类：优先用契约里的 kind；缺字段（旧库/旧响应）时回退到 app 列的
 * 兼容判定（app === "recording" → recording，否则按截图处理）。
 */
export function shotKind(row: ShotRow): ShotKind {
  if (row.kind === "recording" || row.kind === "scrollshot" || row.kind === "region") {
    return row.kind;
  }
  return row.app === "recording" ? "recording" : "region";
}

/**
 * 该条目能否「重新编辑」（进标注编辑器）。
 *
 * 录屏条目不可编辑（MP4/GIF 不是编辑器输入）；**长截图也不可编辑**（终审 wave
 * I7）：长截图产物可达 32MP（实测输出高 20145–23912px），把它灌进 2D canvas 会
 * 让编辑器的全画布重绘/导出路径按巨幅位图走（内存尖峰、导出超时）——长图的后
 * 处理需求（裁切/缩放）归后续版本，编辑器暂不支持。
 */
export function canReedit(row: ShotRow): boolean {
  const k = shotKind(row);
  return k !== "recording" && k !== "scrollshot";
}

/**
 * RFC3339 created_at → 本地化「日期 时间」串（网格瓦片标题）。解析失败原样返回
 * （历史库内容是数据不是文案，手改坏不炸 UI）。lang 决定 Intl.Locale：zh-CN / en-US。
 */
export function formatShotDate(iso: string, lang: "zh-CN" | "en-US"): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString(lang, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}
