//! SQLite persistence layer.
//!
//! Lives at `~/Library/Application Support/Helm/helm.db` on macOS (next
//! to the legacy `hosts.json` / `schedules.json` files). Replaces those
//! JSON files for v1; the old files are renamed to `*.pre-sqlite-backup`
//! on the first boot of a build that includes this module.
//!
//! Storage style is *JSON-blob per row* with explicit columns only for
//! the keys we query, order, or index by. The domain types
//! (`Host`, `Notification`, `Schedule`, `ScheduleRun`) carry enums that
//! evolve quickly; pinning every field as a column would force a SQL
//! migration on every domain change. Blobs keep the schema stable and
//! domain evolution as a pure code concern.
//!
//! Dismissal is monotonic: dismissing a notification is a `DELETE`. No
//! tombstones, no soft-delete column.

use std::path::PathBuf;
use std::sync::Arc;

use helm_domain::{Host, HostId, Notification, NotificationId, Schedule, ScheduleId, ScheduleRun};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};

const APP_DIR: &str = "Helm";
const DB_FILE: &str = "helm.db";
const SCHEMA_VERSION: i32 = 1;

/// Cheap-to-clone handle to the single shared SQLite connection. All
/// access serializes on the inner mutex; rusqlite calls are sync and
/// brief, so a single connection is fine for our write volume (a few
/// hundred writes/sec ceiling under the heaviest BEL/CommandDone burst).
#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

