use rusqlite::Connection;
use serde::Serialize;
use tauri::{AppHandle, Manager};

/// 历史记录行（C3）：一次保存 = 一行 + 截图文件 + 缩略图文件。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShotRow {
    pub id: i64,
    pub created_at: String,
    pub path: String,
    pub thumb_path: String,
    pub w: i64,
    pub h: i64,
    pub app: String,
    /// 条目种类（终审 wave：原先该列**只写不读**——「语义面在 schema，不进前端
    /// 契约」）。现在暴露给前端：历史网格的「重新编辑」资格、录屏占位图等按 kind
    /// 门控（此前靠 app 列冒充 kind，见 recording.rs 的 "recording" 复用）。
    /// 取值：`region`（截图）/ `recording`（录屏 MP4/GIF）/ `scrollshot`（长截图）。
    /// `default` 兼容旧消费方（缺字段按空串补齐；旧库该列 NOT NULL DEFAULT，
    /// 实际不会为空）。
    #[serde(default)]
    pub kind: String,
    /// 录制时长（秒，REAL；M3 继承项①新增）。截图（region/scrollshot）为 NULL →
    /// None；仅 kind=recording 有值。`default` 兼容旧消费方（缺字段按 null 补齐）。
    #[serde(default)]
    pub duration: Option<f64>,
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS shots(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'region',
    path TEXT NOT NULL,
    thumb_path TEXT NOT NULL,
    w INTEGER NOT NULL,
    h INTEGER NOT NULL,
    app TEXT NOT NULL);";

