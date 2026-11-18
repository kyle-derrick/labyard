//! SQLite 持久层。不用 ORM，用编号迁移 + 手写 SQL。
//!
//! 三张有用的表：
//! - `step_state`：幂等的核心，记录每个 step_key 上一次成功时的 input_hash
//! - `vmids`：vmid 的持久分配（同一台主机永远拿到同一个号，重启也不变）
//! - `runs` / `run_steps`：一次 apply 的记录与逐步状态
//!
//! 为什么用同步 `rusqlite` 而不是 async SQL 库：查询几乎全是"按主键取一行"，
//! 临界区在微秒级；上 async SQL 只会引入编译期连库与更大的依赖树。

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("迁移失败: {0}")]
    Migrate(String),
}

/// 一版迁移。顺序执行，版本号记在 `schema_version`。
const MIGRATIONS: &[(&str, &str)] = &[(
    "001-init",
    r#"
CREATE TABLE IF NOT EXISTS step_state (
    key         TEXT PRIMARY KEY,
    input_hash  TEXT NOT NULL,
    state       TEXT NOT NULL,
    message     TEXT,
    updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS vmids (
    vmid  INTEGER PRIMARY KEY,
    zone  TEXT NOT NULL,
    host  TEXT NOT NULL,
    UNIQUE (zone, host)
);

CREATE TABLE IF NOT EXISTS runs (
    id          TEXT PRIMARY KEY,
    status      TEXT NOT NULL,
    spec_name   TEXT,
    started_at  TEXT NOT NULL,
    finished_at TEXT
);

CREATE TABLE IF NOT EXISTS run_steps (
    run_id      TEXT NOT NULL,
    step_key    TEXT NOT NULL,
    state       TEXT NOT NULL,
    message     TEXT,
    started_at  TEXT,
    finished_at TEXT,
    PRIMARY KEY (run_id, step_key)
);
"#,
)];

pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        // WAL + NORMAL：避免每次 fsync 把盘叫醒（§7.6.9 第 5 条）
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL 是单写者：Container 阶段并发 2 时两个写会撞锁。真机实测撞过——
        // 早期写失败被 `let _ =` 吞掉，容器建成但状态没记录，下一轮误判
        // "从未执行"。等待重试是正解，静默丢写是谎言。
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);",
        )?;
        let current: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        for (idx, (name, sql)) in MIGRATIONS.iter().enumerate() {
            let v = (idx + 1) as i64;
            if v <= current {
                continue;
            }
            conn.execute_batch(sql)
                .map_err(|e| DbError::Migrate(format!("{name}: {e}")))?;
            conn.execute("INSERT INTO schema_version (version) VALUES (?1)", [v])?;
        }
        Ok(())
    }

    // ── step_state：幂等的判定依据 ──

    /// 返回上一次成功时的 input_hash。None 表示从没成功过。
    pub fn step_hash(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT input_hash FROM step_state WHERE key = ?1 AND state = 'done'",
            [key],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 返回写入错误而不是吞掉：状态记录失败 = 这一步不算完成，
    /// 调用方要把它当失败处理（资源可能已经建成，重跑由幂等保证安全）。
    pub fn record_step(
        &self,
        key: &str,
        hash: &str,
        state: &str,
        message: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO step_state (key, input_hash, state, message, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET
                input_hash = excluded.input_hash,
                state = excluded.state,
                message = excluded.message,
                updated_at = excluded.updated_at",
            params![key, hash, state, message, now()],
        )
        .map(|_| ())
    }

    /// 清掉某个 key 的记录（销毁资源后调用，否则下次 apply 会以为已完成）。
    ///
    /// 只有测试在用：真正的 destroy 路径还没实现（README「未实现」里有列）。
    /// 标 `cfg(test)` 是为了让"没有调用方"这件事在编译期就说清楚，
    /// 而不是靠一个 `#[allow(dead_code)]` 掩盖过去。
    #[cfg(test)]
    pub fn forget_step(&self, key: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM step_state WHERE key = ?1", [key]);
    }

    // ── vmid 分配 ──

    /// 取这台主机已分配的 vmid，没有就从 `base` 起分配一个最小的空闲号。
    pub fn vmid_for(&self, zone: &str, host: &str, base: u32) -> u32 {
        let conn = self.conn.lock().unwrap();
        if let Ok(v) = conn.query_row(
            "SELECT vmid FROM vmids WHERE zone = ?1 AND host = ?2",
            params![zone, host],
            |r| r.get::<_, u32>(0),
        ) {
            return v;
        }
        let mut candidate = base;
        loop {
            let taken: bool = conn
                .query_row(
                    "SELECT 1 FROM vmids WHERE vmid = ?1",
                    [candidate],
                    |_| Ok(true),
                )
                .optional()
                .unwrap_or(None)
                .unwrap_or(false);
            if !taken {
                break;
            }
            candidate += 1;
        }
        let _ = conn.execute(
            "INSERT INTO vmids (vmid, zone, host) VALUES (?1, ?2, ?3)",
            params![candidate, zone, host],
        );
        candidate
    }

    /// 删除资源时一并释放 vmid（destroy 才调）。同 `forget_step`，仅测试使用。
    #[cfg(test)]
    pub fn release_vmid(&self, zone: &str, host: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM vmids WHERE zone = ?1 AND host = ?2",
            params![zone, host],
        );
    }

    // ── runs ──

    pub fn create_run(&self, id: &str, spec_name: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO runs (id, status, spec_name, started_at) VALUES (?1, 'running', ?2, ?3)",
            params![id, spec_name, now()],
        );
    }

    pub fn finish_run(&self, id: &str, status: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE runs SET status = ?2, finished_at = ?3 WHERE id = ?1",
            params![id, status, now()],
        );
    }

    /// 进程启动时调用：上次没收尾的 run 一律标成 interrupted（可续跑）。
    pub fn mark_interrupted(&self) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE runs SET status = 'interrupted', finished_at = ?1 WHERE status = 'running'",
            [now()],
        );
    }

    /// 仅测试使用（用于断言 `mark_interrupted` 的效果）。
    #[cfg(test)]
    pub fn run_status(&self, id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT status FROM runs WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn record_run_step(
        &self,
        run_id: &str,
        step_key: &str,
        state: &str,
        message: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run_steps (run_id, step_key, state, message, started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(run_id, step_key) DO UPDATE SET
                state = excluded.state, message = excluded.message, finished_at = excluded.finished_at",
            params![run_id, step_key, state, message, now()],
        )
        .map(|_| ())
    }

    /// 列出所有已记录状态的步骤（给 UI 显示"哪些已就绪"）。
    pub fn all_step_states(&self) -> Vec<(String, String, String)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT key, state, COALESCE(message,'') FROM step_state ORDER BY key")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|it| it.filter_map(|x| x.ok()).collect())
            .unwrap_or_default()
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmid_is_stable_across_calls() {
        let db = Db::open_memory().unwrap();
        let a = db.vmid_for("main", "gateway", 100);
        let b = db.vmid_for("main", "gateway", 100);
        assert_eq!(a, b);
        assert_eq!(a, 100);
    }

    #[test]
    fn vmid_skips_taken_numbers() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.vmid_for("main", "a", 100), 100);
        assert_eq!(db.vmid_for("main", "b", 100), 101);
        assert_eq!(db.vmid_for("main", "c", 100), 102);
    }

    #[test]
    fn released_vmid_can_be_reused() {
        let db = Db::open_memory().unwrap();
        db.vmid_for("main", "a", 100);
        db.release_vmid("main", "a");
        assert_eq!(db.vmid_for("main", "b", 100), 100);
    }

    #[test]
    fn step_hash_only_counts_done() {
        let db = Db::open_memory().unwrap();
        db.record_step("bridge:main", "h1", "failed", Some("boom"));
        assert_eq!(db.step_hash("bridge:main"), None, "失败的步骤不能算已完成");
        db.record_step("bridge:main", "h1", "done", None);
        assert_eq!(db.step_hash("bridge:main"), Some("h1".into()));
    }

    #[test]
    fn interrupted_runs_are_marked() {
        let db = Db::open_memory().unwrap();
        db.create_run("r1", "main.yaml");
        assert_eq!(db.run_status("r1"), Some("running".into()));
        db.mark_interrupted();
        assert_eq!(db.run_status("r1"), Some("interrupted".into()));
    }

    #[test]
    fn forget_step_removes_record() {
        let db = Db::open_memory().unwrap();
        db.record_step("ct:main:gw", "h", "done", None);
        db.forget_step("ct:main:gw");
        assert_eq!(db.step_hash("ct:main:gw"), None);
    }
}