pub fn db_path() -> Result<PathBuf, String> {
    let base = dirs::config_dir().ok_or_else(|| "could not locate config dir".to_string())?;
    let dir = base.join(APP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir({:?}): {e}", dir))?;
    Ok(dir.join(DB_FILE))
}

impl Db {
    /// Open (or create) the database, ensure the schema is at the
    /// current version, and apply WAL + foreign-key pragmas. Panics on
    /// open failure — startup can't continue without a working db, and
    /// surfacing this as a Result would just push the panic up to
    /// `run()` for no benefit.
    pub fn open() -> Result<Self, String> {
        let path = db_path()?;
        let conn = Connection::open(&path).map_err(|e| format!("open({:?}): {e}", path))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("set WAL: {e}"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| format!("set foreign_keys: {e}"))?;
        Self::ensure_schema(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory db with the same schema. Used by unit tests
    /// that need a `NotificationsCtx` without touching the user's disk.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        Self::ensure_schema(&conn).expect("schema");
        Self {
            inner: Arc::new(Mutex::new(conn)),
        }
    }

    fn ensure_schema(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);

            CREATE TABLE IF NOT EXISTS hosts (
                id   TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS notifications (
                id         TEXT PRIMARY KEY,
                host_id    TEXT NOT NULL,
                pane_id    TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                data       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_notifications_pane    ON notifications (host_id, pane_id);
            CREATE INDEX IF NOT EXISTS idx_notifications_host    ON notifications (host_id);
            CREATE INDEX IF NOT EXISTS idx_notifications_created ON notifications (created_at);

            CREATE TABLE IF NOT EXISTS schedules (
                id   TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS schedule_runs (
                id          TEXT PRIMARY KEY,
                schedule_id TEXT NOT NULL,
                started_at  INTEGER NOT NULL,
                data        TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_schedule_runs_schedule
                ON schedule_runs (schedule_id, started_at DESC);
            ",
        )
        .map_err(|e| format!("create schema: {e}"))?;

        let current: Option<i32> = conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
            .optional()
            .map_err(|e| format!("read schema_version: {e}"))?;
        if current.is_none() {
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )
            .map_err(|e| format!("seed schema_version: {e}"))?;
        }
        Ok(())
    }

    /// One-shot import of legacy `hosts.json` and `schedules.json`. Runs
    /// only when the hosts and schedules tables are empty AND the JSON
    /// files exist — re-running with a populated db is a no-op so this
    /// is safe to invoke on every boot.
    ///
    /// On success, renames the JSON files to `*.pre-sqlite-backup` so
    /// we don't import them again next boot. Best-effort: if a rename
    /// fails, the import is still committed; the table check on the
    /// next boot keeps us idempotent.
    pub fn migrate_from_json_if_needed(&self) -> Result<(), String> {
        let hosts_empty = self.is_table_empty("hosts")?;
        let schedules_empty = self.is_table_empty("schedules")?;
        if !hosts_empty && !schedules_empty {
            return Ok(());
        }

        let base = dirs::config_dir().ok_or_else(|| "no config dir".to_string())?;
        let dir = base.join(APP_DIR);
        let hosts_json = dir.join("hosts.json");
        let schedules_json = dir.join("schedules.json");

        if hosts_empty && hosts_json.exists() {
            match std::fs::read(&hosts_json) {
                Ok(bytes) if !bytes.is_empty() => {
                    let hosts: Vec<Host> = serde_json::from_slice(&bytes)
                        .map_err(|e| format!("parse hosts.json: {e}"))?;
                    let conn = self.inner.lock();
                    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
                    for h in &hosts {
                        if h.port == 0 {
                            continue; // skip any persisted localhost defensively
                        }
                        let data = serde_json::to_string(h).map_err(|e| e.to_string())?;
                        tx.execute(
                            "INSERT OR REPLACE INTO hosts (id, data) VALUES (?1, ?2)",
                            params![h.id.0.to_string(), data],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    tx.commit().map_err(|e| e.to_string())?;
                    info!("migrated {} hosts from hosts.json to helm.db", hosts.len());
                }
                _ => {}
            }
            let _ = std::fs::rename(
                &hosts_json,
                hosts_json.with_extension("json.pre-sqlite-backup"),
            );
        }

        if schedules_empty && schedules_json.exists() {
            match std::fs::read(&schedules_json) {
                Ok(bytes) if !bytes.is_empty() => {
                    let schedules: Vec<Schedule> = serde_json::from_slice(&bytes)
                        .map_err(|e| format!("parse schedules.json: {e}"))?;
                    let conn = self.inner.lock();
                    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
                    for s in &schedules {
                        let data = serde_json::to_string(s).map_err(|e| e.to_string())?;
                        tx.execute(
                            "INSERT OR REPLACE INTO schedules (id, data) VALUES (?1, ?2)",
                            params![s.id.0.to_string(), data],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    tx.commit().map_err(|e| e.to_string())?;
                    info!(
                        "migrated {} schedules from schedules.json to helm.db",
                        schedules.len()
                    );
                }
                _ => {}
            }
            let _ = std::fs::rename(
                &schedules_json,
                schedules_json.with_extension("json.pre-sqlite-backup"),
            );
        }

        Ok(())
    }

    fn is_table_empty(&self, table: &str) -> Result<bool, String> {
        let conn = self.inner.lock();
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .map_err(|e| format!("count {table}: {e}"))?;
        Ok(count == 0)
    }

    // ---------- hosts ----------

    pub fn list_hosts(&self) -> Result<Vec<Host>, String> {
        let conn = self.inner.lock();
        let mut stmt = conn
            .prepare("SELECT data FROM hosts")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                let data: String = r.get(0)?;
                Ok(data)
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let data = row.map_err(|e| e.to_string())?;
            match serde_json::from_str::<Host>(&data) {
                Ok(h) => out.push(h),
                Err(e) => warn!("skipping unparseable host row: {e}"),
            }
        }
        Ok(out)
    }

    pub fn upsert_host(&self, host: &Host) -> Result<(), String> {
        // Localhost IS persisted now — it can carry `is_anchor` and any
        // future per-machine metadata that should survive restart. The
        // JSON-migration path still defensively skips legacy port==0
        // entries, but we trust the constant `HostId::local()` for
        // dedupe at the row level (PRIMARY KEY).
        let data = serde_json::to_string(host).map_err(|e| e.to_string())?;
        let conn = self.inner.lock();
        conn.execute(
            "INSERT OR REPLACE INTO hosts (id, data) VALUES (?1, ?2)",
            params![host.id.0.to_string(), data],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_host(&self, id: HostId) -> Result<Option<Host>, String> {
        let conn = self.inner.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM hosts WHERE id = ?1",
                params![id.0.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(data) = data else { return Ok(None) };
        serde_json::from_str(&data)
            .map(Some)
            .map_err(|e| format!("parse host: {e}"))
    }

    /// Atomically designate one host as the anchor (or clear the
    /// designation entirely with `None`). Reads every host row, flips
    /// `is_anchor` to match the new target, and writes back only the
    /// rows whose flag actually changed — all under a single
    /// transaction so concurrent host edits can't race into a state
    /// with two anchors.
    ///
    /// Returns the list of hosts whose flag changed, so the caller can
    /// emit one `HostAdded` (upsert) event per affected row.
    pub fn set_anchor_host(&self, target: Option<HostId>) -> Result<Vec<Host>, String> {
        let conn = self.inner.lock();
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let mut affected: Vec<Host> = Vec::new();
        {
            let mut stmt = tx
                .prepare("SELECT data FROM hosts")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            for row in rows {
                let data = row.map_err(|e| e.to_string())?;
                let mut host: Host = match serde_json::from_str(&data) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("set_anchor_host: skipping unparseable host row: {e}");
                        continue;
                    }
                };
                let want = target == Some(host.id);
                if host.is_anchor != want {
                    host.is_anchor = want;
                    affected.push(host);
                }
            }
        }
        for host in &affected {
            let data = serde_json::to_string(host).map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO hosts (id, data) VALUES (?1, ?2)",
                params![host.id.0.to_string(), data],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(affected)
    }

    pub fn delete_host(&self, id: HostId) -> Result<(), String> {
        let conn = self.inner.lock();
        conn.execute("DELETE FROM hosts WHERE id = ?1", params![id.0.to_string()])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ---------- notifications ----------

    pub fn list_notifications(&self) -> Result<Vec<Notification>, String> {
        let conn = self.inner.lock();
        let mut stmt = conn
            .prepare("SELECT data FROM notifications ORDER BY created_at ASC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let data = row.map_err(|e| e.to_string())?;
            match serde_json::from_str::<Notification>(&data) {
                Ok(n) => out.push(n),
                Err(e) => warn!("skipping unparseable notification row: {e}"),
            }
        }
        Ok(out)
    }

    pub fn upsert_notification(&self, n: &Notification) -> Result<(), String> {
        let data = serde_json::to_string(n).map_err(|e| e.to_string())?;
        let conn = self.inner.lock();
        conn.execute(
            "INSERT OR REPLACE INTO notifications (id, host_id, pane_id, created_at, data)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                n.id.0.to_string(),
                n.host_id.0.to_string(),
                n.pane_id,
                n.created_at as i64,
                data,
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_notification(&self, id: NotificationId) -> Result<(), String> {
        let conn = self.inner.lock();
        conn.execute(
            "DELETE FROM notifications WHERE id = ?1",
            params![id.0.to_string()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_notifications_for_host(&self, host_id: HostId) -> Result<(), String> {
        let conn = self.inner.lock();
        conn.execute(
            "DELETE FROM notifications WHERE host_id = ?1",
            params![host_id.0.to_string()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ---------- schedules ----------

    pub fn list_schedules(&self) -> Result<Vec<Schedule>, String> {
        let conn = self.inner.lock();
        let mut stmt = conn
            .prepare("SELECT data FROM schedules")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let data = row.map_err(|e| e.to_string())?;
            match serde_json::from_str::<Schedule>(&data) {
                Ok(s) => out.push(s),
                Err(e) => warn!("skipping unparseable schedule row: {e}"),
            }
        }
        Ok(out)
    }

    pub fn upsert_schedule(&self, schedule: &Schedule) -> Result<(), String> {
        let data = serde_json::to_string(schedule).map_err(|e| e.to_string())?;
        let conn = self.inner.lock();
        conn.execute(
            "INSERT OR REPLACE INTO schedules (id, data) VALUES (?1, ?2)",
            params![schedule.id.0.to_string(), data],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_schedule(&self, id: ScheduleId) -> Result<(), String> {
        let conn = self.inner.lock();
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM schedule_runs WHERE schedule_id = ?1",
            params![id.0.to_string()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM schedules WHERE id = ?1",
            params![id.0.to_string()],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    // ---------- schedule_runs ----------
    //
    // The table is wired but unused in Phase 0 — run history stays
    // in-memory exactly like today. These methods are scaffolding for
    // Phase 1+ when run history moves onto the anchor.

    /// Recent runs for a schedule, most-recent-first, up to `limit` rows.
    #[allow(dead_code)]
    pub fn list_schedule_runs(
        &self,
        schedule_id: ScheduleId,
        limit: usize,
    ) -> Result<Vec<ScheduleRun>, String> {
        let conn = self.inner.lock();
        let mut stmt = conn
            .prepare(
                "SELECT data FROM schedule_runs
                 WHERE schedule_id = ?1
                 ORDER BY started_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![schedule_id.0.to_string(), limit as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let data = row.map_err(|e| e.to_string())?;
            match serde_json::from_str::<ScheduleRun>(&data) {
                Ok(r) => out.push(r),
                Err(e) => warn!("skipping unparseable schedule_run row: {e}"),
            }
        }
        Ok(out)
    }

    /// Insert a run and trim older rows past `keep` so per-schedule
    /// history is bounded. Cheap — both ops hit the
    /// `(schedule_id, started_at DESC)` index.
    #[allow(dead_code)]
    pub fn insert_schedule_run(&self, run: &ScheduleRun, keep: usize) -> Result<(), String> {
        let data = serde_json::to_string(run).map_err(|e| e.to_string())?;
        let conn = self.inner.lock();
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR REPLACE INTO schedule_runs (id, schedule_id, started_at, data)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run.id.0.to_string(),
                run.schedule_id.0.to_string(),
                run.started_at as i64,
                data,
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM schedule_runs
             WHERE schedule_id = ?1
               AND id NOT IN (
                 SELECT id FROM schedule_runs
                 WHERE schedule_id = ?1
                 ORDER BY started_at DESC
                 LIMIT ?2
               )",
            params![run.schedule_id.0.to_string(), keep as i64],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }
}