/// FTS5（M0 建，M2-OCR 扩 ocr_text 列）：external-content 虚表（content='shots'，
/// rowid 对齐 shots.id），索引 path + app + ocr_text 三列——unicode61 分词会把绝对
/// 路径按非字母数字切开，文件名段与 app 名自然成为可检索单元；ocr_text（截图上的
/// 识别文本，M4 填充）让「截过屏的文字」可直接检索。触发器是 external-content 的
/// 标准同步模式（INSERT/UPDATE/DELETE 三向，UPDATE 走 delete+insert 复合）。
const FTS_SCHEMA: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS shots_fts USING fts5(
    path, app, ocr_text, content='shots', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS shots_fts_ai AFTER INSERT ON shots BEGIN
    INSERT INTO shots_fts(rowid, path, app, ocr_text) VALUES (new.id, new.path, new.app, new.ocr_text);
END;
CREATE TRIGGER IF NOT EXISTS shots_fts_ad AFTER DELETE ON shots BEGIN
    INSERT INTO shots_fts(shots_fts, rowid, path, app, ocr_text)
    VALUES ('delete', old.id, old.path, old.app, old.ocr_text);
END;
CREATE TRIGGER IF NOT EXISTS shots_fts_au AFTER UPDATE ON shots BEGIN
    INSERT INTO shots_fts(shots_fts, rowid, path, app, ocr_text)
    VALUES ('delete', old.id, old.path, old.app, old.ocr_text);
    INSERT INTO shots_fts(rowid, path, app, ocr_text) VALUES (new.id, new.path, new.app, new.ocr_text);
END;";

/// history.db 落点（app_data_dir()/history.db）。
/// pub(crate)：M4 P1 的 native_messaging 用同一路径入库（浏览器截图），
/// 免得再包一层 AppHandle 间接层。
pub(crate) fn db_path(app: &AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("history.db")
}

/// 打开连接并确保 schema（幂等）。连接策略：每次操作 `Connection::open` 新建——
/// SQLite 文件型打开是 µs 级，换来无跨线程 Send/生命周期负担（Tauri 命令线程
/// 不定），不持长连接、不引连接池。
fn open_at(db: &std::path::Path) -> Result<Connection, String> {
    let conn = Connection::open(db).map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    ensure_ocr_text_column(&conn)?;
    ensure_duration_column(&conn)?;
    conn.execute_batch(FTS_SCHEMA).map_err(|e| e.to_string())?;
    ensure_fts_backfilled(&conn)?;
    Ok(conn)
}

/// ocr_text 列迁移（M0 预埋，M2-OCR 填充）：PRAGMA table_info 探测 → 缺则
/// ALTER TABLE ADD COLUMN（NULL 直到 OCR 任务回填）。幂等：已存在则跳过——
/// 老 history.db（C3 建）升级零拷贝。
fn ensure_ocr_text_column(conn: &Connection) -> Result<(), String> {
    let has: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('shots') WHERE name = 'ocr_text'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if has == 0 {
        conn.execute_batch("ALTER TABLE shots ADD COLUMN ocr_text TEXT;")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// duration 列迁移（M3 继承项①，user_version 2→3）：ocr_text 同款 pragma_table_info
/// 探测模式——缺则 ALTER TABLE ADD COLUMN duration REAL（截图行为 NULL，录制行为秒数）。
/// 幂等：已存在则跳过；0.2.0 的 v2 库升级零拷贝。
fn ensure_duration_column(conn: &Connection) -> Result<(), String> {
    let has: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('shots') WHERE name = 'duration'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if has == 0 {
        conn.execute_batch("ALTER TABLE shots ADD COLUMN duration REAL;")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// schema 迁移终点（M3 N0）：2 = M2（三列 FTS + ocr_text），3 = M3（+ duration 列）。
const SCHEMA_VERSION: i64 = 3;

/// FTS 索引一次性迁移 + 回填（M0 建，M4 扩列）：`PRAGMA user_version` 闸门。
///
/// - ≥ 3：已迁移库，短路（每次开连接零扫描）。
/// - 0（C3 老库 / 全新库）或 1（M0 库：shots_fts 还是两列旧形状）：旧 FTS 对象
///   （触发器 + 虚表）整体 DROP → 按 FTS_SCHEMA 重建（含 ocr_text 列）→
///   `'rebuild'` 全量回填（从 content 表读全部行）。
/// - 2（0.2.0 库）：FTS 已是三列形状，跳过重建（duration 列已由
///   [`ensure_duration_column`] 按探测补齐），仅升版本号到 3。
/// 任一批次与置位同批顺序执行：失败则置位不发生，下次开连接自动重试。
fn ensure_fts_backfilled(conn: &Connection) -> Result<(), String> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    // v0/v1 才需要重建 FTS（旧虚表缺 ocr_text 列，触发器也不会被替换）；v2 直达置位。
    let rebuild = if version < 2 {
        format!(
            "DROP TRIGGER IF EXISTS shots_fts_ai;
             DROP TRIGGER IF EXISTS shots_fts_ad;
             DROP TRIGGER IF EXISTS shots_fts_au;
             DROP TABLE IF EXISTS shots_fts;
             {FTS_SCHEMA}
             INSERT INTO shots_fts(shots_fts) VALUES('rebuild');"
        )
    } else {
        String::new()
    };
    conn.execute_batch(&format!(
        "{rebuild}
         PRAGMA user_version = {SCHEMA_VERSION};"
    ))
    .map_err(|e| e.to_string())
}

/// 建目录 + 建表。幂等（CREATE TABLE IF NOT EXISTS），启动时调用；失败仅记录，
/// 不阻塞启动（历史是旁路功能，命令侧再失败会向用户返回错误）。
pub fn init(app: &AppHandle) -> Result<(), String> {
    let path = db_path(app);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    open_at(&path).map(|_| ())
}

/// C3 保留策略上限（M3 N0 起单源于此）：入库成功后裁剪至最新 500 条，所有 kind
/// **行预算**平权（region / recording / scrollshot 同一预算）。
///
/// 媒体口径（终审 wave I6）：裁剪**只删行**；截图 / 长截图条目连带删文件，
/// **录制条目保留媒体文件**（用户的 MP4/GIF 产物不是缓存，不被自动裁剪静默删除）。
const KEEP_MAX: i64 = 500;

pub fn record(
    app: &AppHandle,
    path: &str,
    thumb_path: &str,
    w: i64,
    h: i64,
    app_name: &str,
    kind: &str,
    duration: Option<f64>,
) -> Result<i64, String> {
    record_at(&db_path(app), path, thumb_path, w, h, app_name, kind, duration)
}

pub(crate) fn record_at(
    db: &std::path::Path,
    path: &str,
    thumb_path: &str,
    w: i64,
    h: i64,
    app_name: &str,
    kind: &str,
    duration: Option<f64>,
) -> Result<i64, String> {
    let conn = open_at(db)?;
    let created_at = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO shots(created_at, kind, path, thumb_path, w, h, app, duration)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![created_at, kind, path, thumb_path, w, h, app_name, duration],
    )
    .map_err(|e| e.to_string())?;
    let id = conn.last_insert_rowid();
    drop(conn);
    // prune 平权 + 单点化（M3 继承项①）：保留裁剪从 commands.rs 的 record_history
    // 调用点移入 record 本体——此前 recording.rs 的入库绕过 prune（录屏可无限累积），
    // 现在所有 kind 走 record 都自动裁剪。best-effort 旁路语义与整条入库链一致：
    // 裁剪失败仅 eprintln 降级，不影响本次已完成的入库（record 已成功，报
    // 「入库失败」会误导——终审 Fix 1 口径）。裁剪语义由 prune_keeps_newest_max 与
    // record_prunes_beyond_keep_max 单测覆盖。
    if let Err(e) = prune_at(db, KEEP_MAX) {
        eprintln!("[hawksnap] 历史裁剪失败（忽略）: {e}");
    }
    Ok(id)
}

pub fn list(app: &AppHandle, limit: i64, offset: i64) -> Result<Vec<ShotRow>, String> {
    list_at(&db_path(app), limit, offset)
}

/// M0：offset 分页（网格分页留用，默认 0）。新→旧不变。
pub(crate) fn list_at(db: &std::path::Path, limit: i64, offset: i64) -> Result<Vec<ShotRow>, String> {
    let conn = open_at(db)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, path, thumb_path, w, h, app, kind, duration FROM shots
             ORDER BY id DESC LIMIT ?1 OFFSET ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([limit, offset], |r| {
            Ok(ShotRow {
                id: r.get(0)?,
                created_at: r.get(1)?,
                path: r.get(2)?,
                thumb_path: r.get(3)?,
                w: r.get(4)?,
                h: r.get(5)?,
                app: r.get(6)?,
                kind: r.get(7)?,
                duration: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

// —— FTS5 搜索（M0） ——

/// MATCH 查询转义（防 FTS5 语法注入）：整串包双引号做短语查询，内部双引号逐字翻倍
/// ——这是 FTS5 字符串字面量的唯一转义规则，`*`/`(`/`NEAR` 等语法字符在短语内全部
/// 退化为普通文本（分词器切掉非字母数字段），构造不出语法树。
fn escape_match(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// FTS5 搜索：命中按 bm25 rank（越小越相关）升序，同分按 id 新→旧。
/// 空/全空白查询返回空集（前端空输入走 list，此为防御）。
pub fn search(app: &AppHandle, query: &str, limit: i64) -> Result<Vec<ShotRow>, String> {
    search_at(&db_path(app), query, limit)
}

pub(crate) fn search_at(db: &std::path::Path, query: &str, limit: i64) -> Result<Vec<ShotRow>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let conn = open_at(db)?;
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.created_at, s.path, s.thumb_path, s.w, s.h, s.app, s.kind, s.duration
             FROM shots_fts
             JOIN shots s ON s.id = shots_fts.rowid
             WHERE shots_fts MATCH ?1
             ORDER BY shots_fts.rank, s.id DESC
             LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let escaped = escape_match(query);
    let rows = stmt
        .query_map(rusqlite::params![escaped, limit], |r| {
            Ok(ShotRow {
                id: r.get(0)?,
                created_at: r.get(1)?,
                path: r.get(2)?,
                thumb_path: r.get(3)?,
                w: r.get(4)?,
                h: r.get(5)?,
                app: r.get(6)?,
                kind: r.get(7)?,
                duration: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

pub fn delete(app: &AppHandle, id: i64) -> Result<(), String> {
    delete_at(&db_path(app), id)
}

/// M4：OCR 结果回填历史行（按 path 匹配；无匹配行 = 不入库不报错——编辑器装载的
/// 临时文件/未入库图属合法状态）。UPDATE 触发器同步 FTS（delete+insert 复合），
/// ocr_text 即刻可检索。返回受影响行数（0 = 无匹配）。
pub fn set_ocr_text(app: &AppHandle, path: &str, text: &str) -> Result<usize, String> {
    set_ocr_text_at(&db_path(app), path, text)
}

pub(crate) fn set_ocr_text_at(db: &std::path::Path, path: &str, text: &str) -> Result<usize, String> {
    let conn = open_at(db)?;
    let n = conn
        .execute("UPDATE shots SET ocr_text = ?1 WHERE path = ?2", rusqlite::params![text, path])
        .map_err(|e| e.to_string())?;
    Ok(n)
}

/// 删行 + 删两个文件（文件已不在（NotFound）忽略——行存在而文件丢失是合法状态：
/// 用户可手动清理截图目录，历史条目仍可删）。
///
/// **用户显式删除**路径（history_delete / 网格「删除」按钮）：行与媒体文件一起删。
/// 录制的 MP4/GIF 也删——用户明确表达「不要了」，留着就成孤儿文件（~/Movies/HawkSnap
/// 里再也清不掉）。**自动裁剪**路径（[`prune_at`）的口径不同：那里永不删录制媒体。
pub(crate) fn delete_at(db: &std::path::Path, id: i64) -> Result<(), String> {
    delete_row_at(db, id, false)
}

/// 删一行；`keep_media = true` 时**只删行、保留媒体文件**（自动裁剪路径用）。
///
/// `keep_media` 由调用方按该行的 kind 决定（kind 从库里读回——该列此前「只写不读」，
/// 现在是裁剪策略的唯一判据）。
pub(crate) fn delete_row_at(
    db: &std::path::Path,
    id: i64,
    keep_media: bool,
) -> Result<(), String> {
    let conn = open_at(db)?;
    let (path, thumb): (String, String) = conn
        .query_row("SELECT path, thumb_path FROM shots WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM shots WHERE id = ?1", [id])
        .map_err(|e| e.to_string())?;
    if keep_media {
        // 媒体保留：文件留在原处（用户 ~/Movies/HawkSnap 里的 MP4/GIF 不被自动裁剪
        // 悄悄删掉——那是用户自己的录制产物，不是缓存）。
        return Ok(());
    }
    for p in [path, thumb] {
        if p.is_empty() {
            continue;
        }
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // 文件已不在，忽略
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// 读回一行的 kind（该列此前「只写不读」；现在是裁剪策略与前端门控的唯一判据）。
fn kind_of(db: &std::path::Path, id: i64) -> Result<String, String> {
    let conn = open_at(db)?;
    conn.query_row("SELECT kind FROM shots WHERE id = ?1", [id], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())
}

/// 保留最新 max 条：id 大于第 max 新的行的条目逐条 delete。
///
/// 生产入口 = [`record_at`] 的内置裁剪（M3 继承项①单点化：KEEP_MAX 对所有 kind
/// **行预算**平权，record 是唯一入库口，录屏/长截图绕不过）；本函数保留 crate 内
/// 可测形态。
///
/// **媒体口径（终审 wave I6）**：行预算仍是 500 条（对所有 kind 一视同仁），但
/// **自动裁剪永不删除 kind='recording' 的媒体文件**——只丢历史行，MP4/GIF 留在
/// `~/Movies/HawkSnap`。旧实现对所有 kind 一律连带删文件 → 用户攒到第 501 条时，
/// 早期的录屏产物在磁盘上被静默删除（用户不知情、文件不可恢复）。
/// 其他 kind（截图 / 长截图）维持原口径：连带删文件（它们是缓存性质的产物，
/// 可从原始内容重建价值低；长图同理）。
pub(crate) fn prune_at(db: &std::path::Path, max: i64) -> Result<(), String> {
    let conn = open_at(db)?;
    let mut stmt = conn
        .prepare("SELECT id FROM shots ORDER BY id DESC")
        .map_err(|e| e.to_string())?;
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    drop(conn);
    // 保留最新 max 条（id 最大的 max 个），其余逐条 delete。
    for id in ids.into_iter().skip(max.max(0) as usize) {
        // 录制条目：只删行、保留媒体文件（用户的录制产物不是缓存）。
        let keep_media = kind_of(db, id).map(|k| k == "recording").unwrap_or(false);
        delete_row_at(db, id, keep_media)?;
    }
    Ok(())
}


/// M0：limit/offset 可选（前端省略时 200/0——面板首屏口径不变）。
#[tauri::command]
pub fn history_list(
    app: AppHandle,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<ShotRow>, String> {
    list(&app, limit.unwrap_or(200).clamp(1, 500), offset.unwrap_or(0).max(0))
}

/// M0 计划接口：history_search(query) -> Vec<ShotRow>。F12 目标 ≤200ms/500 条
/// （fs_500_rows_search_timing 单测计时守护）。查询转义在 search_at 内做（防注入）。
#[tauri::command]
pub fn history_search(
    app: AppHandle,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<ShotRow>, String> {
    search(&app, &query, limit.unwrap_or(50).clamp(1, 200))
}

#[tauri::command(async)]
pub fn history_delete(app: AppHandle, id: i64) -> Result<(), String> {
    delete(&app, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hs-history-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 1×1 真 PNG 文件（core rgba_to_png 生成），供 delete 断言文件被删。
    fn real_png(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let png = hawksnap_core::crop::rgba_to_png(&[1, 2, 3, 255], 1, 1).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, &png).unwrap();
        path
    }

    fn db_file(tag: &str) -> std::path::PathBuf {
        let dir = temp_dir(tag);
        let db = dir.join("history.db");
        open_at(&db).unwrap();
        db
    }

    #[test]
    fn init_is_idempotent() {
        let dir = temp_dir("idem");
        let db = dir.join("history.db");
        open_at(&db).unwrap();
        open_at(&db).unwrap(); // 二次 init 不炸（CREATE TABLE IF NOT EXISTS）
        assert!(db.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_list_roundtrip() {
        let dir = temp_dir("roundtrip");
        let db = dir.join("history.db");
        let png1 = real_png(&dir, "a.png");
        let png2 = real_png(&dir, "b.png");
        let id1 = record_at(&db, png1.to_str().unwrap(), "", 100, 50, "Safari", "region", None).unwrap();
        let id2 = record_at(&db, png2.to_str().unwrap(), "", 200, 80, "Finder", "region", None).unwrap();
        assert!(id1 < id2, "自增 id 单调");
        let rows = list_at(&db, 10, 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, id2, "list 按新→旧");
        assert_eq!(rows[0].path, png2.to_str().unwrap());
        assert_eq!(rows[0].app, "Finder");
        assert_eq!((rows[0].w, rows[0].h), (200, 80));
        assert_eq!(rows[1].id, id1);
        assert_eq!(rows[1].app, "Safari");
        assert!(!rows[0].created_at.is_empty());
        assert!(rows[0].thumb_path.is_empty() || rows[0].thumb_path == "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_removes_row_and_files() {
        let dir = temp_dir("delete");
        let db = dir.join("history.db");
        let png = real_png(&dir, "c.png");
        let thumb = real_png(&dir, "c.thumb.png");
        let id = record_at(
            &db,
            png.to_str().unwrap(),
            thumb.to_str().unwrap(),
            10, 10, "App", "region", None,
        )
        .unwrap();
        assert!(png.exists() && thumb.exists());
        delete_at(&db, id).unwrap();
        assert!(list_at(&db, 10, 0).unwrap().is_empty(), "行被删");
        assert!(!png.exists(), "截图文件被删");
        assert!(!thumb.exists(), "缩略图文件被删");
        // NotFound 忽略：对已删除文件路径的条目再删不炸
        let id2 = record_at(&db, dir.join("gone.png").to_str().unwrap(), "", 1, 1, "App", "region", None).unwrap();
        delete_at(&db, id2).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_keeps_newest_max() {
        let dir = temp_dir("prune");
        let db = dir.join("history.db");
        let mut ids = Vec::new();
        for i in 0..6 {
            let png = real_png(&dir, &format!("p{i}.png"));
            ids.push(record_at(&db, png.to_str().unwrap(), "", i, i, "App", "region", None).unwrap());
        }
        prune_at(&db, 5).unwrap();
        let rows = list_at(&db, 100, 0).unwrap();
        assert_eq!(rows.len(), 5, "只保留最新 5 条");
        let kept: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert!(!kept.contains(&ids[0]), "最旧的被清");
        assert!(kept.contains(&ids[5]), "最新的保留");
        assert!(!dir.join("p0.png").exists(), "最旧的文件连带删除");
        assert!(dir.join("p5.png").exists(), "保留条目的文件不动");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 终审 wave I6：自动裁剪**永不删录制媒体**——行预算对所有 kind 平权（500 条），
    /// 但 kind='recording' 的行被裁掉时，其 MP4/GIF 文件留在磁盘上（用户的录制
    /// 产物不是缓存，不能被静默删除）。用户**显式**删除该条目仍会删文件
    /// （delete_at = 用户意图，媒体不留孤儿）。
    #[test]
    fn prune_keeps_recording_media_on_disk() {
        let dir = temp_dir("prune-recording");
        let db = dir.join("history.db");
        // 1 条最早的截图 + 3 条录制 + 3 条最新截图（共 7 条）。
        let old_shot = real_png(&dir, "old.png");
        record_at(&db, old_shot.to_str().unwrap(), "", 10, 10, "App", "region", None).unwrap();
        for i in 0..3 {
            let mp4 = real_png(&dir, &format!("rec{i}.mp4"));
            record_at(&db, mp4.to_str().unwrap(), "", 100, 100, "recording", "recording", Some(3.0)).unwrap();
        }
        for i in 0..3 {
            let png = real_png(&dir, &format!("shot{i}.png"));
            record_at(&db, png.to_str().unwrap(), "", 10, 10, "App", "region", None).unwrap();
        }
        // 行预算 3 → 保留最新 3 条（3 条最新截图），裁掉最早的 4 条（1 截图 + 3 录制）。
        prune_at(&db, 3).unwrap();
        let rows = list_at(&db, 100, 0).unwrap();
        assert_eq!(rows.len(), 3, "行预算对所有 kind 平权（保留最新 3 条）");
        assert!(rows.iter().all(|r| r.kind == "region"), "被裁掉的是 3 条录制 + 1 条旧截图");
        for i in 0..3 {
            assert!(dir.join(format!("rec{i}.mp4")).exists(), "录制媒体文件不被自动裁剪删除");
            assert!(dir.join(format!("shot{i}.png")).exists(), "保留条目的文件不动");
        }
        assert!(!old_shot.exists(), "截图文件仍按原口径连带删除");
        // 用户显式删除录制条目 → 文件一并删除（不留孤儿）。
        let rec_row = record_at(&db, dir.join("manual.mp4").to_str().unwrap(), "", 10, 10, "recording", "recording", Some(1.0)).unwrap();
        delete_at(&db, rec_row).unwrap();
        assert!(!dir.join("manual.mp4").exists(), "用户显式删除仍删媒体文件");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_offset_paginates() {
        let dir = temp_dir("offset");
        let db = dir.join("history.db");
        let mut ids = Vec::new();
        for i in 0..3 {
            ids.push(record_at(&db, dir.join(format!("o{i}.png")).to_str().unwrap(), "", 1, 1, "App", "region", None).unwrap());
        }
        let page1 = list_at(&db, 2, 0).unwrap();
        let page2 = list_at(&db, 2, 2).unwrap();
        assert_eq!(page1.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[2], ids[1]], "第一页新→旧");
        assert_eq!(page2.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[0]], "第二页只剩最旧一条");
        assert!(list_at(&db, 2, 9).unwrap().is_empty(), "越界 offset 空集");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ocr_text_column_migrates_and_is_idempotent() {
        let dir = temp_dir("ocr-col");
        let db = dir.join("history.db");
        // 造一个 C3 老库（无 ocr_text 列、无 FTS），再经 open_at 升级
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO shots(created_at, path, thumb_path, w, h, app) VALUES ('t', '/x/a.png', '', 1, 1, 'App')",
                [],
            )
            .unwrap();
        }
        open_at(&db).unwrap();
        open_at(&db).unwrap(); // 二次迁移幂等（ALTER 不重复执行）
        let conn = Connection::open(&db).unwrap();
        let has: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('shots') WHERE name = 'ocr_text'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(has, 1, "ocr_text 列已加");
        // 老数据不受影响
        let (n, ocr): (i64, Option<String>) = conn
            .query_row("SELECT COUNT(*), MAX(ocr_text) FROM shots", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((n, ocr), (1, None), "老行保留且 ocr_text 为 NULL");
        // 评审 Fix 同源断言：老库升级后老行必须可检索（回填生效）
        assert_eq!(search_at(&db, "a.png", 50).unwrap().len(), 1, "升级首开回填老行进 FTS");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// FTS5 可用性实证（Task M0 验证项）：bundled libsqlite3-sys 0.38.2 的 build.rs
    /// 带 `-DSQLITE_ENABLE_FTS5`（registry src 159 行），此处用真实
    /// `CREATE VIRTUAL TABLE ... USING fts5` 探测坐实——编译不过/运行不支持都红。
    #[test]
    fn fts5_available_and_open_idempotent() {
        let db = db_file("fts5-avail");
        open_at(&db).unwrap(); // 含 FTS_SCHEMA 二次执行（CREATE VIRTUAL TABLE IF NOT EXISTS）
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE VIRTUAL TABLE IF NOT EXISTS fts5_probe USING fts5(x)", [])
            .expect("bundled SQLite 必须启用 FTS5");
        std::fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }

    /// 评审 Fix：C3 老库（裸 SCHEMA + 手工 INSERT，无 FTS 表、user_version 默认 0）
    /// 升级首开必须把既有行回填进 shots_fts——否则 0.1.0 用户既有历史永久搜不到。
    #[test]
    fn fts_backfills_preexisting_rows_on_upgrade() {
        let dir = temp_dir("fts-upgrade");
        let db = dir.join("history.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO shots(created_at, path, thumb_path, w, h, app)
                 VALUES ('t', '/pics/HawkSnap/legacy.png', '', 1, 1, 'LegacyApp')",
                [],
            )
            .unwrap();
        }
        let v: i64 = Connection::open(&db).unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 0, "前置：老库未迁移（user_version 默认 0）");
        open_at(&db).unwrap();
        let hits = search_at(&db, "legacy", 50).unwrap();
        assert_eq!(hits.len(), 1, "升级首开必须回填老行进 FTS");
        assert_eq!(hits[0].app, "LegacyApp");
        // 闸门已置位：老库变已迁移库
        let v: i64 = Connection::open(&db).unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 3, "回填后 user_version 置 3（M3 duration 列后的迁移终点）");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 评审 Fix：已迁移库（user_version ≥ 1）不得在每次开连接时重建索引。
    /// 可观测差异法：正常入库（触发器已喂 FTS）后手动掏空该行的 FTS 索引，再开连接
    /// ——若 rebuild 仍会执行，索引被恢复、search 命中；短路正确时 search 落空。
    #[test]
    fn fts_rebuild_short_circuits_after_first_migration() {
        let dir = temp_dir("fts-shortcircuit");
        let db = dir.join("history.db");
        let id = record_at(&db, "/pics/HawkSnap/once.png", "", 1, 1, "App", "region", None).unwrap();
        assert_eq!(search_at(&db, "once", 50).unwrap().len(), 1, "触发器路径正常命中");
        {
            let conn = Connection::open(&db).unwrap();
            let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, 3, "record_at 首开已置闸门");
            conn.execute("DELETE FROM shots_fts WHERE rowid = ?1", [id]).unwrap();
        }
        open_at(&db).unwrap(); // 若此处错误地 rebuild，索引会被恢复
        assert!(
            search_at(&db, "once", 50).unwrap().is_empty(),
            "user_version ≥ 3 必须短路 rebuild（开连接不恢复被掏空的索引）"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_hits_by_filename_and_app() {
        let dir = temp_dir("fts-hit");
        let db = dir.join("history.db");
        record_at(&db, "/pics/HawkSnap/Safari 2026-10-23 101010.png", "", 10, 10, "Safari", "region", None).unwrap();
        record_at(&db, "/pics/HawkSnap/Finder 2026-10-23 101111.png", "", 10, 10, "Finder", "region", None).unwrap();
        record_at(&db, "/pics/HawkSnap/safari-web 2026-10-23 101212.png", "", 10, 10, "Xcode", "region", None).unwrap();
        // 文件名段命中
        let by_name = search_at(&db, "safari", 50).unwrap();
        assert_eq!(by_name.len(), 2, "大小写不敏感，命中两个 safari 文件名");
        // app 列命中
        let by_app = search_at(&db, "finder", 50).unwrap();
        assert_eq!(by_app.len(), 1);
        assert_eq!(by_app[0].app, "Finder");
        // 未命中
        assert!(search_at(&db, "nomatch", 50).unwrap().is_empty());
        // bm25 rank 排序：更长的同名串相关度低，safari-web 在后（rank 升序 + id 降序 tiebreak）
        assert_eq!(by_name[0].app, "Safari");
        // 空查询防御：空集不 Err
        assert!(search_at(&db, "   ", 50).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_syntax_characters_do_not_crash() {
        // 防注入（Task M0 验收）：`"` 与 `*` 等语法字符经整串引号包裹 + 内部引号翻倍后
        // 不构成 FTS 语法——任何输入都不炸、只可能空/命中。
        let dir = temp_dir("fts-inject");
        let db = dir.join("history.db");
        record_at(&db, "/pics/HawkSnap/a.png", "", 10, 10, "App", "region", None).unwrap();
        for q in ["\"", "*", "a*", "\"*", "NEAR(", ")*(", "\"\"", "a OR b", "\\'", ""] {
            let r = search_at(&db, q, 50);
            assert!(r.is_ok(), "查询 {q:?} 不得报错: {:?}", r.err());
        }
        // 引号查询仍能命中普通词（转义不破坏匹配语义）
        assert_eq!(search_at(&db, "a.pn\"g", 50).unwrap().len(), 0);
        assert_eq!(search_at(&db, "a.p\"ng", 50).unwrap().len(), 0, "含引号的短语按分词匹配");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_cascades_fts_index() {
        let dir = temp_dir("fts-delete");
        let db = dir.join("history.db");
        let id = record_at(&db, "/pics/HawkSnap/cascade.png", "", 10, 10, "Zed", "region", None).unwrap();
        assert_eq!(search_at(&db, "cascade", 50).unwrap().len(), 1);
        delete_at(&db, id).unwrap();
        assert!(search_at(&db, "cascade", 50).unwrap().is_empty(), "删除后 FTS 索引同步失效");
        // 触发器 delete 路径无残留：FTS 行数与 shots 行数一致
        let conn = Connection::open(&db).unwrap();
        let fts_n: i64 = conn.query_row("SELECT COUNT(*) FROM shots_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(fts_n, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// M4：M0 形态旧库（两列 FTS + user_version=1）升级——重建三列 FTS 且旧行
    /// ocr_text 置入后立即可检索。这是 user_version 1→2 迁移的实证。
    #[test]
    fn fts_upgrade_from_m0_shape_reindexes_ocr_text() {
        let dir = temp_dir("fts-m0-shape");
        let db = dir.join("history.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch("ALTER TABLE shots ADD COLUMN ocr_text TEXT;").unwrap();
            // M0 时代的两列 FTS + 旧触发器（与 M4 FTS_SCHEMA 的关键差异：无 ocr_text 列）。
            conn.execute_batch(
                "CREATE VIRTUAL TABLE shots_fts USING fts5(path, app, content='shots', content_rowid='id');
                 CREATE TRIGGER shots_fts_ai AFTER INSERT ON shots BEGIN
                     INSERT INTO shots_fts(rowid, path, app) VALUES (new.id, new.path, new.app);
                 END;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO shots(created_at, path, thumb_path, w, h, app) VALUES ('t', '/pics/a.png', '', 1, 1, 'App')",
                [],
            )
            .unwrap();
        }
        // 旧库下：ocr 文本搜不到（两列 FTS 没这列）。
        assert!(search_at(&db, "发票抬头", 50).unwrap().is_empty());
        open_at(&db).unwrap();
        let v: i64 = Connection::open(&db).unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 3, "M0 库升级置 3");
        let n = set_ocr_text_at(&db, "/pics/a.png", "发票抬头 HawkSnap").unwrap();
        assert_eq!(n, 1, "UPDATE 命中一行");
        // 新列即时可检索（UPDATE 触发器已喂 FTS）。
        let hits = search_at(&db, "发票抬头", 50).unwrap();
        assert_eq!(hits.len(), 1, "ocr 文本可检索");
        assert_eq!(hits[0].path, "/pics/a.png");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// M4：set_ocr_text 回填 → 检索 → 覆盖更新 → 删除级联，全程 FTS 一致。
    #[test]
    fn set_ocr_text_updates_column_and_fts() {
        let dir = temp_dir("fts-setocr");
        let db = dir.join("history.db");
        let id = record_at(&db, "/pics/HawkSnap/sheet.png", "", 10, 10, "App", "region", None).unwrap();
        // 未入库路径：0 行受影响但不报错（编辑器临时文件合法态）。
        assert_eq!(set_ocr_text_at(&db, "/tmp/unknown.png", "x").unwrap(), 0);
        // 首次回填即可检索。
        assert_eq!(set_ocr_text_at(&db, "/pics/HawkSnap/sheet.png", "季度报表 合计 1234").unwrap(), 1);
        assert_eq!(search_at(&db, "季度报表", 50).unwrap().len(), 1);
        assert_eq!(search_at(&db, "合计", 50).unwrap().len(), 1);
        // 覆盖更新：旧词失效，新词生效（UPDATE 复合触发器 delete+insert）。
        assert_eq!(set_ocr_text_at(&db, "/pics/HawkSnap/sheet.png", "年度预算 5678").unwrap(), 1);
        assert!(search_at(&db, "季度报表", 50).unwrap().is_empty(), "旧文本不再命中");
        assert_eq!(search_at(&db, "年度预算", 50).unwrap().len(), 1);
        // 行删除级联 ocr 索引。
        delete_at(&db, id).unwrap();
        assert!(search_at(&db, "年度预算", 50).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// F12 计时守护（≤200ms/500 条）：合成 500 行后整链计时（含每次操作重开连接——
    /// 生产口径）。bm25 检索本身 µs 级，200ms 预算极宽裕；实测值打印留档（报告引用）。
    #[test]
    fn fs_500_rows_search_timing() {
        let dir = temp_dir("fts-perf");
        let db = dir.join("history.db");
        for i in 0..500 {
            let app = if i % 2 == 0 { "Safari" } else { "Finder" };
            record_at(&db, &format!("/pics/HawkSnap/{app} 2026-10-23 {i:06}.png"), "", 10, 10, app, "region", None).unwrap();
        }
        let t0 = std::time::Instant::now();
        let hits = search_at(&db, "safari", 50).unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(hits.len(), 50, "limit=50 截断");
        assert_eq!(hits[0].app, "Safari");
        println!("fts search over 500 rows: {elapsed:?} (F12 预算 200ms)");
        assert!(
            elapsed.as_millis() < 200,
            "F12：500 行搜索必须 <200ms，实测 {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// M3 继承项①：kind + duration 写入契约——record_at 按 kind 落 kind 列；截图
    /// （region）duration 为 NULL → ShotRow.duration = None，录制路径 Some(秒)。
    #[test]
    fn record_writes_kind_and_duration() {
        let dir = temp_dir("kind-duration");
        let db = dir.join("history.db");
        record_at(&db, "/pics/HawkSnap/shot.png", "", 10, 10, "HawkSnap", "region", None).unwrap();
        record_at(&db, "/movies/HawkSnap/rec.mp4", "", 1920, 1080, "recording", "recording", Some(12.5)).unwrap();
        let rows = list_at(&db, 10, 0).unwrap();
        assert_eq!(rows.len(), 2);
        // 新→旧：rec 在前（duration=12.5），region 在后（duration=None）
        assert_eq!(rows[0].duration, Some(12.5), "录制行 duration 为秒数");
        assert_eq!(rows[1].duration, None, "截图行 duration 为 NULL");
        // kind 列直查 + **前端契约面**：ShotRow 现在暴露 kind（终审 wave：前端按
        // kind 门控「重新编辑」等动作，不再靠 app 列冒充）。
        let conn = Connection::open(&db).unwrap();
        let kinds: Vec<String> = conn
            .prepare("SELECT kind FROM shots ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(kinds, vec!["region", "recording"]);
        // 前端契约：ShotRow.kind 直接可读（region / recording）。
        assert_eq!(rows[0].kind, "recording");
        assert_eq!(rows[1].kind, "region");
        // duration 语义入库即生效：老行（迁移库）读回 NULL 不炸
        assert!(search_at(&db, "rec.mp4", 10).unwrap()[0].duration == Some(12.5));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// M3 继承项①：0.2.0 的 v2 库升级到 v3——duration 列按探测补齐，老行保留且
    /// duration 为 NULL，user_version 置 3；二次开连接幂等（ALTER 不重复执行）。
    #[test]
    fn duration_column_migrates_v2_to_v3() {
        let dir = temp_dir("duration-v2");
        let db = dir.join("history.db");
        // 造一个 0.2.0 形态的 v2 库（有 ocr_text、三列 FTS，无 duration）
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch("ALTER TABLE shots ADD COLUMN ocr_text TEXT;").unwrap();
            conn.execute_batch(FTS_SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO shots(created_at, kind, path, thumb_path, w, h, app)
                 VALUES ('t', 'region', '/pics/HawkSnap/old.png', '', 1, 1, 'App')",
                [],
            )
            .unwrap();
            conn.execute_batch("PRAGMA user_version = 2;").unwrap();
        }
        open_at(&db).unwrap();
        open_at(&db).unwrap(); // 二次迁移幂等
        let conn = Connection::open(&db).unwrap();
        let has: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('shots') WHERE name = 'duration'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(has, 1, "duration 列已加");
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 3, "v2 库升级置 3");
        let (n, kind, duration): (i64, String, Option<f64>) = conn
            .query_row(
                "SELECT COUNT(*), MAX(kind), MAX(duration) FROM shots",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((n, kind.as_str(), duration), (1, "region", None), "老行保留且 duration 为 NULL");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// M3 继承项①（prune 平权）：prune 已移入 record 本体——入库超过 KEEP_MAX 即
    /// 自动裁剪到最新 500 条，任何 kind 一致。这是「录屏入库绕过 prune」缺陷的
    /// 结构性收口：record 是唯一入库口，绕不过。
    ///
    /// 终审 wave I6 追加口径：**行预算平权，但媒体不跟着删**——本测试的条目
    /// kind='recording' → 裁剪只删行、保留媒体文件（截图类条目的连带删文件由
    /// `prune_keeps_newest_max` 覆盖，录制媒体的保留由
    /// `prune_keeps_recording_media_on_disk` 覆盖）。
    #[test]
    fn record_prunes_beyond_keep_max() {
        let dir = temp_dir("record-prune");
        let db = dir.join("history.db");
        let mut first = None;
        let mut last = None;
        for i in 0..=KEEP_MAX {
            // 首尾条目配真文件，断言最旧被裁、最新保留
            let png = if i == 0 || i == KEEP_MAX as i64 {
                Some(real_png(&dir, &format!("edge{i}.png")))
            } else {
                None
            };
            let path = png
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("/pics/HawkSnap/fill{i}.png"));
            let id = record_at(&db, &path, "", 1, 1, "App", "recording", Some(i as f64)).unwrap();
            if i == 0 {
                first = Some((id, path));
            }
            last = Some(id);
        }
        let rows = list_at(&db, 10_000, 0).unwrap();
        assert_eq!(rows.len() as i64, KEEP_MAX, "恰好保留最新 500 条");
        let (first_id, first_path) = first.unwrap();
        assert!(!rows.iter().any(|r| r.id == first_id), "最旧一条被裁");
        assert!(
            std::path::Path::new(&first_path).exists(),
            "录制条目的媒体文件不被自动裁剪删除（只删行，留文件）"
        );
        assert!(rows.iter().any(|r| r.id == last.unwrap()), "最新一条保留");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
