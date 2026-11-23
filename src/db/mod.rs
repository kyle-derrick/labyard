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
const MIGRATIONS: &[(&str, &str)] = &[
    (
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
    ),
    (
        // retire 是**运行态**，不是声明式配置：它记录"这轮做完了，先收起来"，
        // 必须跨 apply 存活（否则一次 apply 就把它复活了）。host = '*' 表示
        // 整个 zone 被废弃。
        "002-lifecycle",
        r#"
CREATE TABLE IF NOT EXISTS retired (
    zone       TEXT NOT NULL,
    host       TEXT NOT NULL,
    note       TEXT,
    retired_at TEXT NOT NULL,
    PRIMARY KEY (zone, host)
);
"#,
    ),
];

/// run 历史行（只读视图，运维页直接序列化）。
#[derive(Debug, serde::Serialize)]
pub struct RunRow {
    pub id: String,
    pub status: String,
    pub spec_name: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// 失败的第一个步骤与原因。列表里直接给出来——让用户为了知道"为什么失败"
    /// 还要点进明细，是这次复盘里最常被抱怨的一处。
    pub failed_step: Option<String>,
    pub failed_message: Option<String>,
}

/// 一次 run 内的步骤记录。
#[derive(Debug, serde::Serialize)]
pub struct RunStepRow {
    pub step_key: String,
    pub state: String,
    pub message: String,
    pub started_at: String,
    pub finished_at: String,
}

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
    /// 删除步骤状态（destroy 调：资源没了，状态也该跟着没）。
    /// 之前 destroy 未实现时标过 cfg(test)——现在有真实调用方，转正。
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

    /// 删除资源时释放 vmid（destroy 调）。retire 不调——retire 保留占用。
    pub fn release_vmid(&self, zone: &str, host: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM vmids WHERE zone = ?1 AND host = ?2",
            params![zone, host],
        );
    }

    /// 把**已经在 PVE 上跑着**的容器绑给 (zone, host)。
    ///
    /// 与 `vmid_for` 的分工：那个是"给我一个号"（从 base 起找空闲），
    /// 这个是"这台容器就是它"（认调用方给的号）。纳管用后者。
    ///
    /// 两种冲突都拒绝，绝不"顺手覆盖"：
    /// - 这个 vmid 已经属于别的 (zone, host) → 那是别人管着的容器；
    /// - 这个 (zone, host) 已经绑了别的号 → 名字被占，再绑会让两台共用一个身份。
    pub fn bind_vmid(&self, zone: &str, host: &str, vmid: u32) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        if let Ok((z, h)) = conn.query_row(
            "SELECT zone, host FROM vmids WHERE vmid = ?1",
            [vmid],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ) {
            if z == zone && h == host {
                return Ok(());
            }
            return Err(format!("vmid {vmid} 已经绑定在 {z}:{h}"));
        }
        if let Ok(v) = conn.query_row(
            "SELECT vmid FROM vmids WHERE zone = ?1 AND host = ?2",
            params![zone, host],
            |r| r.get::<_, u32>(0),
        ) {
            return Err(format!("{zone}:{host} 已经绑定 vmid {v}"));
        }
        conn.execute(
            "INSERT INTO vmids (vmid, zone, host) VALUES (?1, ?2, ?3)",
            params![vmid, zone, host],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 只读查询：这台主机是否分配过 vmid。destroy 用它判断"有没有东西可删"，
    /// 不能用 `vmid_for`——那会在查不到时顺手分配一个，destroy 拿它一查
    /// 就"资源存在"了，实际 PVE 上根本没有这台容器。
    pub fn vmid_of(&self, zone: &str, host: &str) -> Option<u32> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT vmid FROM vmids WHERE zone = ?1 AND host = ?2",
            params![zone, host],
            |r| r.get::<_, u32>(0),
        )
        .optional()
        .unwrap_or(None)
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

    /// 一次 run 的当前状态。SSE 用它判断"这个 run 是不是已经结束"，好决定
    /// 是挂上去等事件，还是补一条 `finished` 就关流。
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
    /// run 历史（新→旧）。前端运维页用。
    pub fn list_runs(&self, limit: u32) -> Vec<RunRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT r.id, r.status, r.spec_name, r.started_at, r.finished_at,
                    (SELECT s.step_key FROM run_steps s
                      WHERE s.run_id = r.id AND s.state = 'failed'
                      ORDER BY s.started_at LIMIT 1),
                    (SELECT s.message FROM run_steps s
                      WHERE s.run_id = r.id AND s.state = 'failed'
                      ORDER BY s.started_at LIMIT 1)
             FROM runs r ORDER BY r.started_at DESC LIMIT ?1",
        ) {
            Ok(st) => st,
            Err(_) => return vec![],
        };
        stmt.query_map(params![limit], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                status: r.get(1)?,
                spec_name: r.get(2)?,
                started_at: r.get(3)?,
                finished_at: r.get(4)?,
                failed_step: r.get(5)?,
                failed_message: r.get(6)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// 一次 run 的步骤明细（含失败原因）。
    pub fn run_steps(&self, run_id: &str) -> Vec<RunStepRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT step_key, state, COALESCE(message,''), COALESCE(started_at,''), COALESCE(finished_at,'') \
             FROM run_steps WHERE run_id = ?1 ORDER BY started_at",
        ) {
            Ok(st) => st,
            Err(_) => return vec![],
        };
        stmt.query_map(params![run_id], |r| {
            Ok(RunStepRow {
                step_key: r.get(0)?,
                state: r.get(1)?,
                message: r.get(2)?,
                started_at: r.get(3)?,
                finished_at: r.get(4)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// 全部分配记录。前端主机列表用（主机身份的唯一权威是这张表）。
    pub fn all_vmids(&self) -> Vec<(String, String, u32)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT zone, host, vmid FROM vmids ORDER BY vmid") {
            Ok(st) => st,
            Err(_) => return vec![],
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// 按 LIKE 模式清步骤记录。
    ///
    /// retire 用它把与某个 zone / 主机相关的镜像与网关步骤清掉：revive 时
    /// 这些步骤才会真的重跑一遍，把策略与镜像实例原样装回去。
    pub fn forget_steps_matching(&self, pattern: &str) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM step_state WHERE key LIKE ?1", params![pattern])
            .unwrap_or(0)
    }

    /// 按 key 精确删步骤记录，返回删除条数。
    ///
    /// 规格文件被删掉之后，它留下的步骤记录不会自己消失：概览页会把
    /// `ct:main:gateway` 这种早已不存在的步骤当成"当前失败"，红着一个大数字
    /// 却找不到对应实体。清理必须按 key 精确删——用 LIKE 去猜会误伤名字前缀
    /// 相同的 zone（`lab` 与 `lab2`）。
    pub fn forget_steps(&self, keys: &[String]) -> usize {
        if keys.is_empty() {
            return 0;
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(_) => return 0,
        };
        let mut n = 0;
        for k in keys {
            n += tx
                .execute("DELETE FROM step_state WHERE key = ?1", params![k])
                .unwrap_or(0);
        }
        let _ = tx.commit();
        n
    }

    /// 删掉一条 vmid 分配记录。
    ///
    /// 只有在"规格里已经没有这台主机，且容器也确实不存在"时才允许调用——
    /// 分配记录是 IP 与笔记的锚，误删会让重建的主机换号。调用方负责判条件。
    pub fn forget_vmid(&self, zone: &str, host: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM vmids WHERE zone = ?1 AND host = ?2",
            params![zone, host],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    // ── retire / revive（运行态，不是声明式配置）──

    /// 标记废弃。`host = "*"` 表示整个 zone。
    ///
    /// 与 destroy 的区别只有一个但很关键：**保留 vmid 与步骤记录**——
    /// 否则重建时同一台 `main-server1` 可能拿到另一个 IP，笔记就全废了。
    pub fn retire(&self, zone: &str, host: &str, note: Option<&str>) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO retired (zone, host, note, retired_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(zone, host) DO UPDATE SET note = excluded.note, retired_at = excluded.retired_at",
            params![zone, host, note, now()],
        );
    }

    /// 取消废弃。返回是否真的有一条记录被删（用于判断"本来就没废弃"）。
    pub fn unretire(&self, zone: &str, host: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM retired WHERE zone = ?1 AND host = ?2",
            params![zone, host],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    /// 全部废弃记录：(zone, host, retired_at, note)。host = "*" 表示整个 zone。
    pub fn retired_list(&self) -> Vec<(String, String, String, String)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT zone, host, retired_at, COALESCE(note,'') FROM retired ORDER BY retired_at DESC",
        ) {
            Ok(st) => st,
            Err(_) => return vec![],
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// zone 级与主机级同时判断：zone 被标 `*` 时，其中每台主机都算废弃。
    pub fn is_retired(&self, zone: &str, host: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM retired WHERE zone = ?1 AND (host = ?2 OR host = '*') LIMIT 1",
            params![zone, host],
            |_| Ok(true),
        )
        .optional()
        .unwrap_or(None)
        .unwrap_or(false)
    }

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
        db.record_step("bridge:main", "h1", "failed", Some("boom"))
            .unwrap();
        assert_eq!(db.step_hash("bridge:main"), None, "失败的步骤不能算已完成");
        db.record_step("bridge:main", "h1", "done", None).unwrap();
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
        db.record_step("ct:main:gw", "h", "done", None).unwrap();
        db.forget_step("ct:main:gw");
        assert_eq!(db.step_hash("ct:main:gw"), None);
    }
}
