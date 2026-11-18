//! 分配器对照压测：同一个二进制，用 feature 切 `#[global_allocator]`。
//!
//! 负载模拟 labyard 的真实特征：HTTP + JSON + SQLite 查询 + 中等并发 + 内存 churn。
//! 测的不是吞吐（容器里测不准），是 **RSS 曲线**：分配释放反复进行后会不会收敛。

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
struct App {
    db: Arc<Mutex<rusqlite::Connection>>,
    // 模拟"长期运行中不断分配释放的中间对象"：缓存条目、日志行、JSON 缓冲
    heap: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[derive(Serialize)]
struct Resp {
    id: i64,
    name: String,
    values: Vec<i64>,
}

async fn query(State(app): State<App>) -> Json<Resp> {
    let (id, name) = {
        let conn = app.db.lock().unwrap();
        let mut stmt = conn
            .prepare_cached("SELECT id, name FROM items WHERE id = ?1")
            .unwrap();
        stmt.query_row([1i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
    };
    Json(Resp {
        id,
        name,
        values: (0..50).collect(),
    })
}

/// 一轮 churn = 1000 个对象（按真实分布）分配 + 整体释放。
/// 分布：90% 小对象(64–512B) / 9% 中等(1–4KB) / 1% 大(64–128KB)。
async fn churn(State(app): State<App>) -> Json<serde_json::Value> {
    let rounds = 300usize;
    for round in 0..rounds {
        {
            let mut h = app.heap.lock().unwrap();
            for i in 0..1000usize {
                let size = match i % 100 {
                    n if n < 90 => 64 + n * 5,      // 64..512 B
                    n if n < 99 => 1024 << (n % 3), // 1K..4K
                    n => 65536 << (n % 2),          // 64K..128K
                };
                h.push(vec![(round % 251) as u8; size]);
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
        app.heap.lock().unwrap().clear();
    }
    Json(serde_json::json!({ "churn_rounds": rounds }))
}

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO items VALUES (1, 'labyard');",
    )
    .unwrap();

    let app = App {
        db: Arc::new(Mutex::new(db)),
        heap: Arc::new(Mutex::new(Vec::new())),
    };

    let router = Router::new()
        .route("/health", get(health))
        .route("/api/item", get(query))
        .route("/api/churn", post(churn))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:18080")
        .await
        .unwrap();
    axum::serve(listener, router).await.unwrap();
}
