///! SQLite persistence: monitor configuration, application registry, directory ledger.

use crate::error::{Error, Result};
use crate::path;
use crate::types::{DirCreateRecord, MonitorRoot, RecordStatus, DbPath};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub const SCHEMA_VERSION: i64 = 3;

#[derive(Debug, Clone)]
pub struct AppRow {
    pub app_id: i64,
    pub exe_path: DbPath,
    pub exe_name: String,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone)]
pub struct DirRow {
    pub directory_id: i64,
    pub app_id: i64,
    pub directory_path: DbPath,
    pub monitor_root: DbPath,
    pub creator_pid: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub record_status: RecordStatus,
    pub capture_source: String,
    pub attribution_confidence: String,
}

#[derive(Debug, Clone)]
pub struct IgnoredRow {
    pub observed_at_ms: i64,
    pub reason: String,
    pub directory_path: DbPath,
    pub creator_pid: u32,
    pub exe_path: Option<DbPath>,
}

/// 注册表账本中的一条记录 (对应 registry_events 行)。
#[derive(Debug, Clone)]
pub struct RegRow {
    pub reg_id: i64,
    pub app_id: i64,
    pub pid: u32,
    pub op: String,
    pub key_path: String,
    pub value_name: String,
    pub event_at_ms: i64,
}

/// 注册表账本按 exe 聚合的概览行。
#[derive(Debug, Clone)]
pub struct RegAppSummary {
    pub app_id: i64,
    pub exe_path: String,
    pub exe_name: String,
    pub event_count: i64,
}

/// 符号链接账本中的一条记录 (GUI 符号链接页签创建时写入)。
#[derive(Debug, Clone)]
pub struct SymlinkRecord {
    pub link_path: DbPath,
    pub target_path: DbPath,
    pub created_at_ms: i64,
}

/// One connection, guarded so the daemon thread and the CLI can share it.
pub struct Db {
    conn: Mutex<Connection>,
}

