import { describe, expect, it } from "vitest";
import { formatShotDate, shotKind, canReedit, type ShotRow } from "./history";

describe("formatShotDate", () => {
  it("formats an RFC3339 timestamp for both locales", () => {
    // RFC3339（Rust chrono Local now().to_rfc3339() 口径）带时区
    const iso = "2026-10-23T10:30:00+08:00";
    const zh = formatShotDate(iso, "zh-CN");
    const en = formatShotDate(iso, "en-US");
    expect(zh).toContain("2026");
    expect(en).toContain("2026");
    expect(zh).toMatch(/10/); // 时、分任一处含 10（分钟 30 前 10 时）
    expect(en).toMatch(/10/);
    expect(zh).not.toEqual(en);
  });

  it("returns raw input on unparseable data (库数据不炸 UI)", () => {
    expect(formatShotDate("not-a-date", "zh-CN")).toBe("not-a-date");
    expect(formatShotDate("", "en-US")).toBe("");
  });

  it("shot row contract keeps camelCase fields aligned with Rust ShotRow", () => {
    // serde rename_all = camelCase 对齐守护：字段名漂移在此红
    const row: ShotRow = {
      id: 1,
      createdAt: "2026-10-23T10:30:00+08:00",
      path: "/pics/a.png",
      thumbPath: "/pics/thumbs/a.png",
      w: 100,
      h: 50,
      app: "Safari",
      kind: "region",
    };
    expect(Object.keys(row).sort()).toEqual([
      "app", "createdAt", "h", "id", "kind", "path", "thumbPath", "w",
    ]);
  });
});

describe("shotKind / canReedit（终审 wave：kind 进契约 + 长截图禁重新编辑）", () => {
  const base = {
    id: 1,
    createdAt: "2026-10-23T10:30:00+08:00",
    path: "/pics/a.png",
    thumbPath: "",
    w: 100,
    h: 50,
    app: "App",
  };

  it("reads the kind column", () => {
    expect(shotKind({ ...base, kind: "region" })).toBe("region");
    expect(shotKind({ ...base, kind: "recording" })).toBe("recording");
    expect(shotKind({ ...base, kind: "scrollshot" })).toBe("scrollshot");
  });

  it("falls back to the app column when kind is missing (旧库/旧响应)", () => {
    expect(shotKind({ ...base, app: "recording" })).toBe("recording");
    expect(shotKind({ ...base, app: "Safari" })).toBe("region");
  });

  it("gates re-edit on kind: recording and scrollshot are excluded", () => {
    expect(canReedit({ ...base, kind: "region" })).toBe(true);
    expect(canReedit({ ...base, kind: "recording" })).toBe(false);
    // 长截图产物可达 32MP → 编辑器路径不支持（终审 wave I7）
    expect(canReedit({ ...base, kind: "scrollshot" })).toBe(false);
  });
});