const DDL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS monitor_roots (
    root_id       INTEGER PRIMARY KEY,
    root_path     TEXT NOT NULL UNIQUE,
    root_key      TEXT NOT NULL UNIQUE,
    enabled       INTEGER NOT NULL DEFAULT 1,
    added_at_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS applications (
    app_id        INTEGER PRIMARY KEY,
    exe_path      TEXT NOT NULL UNIQUE,
    exe_key       TEXT NOT NULL UNIQUE,
    exe_name      TEXT NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS directories (
    directory_id   INTEGER PRIMARY KEY,
    app_id         INTEGER NOT NULL REFERENCES applications(app_id),
    directory_path TEXT NOT NULL UNIQUE,
    dir_key        TEXT NOT NULL UNIQUE,
    monitor_root   TEXT NOT NULL,
    creator_pid    INTEGER NOT NULL,
    capture_source TEXT NOT NULL DEFAULT 'sim',
    confidence     TEXT NOT NULL DEFAULT 'scripted-exact',
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL,
    record_status  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS symlink_records (
    record_id     INTEGER PRIMARY KEY,
    link_path     TEXT NOT NULL UNIQUE,
    target_path   TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_directories_app    ON directories(app_id);
CREATE INDEX IF NOT EXISTS idx_directories_status ON directories(record_status);
CREATE INDEX IF NOT EXISTS idx_applications_name  ON applications(exe_name);

CREATE TABLE IF NOT EXISTS ignored_events (
    ignored_id      INTEGER PRIMARY KEY,
    observed_at_ms  INTEGER NOT NULL,
    reason          TEXT NOT NULL,
    directory_path  TEXT NOT NULL,
    creator_pid     INTEGER NOT NULL,
    exe_path        TEXT
);

CREATE INDEX IF NOT EXISTS idx_ignored_reason ON ignored_events(reason);

CREATE TABLE IF NOT EXISTS registry_events (
    reg_id        INTEGER PRIMARY KEY,
    app_id        INTEGER NOT NULL REFERENCES applications(app_id) ON DELETE CASCADE,
    pid           INTEGER NOT NULL,
    op            TEXT NOT NULL,
    key_path      TEXT NOT NULL,
    value_name    TEXT NOT NULL,
    event_at_ms   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_reg_app ON registry_events(app_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_reg_dedup
    ON registry_events(app_id, op, key_path, value_name);
"#;

impl Db {
    pub fn open(file: &Path) -> Result<Db> {
        let conn = Connection::open(file).map_err(|e| Error::Sqlite(e.to_string()))?;
        conn.execute_batch(DDL)
            .map_err(|e| Error::Sqlite(format!("schema init failed: {e}")))?;
        Self::migrate(&conn)?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )
        .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Sqlite(e.to_string()))?;
        conn.execute_batch(DDL)
            .map_err(|e| Error::Sqlite(format!("schema init failed: {e}")))?;
        Self::migrate(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// Migration for databases created before source/confidence columns existed. Old rows
    /// were all written by the sim source, so the backfill values match their provenance.
    fn migrate(conn: &Connection) -> Result<()> {
        let has_source: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('directories') WHERE name = 'capture_source'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        if has_source == 0 {
            conn.execute_batch(
                "ALTER TABLE directories ADD COLUMN capture_source TEXT NOT NULL DEFAULT 'sim';
                 ALTER TABLE directories ADD COLUMN confidence TEXT NOT NULL DEFAULT 'scripted-exact';",
            )
            .map_err(|e| Error::Sqlite(format!("schema migration failed: {e}")))?;
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---------- monitor roots ----------

    /// Add a watch root. Returns false when it was already configured.
    pub fn add_root(&self, raw: &str, now_ms: i64) -> Result<(MonitorRoot, bool)> {
        let norm = path::normalize(raw).ok_or_else(|| Error::InvalidPath(raw.to_string()))?;
        let key = norm.to_ascii_lowercase();
        let db = self.lock();
        let changed = db
            .execute(
                "INSERT INTO monitor_roots(root_path, root_key, enabled, added_at_ms)
                 VALUES(?1, ?2, 1, ?3) ON CONFLICT(root_key) DO NOTHING",
                params![norm, key, now_ms],
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let row = db
            .query_row(
                "SELECT root_path, enabled, added_at_ms FROM monitor_roots WHERE root_key = ?1",
                params![key],
                |r| {
                    Ok(MonitorRoot {
                        path: r.get(0)?,
                        enabled: r.get::<_, i64>(1)? != 0,
                        added_at_ms: r.get(2)?,
                    })
                },
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok((row, changed == 1))
    }

    // ---- 符号链接记录 (GUI 符号链接页签) ----

    /// 记录一次符号链接创建; 同一 link_path 重复创建时覆盖旧记录。
    pub fn add_symlink_record(&self, link_path: &str, target_path: &str) -> Result<()> {
        let db = self.lock();
        db.execute(
            "INSERT INTO symlink_records(link_path, target_path, created_at_ms) VALUES(?1, ?2, ?3)
             ON CONFLICT(link_path) DO UPDATE SET
               target_path = excluded.target_path,
               created_at_ms = excluded.created_at_ms",
            params![link_path, target_path, crate::timefmt::now_ms()],
        )
        .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(())
    }

    pub fn symlink_records(&self) -> Result<Vec<SymlinkRecord>> {
        let db = self.lock();
        let mut st = db
            .prepare(
                "SELECT link_path, target_path, created_at_ms
                 FROM symlink_records ORDER BY created_at_ms DESC, record_id DESC",
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = st
            .query_map([], |r| {
                Ok(SymlinkRecord {
                    link_path: r.get(0)?,
                    target_path: r.get(1)?,
                    created_at_ms: r.get(2)?,
                })
            })
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))
    }

    /// 删除指定 link_path 的记录, 返回删除行数。
    pub fn delete_symlink_records(&self, links: &[String]) -> Result<usize> {
        let db = self.lock();
        let mut n = 0;
        for l in links {
            n += db
                .execute("DELETE FROM symlink_records WHERE link_path = ?1", params![l])
                .map_err(|e| Error::Sqlite(e.to_string()))?;
        }
        Ok(n)
    }

    pub fn remove_root(&self, raw: &str) -> Result<bool> {
        let key = path::key_of(raw).ok_or_else(|| Error::InvalidPath(raw.to_string()))?;
        let db = self.lock();
        // Ledger rows are kept: history survives configuration changes.
        let n = db
            .execute("DELETE FROM monitor_roots WHERE root_key = ?1", params![key])
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(n == 1)
    }

    pub fn set_root_enabled(&self, raw: &str, enabled: bool) -> Result<bool> {
        let key = path::key_of(raw).ok_or_else(|| Error::InvalidPath(raw.to_string()))?;
        let db = self.lock();
        let n = db
            .execute(
                "UPDATE monitor_roots SET enabled = ?2 WHERE root_key = ?1",
                params![key, enabled as i64],
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(n == 1)
    }

    pub fn roots(&self) -> Result<Vec<MonitorRoot>> {
        let db = self.lock();
        let mut st = db
            .prepare("SELECT root_path, enabled, added_at_ms FROM monitor_roots ORDER BY added_at_ms")
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = st
            .query_map([], |r| {
                Ok(MonitorRoot {
                    path: r.get(0)?,
                    enabled: r.get::<_, i64>(1)? != 0,
                    added_at_ms: r.get(2)?,
                })
            })
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))
    }

    pub fn has_root_key(&self, raw: &str) -> Result<bool> {
        let key = match path::key_of(raw) {
            Some(k) => k,
            None => return Ok(false),
        };
        let db = self.lock();
        db.query_row(
            "SELECT 1 FROM monitor_roots WHERE root_key = ?1",
            params![key],
            |_| Ok(()),
        )
        .optional()
        .map(|o| o.is_some())
        .map_err(|e| Error::Sqlite(e.to_string()))
    }

    // ---------- ledger ----------

    fn upsert_app(&self, rec: &DirCreateRecord) -> Result<i64> {
        let key = rec.exe_path.to_ascii_lowercase();
        let db = self.lock();
        db.execute(
            "INSERT INTO applications(exe_path, exe_key, exe_name, first_seen_ms, last_seen_ms)
             VALUES(?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(exe_key) DO UPDATE SET last_seen_ms = max(excluded.last_seen_ms, applications.last_seen_ms)",
            params![rec.exe_path, key, rec.exe_name, rec.observed_at_ms],
        )
        .map_err(|e| Error::Sqlite(e.to_string()))?;
        db.query_row(
            "SELECT app_id FROM applications WHERE exe_key = ?1",
            params![key],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| Error::Sqlite(e.to_string()))
    }

    pub fn dir_by_path(&self, raw: &str) -> Result<Option<DirRow>> {
        let key = match path::key_of(raw) {
            Some(k) => k,
            None => return Ok(None),
        };
        let db = self.lock();
        db.query_row(
            "SELECT d.directory_id, d.app_id, d.directory_path, d.monitor_root, d.creator_pid,
                    d.created_at_ms, d.updated_at_ms, d.record_status, d.capture_source, d.confidence
               FROM directories d WHERE d.dir_key = ?1",
            params![key],
            DirRow::from_row,
        )
        .optional()
        .map_err(|e| Error::Sqlite(e.to_string()))
    }

    // ---- 注册表账本 (registry_events) ----

    /// 目录账本中所有已归属 exe 的 (exe_key, app_id)。
    /// 注册表采集用它一次性建内存索引, 避免每条事件查一次 SQLite。
    pub fn known_apps(&self) -> Result<Vec<(String, i64)>> {
        let db = self.lock();
        let mut stmt = db
            .prepare("SELECT exe_key, app_id FROM applications")
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| Error::Sqlite(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(rows)
    }

    /// exe 路径是否已被目录监视捕获过 (在 applications 表中); 命中返回 app_id。
    pub fn reg_known_app(&self, exe_path: &str) -> Result<Option<i64>> {
        let lower = exe_path.to_ascii_lowercase();
        let db = self.lock();
        db.query_row(
            "SELECT app_id FROM applications WHERE exe_key = ?1 OR exe_key = ?2",
            params![exe_path, lower],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| Error::Sqlite(e.to_string()))
    }

    /// 插入注册表事件。同 app+op+键+值 视为同一条, 重复写入忽略。
    /// 返回是否新插入了行。
    pub fn reg_insert(
        &self,
        app_id: i64,
        pid: u32,
        op: &str,
        key_path: &str,
        value_name: &str,
        at_ms: i64,
    ) -> Result<bool> {
        let db = self.lock();
        let n = db
            .execute(
                "INSERT OR IGNORE INTO registry_events(app_id, pid, op, key_path, value_name, event_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![app_id, pid, op, key_path, value_name, at_ms],
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(n > 0)
    }

    /// 按 exe 聚合的注册表账本概览。
    pub fn reg_apps(&self) -> Result<Vec<RegAppSummary>> {
        let db = self.lock();
        let mut stmt = db
            .prepare(
                "SELECT a.app_id, a.exe_path, a.exe_name, COUNT(r.reg_id) AS cnt
                   FROM registry_events r JOIN applications a ON a.app_id = r.app_id
                  GROUP BY a.app_id ORDER BY cnt DESC, a.exe_name",
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RegAppSummary {
                    app_id: r.get(0)?,
                    exe_path: r.get(1)?,
                    exe_name: r.get(2)?,
                    event_count: r.get(3)?,
                })
            })
            .map_err(|e| Error::Sqlite(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(rows)
    }

    /// 某 exe 的全部注册表记录。
    pub fn reg_events_of_app(&self, app_id: i64) -> Result<Vec<RegRow>> {
        let db = self.lock();
        let mut stmt = db
            .prepare(
                "SELECT reg_id, app_id, pid, op, key_path, value_name, event_at_ms
                   FROM registry_events WHERE app_id = ?1 ORDER BY event_at_ms DESC, reg_id DESC",
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = stmt
            .query_map(params![app_id], |r| {
                Ok(RegRow {
                    reg_id: r.get(0)?,
                    app_id: r.get(1)?,
                    pid: r.get::<_, i64>(2)? as u32,
                    op: r.get(3)?,
                    key_path: r.get(4)?,
                    value_name: r.get(5)?,
                    event_at_ms: r.get(6)?,
                })
            })
            .map_err(|e| Error::Sqlite(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(rows)
    }

    /// 删除指定 reg_id 的记录, 返回实际删除数。
    pub fn reg_delete_ids(&self, ids: &[i64]) -> Result<usize> {
        let db = self.lock();
        let mut n = 0;
        for id in ids {
            n += db
                .execute("DELETE FROM registry_events WHERE reg_id = ?1", params![id])
                .map_err(|e| Error::Sqlite(e.to_string()))?;
        }
        Ok(n)
    }

    /// 清空某 exe 的全部注册表记录 (不动 applications/directories)。
    pub fn reg_delete_app(&self, app_id: i64) -> Result<usize> {
        let db = self.lock();
        db.execute("DELETE FROM registry_events WHERE app_id = ?1", params![app_id])
            .map_err(|e| Error::Sqlite(e.to_string()))
    }

    pub fn app_by_id(&self, app_id: i64) -> Result<Option<AppRow>> {
        let db = self.lock();
        db.query_row(
            "SELECT app_id, exe_path, exe_name, first_seen_ms, last_seen_ms FROM applications WHERE app_id = ?1",
            params![app_id],
            |r| {
                Ok(AppRow {
                    app_id: r.get(0)?,
                    exe_path: r.get(1)?,
                    exe_name: r.get(2)?,
                    first_seen_ms: r.get(3)?,
                    last_seen_ms: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|e| Error::Sqlite(e.to_string()))
    }

    pub fn set_status(&self, directory_id: i64, status: RecordStatus, now_ms: i64) -> Result<()> {
        let db = self.lock();
        db.execute(
            "UPDATE directories SET record_status = ?2, updated_at_ms = ?3 WHERE directory_id = ?1",
            params![directory_id, status.as_str(), now_ms],
        )
        .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(())
    }

    pub fn log_ignored(&self, reason: &str, rec: &DirCreateRecord, pid: u32, exe: Option<&str>) -> Result<()> {
        let db = self.lock();
        db.execute(
            "INSERT INTO ignored_events(observed_at_ms, reason, directory_path, creator_pid, exe_path)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![rec.observed_at_ms, reason, rec.directory_path, pid, exe],
        )
        .map_err(|e| Error::Sqlite(e.to_string()))?;
        Ok(())
    }

    /// Insert the record fresh, or re-validate an existing path whose directory was
    /// deleted and created again. Never blindly overwrites an active record.
    pub fn insert_or_reregister(&self, rec: &DirCreateRecord) -> Result<InsertOutcome> {
        let dir_key = rec.directory_path.to_ascii_lowercase();
        let app_id = self.upsert_app(rec)?;
        let db = self.lock();
        let existing: Option<(i64, i64, String)> = db
            .query_row(
                "SELECT directory_id, app_id, record_status FROM directories WHERE dir_key = ?1",
                params![dir_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        match existing {
            Some((id, _app, status)) if status == RecordStatus::Active.as_str() => {
                Ok(InsertOutcome::AlreadyRegistered { directory_id: id })
            }
            Some((id, _, _status)) => {
                db.execute(
                    "UPDATE directories
                        SET app_id = ?2, monitor_root = ?3, creator_pid = ?4,
                            created_at_ms = ?5, updated_at_ms = ?6, record_status = 'active',
                            directory_path = ?7
                      WHERE directory_id = ?1",
                    params![id, app_id, rec.monitor_root, rec.creator_pid as i64,
                           rec.created_at_ms, rec.observed_at_ms, rec.directory_path],
                )
                .map_err(|e| Error::Sqlite(e.to_string()))?;
                Ok(InsertOutcome::Reregistered { directory_id: id })
            }
            None => {
                db.execute(
                    "INSERT INTO directories(app_id, directory_path, dir_key, monitor_root, creator_pid,
                                            created_at_ms, updated_at_ms, record_status, capture_source, confidence)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?9)",
                    params![app_id, rec.directory_path, dir_key, rec.monitor_root,
                            rec.creator_pid as i64, rec.created_at_ms, rec.observed_at_ms,
                            rec.capture_source.as_str(), rec.attribution_confidence],
                )
                .map_err(|e| Error::Sqlite(e.to_string()))?;
                let id = db
                    .last_insert_rowid();
                Ok(InsertOutcome::Recorded { directory_id: id })
            }
        }
    }

    /// Directories created by one EXE path, active first, then by path.
    pub fn dirs_of_exe(&self, exe_path: &str, include_stale: bool) -> Result<Vec<DirRow>> {
        let key = path::key_of(exe_path).ok_or_else(|| Error::InvalidPath(exe_path.to_string()))?;
        let db = self.lock();
        let sql = format!(
            "SELECT d.directory_id, d.app_id, d.directory_path, d.monitor_root, d.creator_pid,
                    d.created_at_ms, d.updated_at_ms, d.record_status, d.capture_source, d.confidence
               FROM directories d JOIN applications a ON a.app_id = d.app_id
              WHERE a.exe_key = ?1 {}
           ORDER BY CASE d.record_status WHEN 'active' THEN 0 ELSE 1 END, d.directory_path",
            if include_stale {
                ""
            } else {
                "AND d.record_status = 'active'"
            }
        );
        let mut st = db.prepare(&sql).map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = st
            .query_map(params![key], DirRow::from_row)
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))
    }

    pub fn all_dirs(&self) -> Result<Vec<DirRow>> {
        let db = self.lock();
        let mut st = db
            .prepare(
                "SELECT directory_id, app_id, directory_path, monitor_root, creator_pid,
                        created_at_ms, updated_at_ms, record_status, capture_source, confidence
                   FROM directories ORDER BY directory_path",
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = st
            .query_map([], DirRow::from_row)
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))
    }

    /// 删除账本中的目录记录 (只动 SQLite, 不碰文件系统)。返回删除行数。
    /// 用于清理“原目录已不存在、仅剩账本记录”的失效条目。
    pub fn delete_dir_records(&self, ids: &[i64]) -> Result<usize> {
        let db = self.lock();
        let mut n = 0;
        for id in ids {
            n += db
                .execute("DELETE FROM directories WHERE directory_id = ?1", params![id])
                .map_err(|e| Error::Sqlite(e.to_string()))?;
        }
        Ok(n)
    }

    pub fn ignored_counts(&self) -> Result<Vec<(String, i64)>> {
        let db = self.lock();
        let mut st = db
            .prepare("SELECT reason, COUNT(*) FROM ignored_events GROUP BY reason ORDER BY 2 DESC")
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))
    }

    pub fn recent_ignored(&self, limit: i64) -> Result<Vec<IgnoredRow>> {
        let db = self.lock();
        let mut st = db
            .prepare(
                "SELECT observed_at_ms, reason, directory_path, creator_pid, exe_path
                   FROM ignored_events ORDER BY ignored_id DESC LIMIT ?1",
            )
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        let rows = st
            .query_map(params![limit], |r| {
                Ok(IgnoredRow {
                    observed_at_ms: r.get(0)?,
                    reason: r.get(1)?,
                    directory_path: r.get(2)?,
                    creator_pid: r.get::<_, i64>(3)? as u32,
                    exe_path: r.get(4)?,
                })
            })
            .map_err(|e| Error::Sqlite(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Sqlite(e.to_string()))
    }

    pub fn dir_count(&self, status: Option<RecordStatus>) -> Result<i64> {
        let db = self.lock();
        match status {
            Some(s) => db
                .query_row(
                    "SELECT COUNT(*) FROM directories WHERE record_status = ?1",
                    params![s.as_str()],
                    |r| r.get(0),
                )
                .map_err(|e| Error::Sqlite(e.to_string())),
            None => db
                .query_row("SELECT COUNT(*) FROM directories", [], |r| r.get(0))
                .map_err(|e| Error::Sqlite(e.to_string())),
        }
    }

    pub fn app_count(&self) -> Result<i64> {
        let db = self.lock();
        db.query_row("SELECT COUNT(*) FROM applications", [], |r| r.get(0))
            .map_err(|e| Error::Sqlite(e.to_string()))
    }
}

impl DirRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let status: String = r.get(7)?;
        let capture_source: String = r.get(8)?;
        let attribution_confidence: String = r.get(9)?;
        Ok(DirRow {
            directory_id: r.get(0)?,
            app_id: r.get(1)?,
            directory_path: r.get(2)?,
            monitor_root: r.get(3)?,
            creator_pid: r.get::<_, i64>(4)? as u32,
            created_at_ms: r.get(5)?,
            updated_at_ms: r.get(6)?,
            record_status: RecordStatus::parse(&status).unwrap_or(RecordStatus::NeedsReview),
            capture_source,
            attribution_confidence,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Recorded { directory_id: i64 },
    Reregistered { directory_id: i64 },
    AlreadyRegistered { directory_id: i64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SourceKind;

    fn seed(db: &Db) -> i64 {
        db.add_root(r"C:\appdir-test", 1_000).unwrap();
        let rec = DirCreateRecord {
            directory_path: r"C:\appdir-test\created".to_string(),
            monitor_root: r"C:\appdir-test".to_string(),
            exe_path: r"C:\tools\setup.exe".to_string(),
            exe_name: "setup.exe".to_string(),
            creator_pid: 1234,
            created_at_ms: 2_000,
            observed_at_ms: 2_000,
            capture_source: SourceKind::Sim,
            attribution_confidence: "sim".to_string(),
        };
        match db.insert_or_reregister(&rec).unwrap() {
            InsertOutcome::Recorded { directory_id } => directory_id,
            other => panic!("unexpected insert outcome: {other:?}"),
        }
    }

    /// 删除账本记录只动 SQLite: 目录行消失, 应用行保留 (exe 仍是已知程序)。
    #[test]
    fn delete_dir_records_removes_rows_only() {
        let db = Db::in_memory().unwrap();
        let id = seed(&db);
        assert_eq!(db.all_dirs().unwrap().len(), 1);

        assert_eq!(db.delete_dir_records(&[id]).unwrap(), 1);
        assert!(db.all_dirs().unwrap().is_empty());
        assert!(db.dir_by_path(r"C:\appdir-test\created").unwrap().is_none());
        // 程序条目保留, 仍可被注册表采集当作已知 exe。
        assert!(db.known_apps().unwrap().iter().any(|(_, a)| *a >= 1));
    }

    /// 删除不存在的 id 不报错, 返回 0。
    #[test]
    fn delete_dir_records_missing_id_is_noop() {
        let db = Db::in_memory().unwrap();
        assert_eq!(db.delete_dir_records(&[424242]).unwrap(), 0);
    }

    /// 目录被标记 Missing 后仍然保留在账本里, 直到显式删除记录。
    #[test]
    fn missing_rows_survive_until_record_deleted() {
        let db = Db::in_memory().unwrap();
        let id = seed(&db);
        db.set_status(id, RecordStatus::Missing, 3_000).unwrap();
        let rows = db.all_dirs().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_status, RecordStatus::Missing);

        db.delete_dir_records(&[id]).unwrap();
        assert!(db.all_dirs().unwrap().is_empty());
    }
}
