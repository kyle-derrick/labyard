//! HTTP 层：axum 路由、SSE 日志流、认证、静态资源。

pub mod auth;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::core::plan::Plan;
use crate::core::runner::{Event, Runner};
use crate::core::spec::Spec;
use crate::db::Db;
use crate::pve::Pve;
use crate::resources::{assets, Executor};

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

pub struct AppState {
    pub db: Arc<Db>,
    pub pve: Arc<Pve>,
    pub runner: Arc<dyn Runner>,
    pub cfg: Arc<Config>,
    pub secret: Arc<Vec<u8>>,
    pub password_hash: Arc<String>,
    pub runs: Arc<Mutex<HashMap<String, RunHandle>>>,
    pub concurrency: usize,
    /// 解析 + 规划的并发闸。
    ///
    /// 实测：20 个并发 POST /api/preview 会让峰值 HWM 冲到 38 MB——每次 preview 都要
    /// 完整解析 YAML、resolve 三层并构建计划，瞬时对象量远大于稳态。这是整个进程
    /// 唯一会真正吃内存的路径，所以必须限流，否则一个刷新风暴就能顶到 MemoryMax。
    pub preview_gate: Arc<tokio::sync::Semaphore>,
    /// 密码哈希闸（容量 1）。
    ///
    /// 实测：`scrypt(N=2^15, r=8)` 的工作内存是 `128·N·r = 32 MB`，且是**瞬时**的。
    /// 单次哈希让进程 HWM 从 6.7 MB 冲到 38 MB。两个并发登录就是 64 MB+，
    /// 正好顶到 `MemoryMax=64M` 会被内核杀掉。所以哈希必须串行。
    pub hash_gate: Arc<tokio::sync::Semaphore>,
}

pub struct RunHandle {
    pub tx: broadcast::Sender<Event>,
    pub cancel: CancellationToken,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/state", get(state_of_world))
        .route("/api/spec", get(get_spec).put(put_spec))
        .route("/api/preview", post(preview))
        .route("/api/apply", post(apply))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/doctor", get(doctor))
        .route("/api/openapi.json", get(openapi))
        .fallback(get(static_handler))
        .with_state(state)
}

// ───────────────────────── 认证 ─────────────────────────

/// 从 cookie 里取用户名。未登录返回 None。
fn current_user(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if state.cfg.server.allow_anonymous {
        return Some("anonymous".into());
    }
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    let token = cookie
        .split(';')
        .filter_map(|c| c.trim().strip_prefix("labyard_session="))
        .next()?;
    let payload = auth::verify(&state.secret, token)?;
    let (user, exp) = payload.rsplit_once('|')?;
    let exp: i64 = exp.parse().ok()?;
    if chrono::Utc::now().timestamp() > exp {
        return None;
    }
    Some(user.to_string())
}

macro_rules! require_auth {
    ($state:expr, $headers:expr) => {
        match current_user(&$state, &$headers) {
            Some(u) => u,
            None => return unauthorized(),
        }
    };
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "未登录"}))).into_response()
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

#[derive(Serialize)]
struct LoginResp {
    user: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginReq>,
) -> Response {
    // scrypt 要 32 MB 工作内存，必须串行，否则并发登录会把 MemoryMax 顶穿
    let _permit = match state.hash_gate.acquire().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "服务正在关闭"})),
            )
                .into_response()
        }
    };

    let pw = req.password;
    let hash = state.password_hash.to_string();
    // scrypt 是 CPU + 内存密集的同步计算，放 spawn_blocking，
    // 否则会占住一个 runtime worker 几十毫秒
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&pw, &hash))
        .await
        .unwrap_or(false);

    if !ok {
        // 故意不区分"用户不存在/密码错"，也不返回剩余尝试次数
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "密码错误"})),
        )
            .into_response();
    }
    let exp = chrono::Utc::now().timestamp() + state.cfg.server.session_hours * 3600;
    let token = auth::sign(&state.secret, &format!("admin|{exp}"));
    let cookie = format!(
        "labyard_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        state.cfg.server.session_hours * 3600
    );
    let mut resp = Json(LoginResp { user: "admin".into() }).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        cookie.parse().unwrap_or_else(|_| "".parse().unwrap()),
    );
    resp
}

async fn logout() -> Response {
    let mut resp = Json(serde_json::json!({"ok": true})).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        "labyard_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
            .parse()
            .unwrap(),
    );
    resp
}

async fn me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match current_user(&state, &headers) {
        Some(u) => Json(serde_json::json!({"user": u})).into_response(),
        None => unauthorized(),
    }
}

// ───────────────────────── 健康 / 状态 ─────────────────────────

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Serialize)]
struct StepView {
    key: String,
    phase: String,
    state: String,
    message: String,
}

async fn state_of_world(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    require_auth!(state, headers);
    let steps: Vec<StepView> = state
        .db
        .all_step_states()
        .into_iter()
        .map(|(key, st, msg)| StepView {
            phase: phase_of_key(&key),
            key,
            state: st,
            message: msg,
        })
        .collect();
    Json(serde_json::json!({ "steps": steps })).into_response()
}

fn phase_of_key(key: &str) -> String {
    key.split(':').next().unwrap_or("unknown").to_string()
}

// ───────────────────────── 规格持久化 ─────────────────────────

/// 为什么不放 SQLite：规格是一份人可读的 YAML，运维要能 `cat` 出来、能用 git 管、
/// 能在出问题时手工改回去。放文件里这些全都免费，放 DB 里就要多写一套导入导出。
/// `paths.specs` 在配置里本来就有，之前只建了目录没用上。
const SPEC_MAX_BYTES: usize = 512 * 1024;

fn spec_file(cfg: &Config) -> std::path::PathBuf {
    cfg.paths.specs.join("lab.yaml")
}

#[derive(Deserialize)]
struct SpecBody {
    yaml: String,
}

async fn get_spec(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    require_auth!(state, headers);
    match tokio::fs::read_to_string(spec_file(&state.cfg)).await {
        // 原样返回，不在这里解析：文件可能被手工改坏，让前端拿到它自己报错，
        // 比返回一个空规格让用户以为"东西没了"要好。
        Ok(yaml) => Json(serde_json::json!({ "yaml": yaml, "exists": true })).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Json(serde_json::json!({ "yaml": null, "exists": false })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("读取规格失败: {e}") })),
        )
            .into_response(),
    }
}

async fn put_spec(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<SpecBody>,
) -> Response {
    require_auth!(state, headers);
    // 先卡大小再解析：Spec::parse 会为整份文本建对象，超大输入不该进解析器
    if body.yaml.len() > SPEC_MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "error": format!("规格超过 {} KB 上限", SPEC_MAX_BYTES / 1024)
            })),
        )
            .into_response();
    }
    // 存进去的必须是能 resolve 的：语法错、字段拼错、uplink 指向不存在的 zone，
    // 都在这里挡住，而不是等 apply 时才发现。
    if let Err(e) = Spec::parse(&body.yaml).and_then(|s| s.resolve(&state.cfg.pve.wan_bridge)) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() })))
            .into_response();
    }
    let p = spec_file(&state.cfg);
    // 原子写：同目录 tmp + rename。直接写会在掉电/被杀时留下半份 YAML，
    // 而半份 YAML 比"没有 YAML"更糟——它解析失败，但看起来像用户自己写错了。
    let tmp = p.with_extension("yaml.tmp");
    if let Err(e) = tokio::fs::write(&tmp, body.yaml.as_bytes()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("写临时文件失败: {e}") })),
        )
            .into_response();
    }
    if let Err(e) = tokio::fs::rename(&tmp, &p).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("替换规格失败: {e}") })),
        )
            .into_response();
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ───────────────────────── 预览 / 执行 ─────────────────────────

#[derive(Deserialize)]
struct SpecReq {
    yaml: String,
    #[serde(default)]
    snapshot: bool,
}

#[derive(Serialize)]
struct PreviewResp {
    zones: Vec<serde_json::Value>,
    steps: Vec<serde_json::Value>,
}

async fn preview(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<SpecReq>) -> Response {
    require_auth!(state, headers);
    // 拿到许可才解析；拿不到就排队，而不是把内存打满
    let _permit = match state.preview_gate.acquire().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "服务正在关闭"})),
            )
                .into_response()
        }
    };
    match build_plan(&state, &req.yaml, req.snapshot) {
        Ok((zones, plan)) => Json(PreviewResp {
            zones: zones
                .iter()
                .map(|z| serde_json::to_value(z).unwrap_or_default())
                .collect(),
            steps: plan
                .steps
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "key": s.key,
                        "phase": s.phase.as_str(),
                        "input_hash": &s.input_hash[..8],
                        "zone": s.zone,
                    })
                })
                .collect(),
        })
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))).into_response(),
    }
}

fn build_plan(state: &AppState, yaml: &str, snapshot: bool) -> Result<(Vec<crate::core::spec::ResolvedZone>, Plan), String> {
    let spec = Spec::parse(yaml).map_err(|e| e.to_string())?;
    let mut zones = spec
        .resolve(&state.cfg.pve.wan_bridge)
        .map_err(|e| e.to_string())?;
    let db = state.db.clone();
    let base = state.cfg.pve.vmid_base;

    // 回填 vmid：预览时就应该显示真实号，而不是"?"。
    // 这里分配的号会被写进 DB，与随后 Plan 里用的是同一个值（vmid_for 是幂等的）。
    for z in &mut zones {
        for h in &mut z.hosts {
            if h.vmid.is_none() {
                h.vmid = Some(db.vmid_for(&z.name, &h.name, base));
            }
        }
    }

    let plan = Plan::build(
        &zones,
        &|p| assets::Assets::provisioner_hash(p),
        &|z, h| db.vmid_for(z, h, base),
        snapshot,
    );
    Ok((zones, plan))
}

#[derive(Serialize)]
struct ApplyResp {
    run_id: String,
}

async fn apply(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<SpecReq>) -> Response {
    require_auth!(state, headers);

    // 先卡版本再规划：设计里写的是"不做 8.x 兼容层"，那就必须在动手之前拒绝，
    // 而不是让用户在 PVE 8 上跑到一半才撞见字段不认识。
    // `doctor` 也会报这一条，但 doctor 是给人看的，这里才是真正的闸。
    {
        let cancel = CancellationToken::new();
        if let Err(e) = state.pve.require_pve9(&cancel).await {
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    let (zones, plan) = match build_plan(&state, &req.yaml, req.snapshot) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))).into_response()
        }
    };

    let run_id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        &crate::core::plan::sha256_hex(run_id_seed().as_bytes())[..6]
    );
    state.db.create_run(&run_id, "web");

    let (btx, _) = broadcast::channel::<Event>(1024);
    let cancel = CancellationToken::new();
    state.runs.lock().await.insert(
        run_id.clone(),
        RunHandle {
            tx: btx.clone(),
            cancel: cancel.clone(),
        },
    );

    let db = state.db.clone();
    let pve = state.pve.clone();
    let runner = state.runner.clone();
    let conc = state.concurrency;
    let rid = run_id.clone();
    let _ = zones;

    tokio::spawn(async move {
        let (mtx, mut mrx) = mpsc::channel::<Event>(500);
        let btx2 = btx.clone();
        // mpsc（有背压）→ broadcast（多订阅者）
        let fwd = tokio::spawn(async move {
            while let Some(ev) = mrx.recv().await {
                let _ = btx2.send(ev);
            }
        });

        let ex = Executor::new(pve, db.clone(), runner);
        let summary = ex.execute(&plan, &rid, mtx, cancel.clone(), conc).await;

        let _ = fwd.await;
        let final_status = if cancel.is_cancelled() {
            "cancelled"
        } else if summary.ok() {
            "ok"
        } else {
            "failed"
        };
        db.finish_run(&rid, final_status);
        let _ = btx.send(Event::Finished {
            key: format!("run:{rid}"),
            code: Some(if summary.ok() { 0 } else { 1 }),
            duration_ms: 0,
        });
    });

    Json(ApplyResp { run_id }).into_response()
}

fn run_id_seed() -> String {
    format!(
        "{}:{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        std::process::id()
    )
}

// ───────────────────────── SSE ─────────────────────────

async fn run_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    require_auth!(state, headers);
    let rx = {
        let runs = state.runs.lock().await;
        match runs.get(&id) {
            Some(h) => h.tx.subscribe(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "run 不存在"})),
                )
                    .into_response()
            }
        }
    };

    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(ev) => {
            let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
            Some(Ok::<_, std::convert::Infallible>(
                SseEvent::default().data(data),
            ))
        }
        // 落后太多：告诉前端"丢了一段"，但不断流
        Err(_) => Some(Ok(SseEvent::default().comment("lagged"))),
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

async fn cancel_run(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    require_auth!(state, headers);
    let runs = state.runs.lock().await;
    match runs.get(&id) {
        Some(h) => {
            h.cancel.cancel();
            Json(serde_json::json!({"ok": true})).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "run 不存在"}))).into_response(),
    }
}

// ───────────────────────── doctor ─────────────────────────

#[derive(Serialize)]
struct DoctorItem {
    name: String,
    ok: bool,
    detail: String,
}

async fn doctor(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    require_auth!(state, headers);
    let cancel = CancellationToken::new();
    let mut items = Vec::new();

    match state.pve.version(&cancel).await {
        Ok(v) => {
            let major: u32 = v.version.split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0);
            items.push(DoctorItem {
                name: "PVE 版本".into(),
                ok: major >= 9,
                detail: format!("{} (需要 ≥ 9.0)", v.version),
            });
        }
        Err(e) => items.push(DoctorItem {
            name: "PVE 版本".into(),
            ok: false,
            detail: format!("无法读取: {e}"),
        }),
    }

    // 节点名写错是最难自查的一类失败：所有 `pct` 命令都还"能跑"，
    // 但会拿不到任何资源。这里直接对着 `/nodes` 核对，把名字列出来。
    match state.pve.nodes(&cancel).await {
        Ok(ns) => items.push(DoctorItem {
            name: "节点名".into(),
            ok: ns.iter().any(|n| n == &state.cfg.pve.node),
            detail: format!("配置 `{}`；现有: {}", state.cfg.pve.node, ns.join(", ")),
        }),
        Err(e) => items.push(DoctorItem {
            name: "节点名".into(),
            ok: false,
            detail: format!("无法读取: {e}"),
        }),
    }

    match state.pve.bridges(&cancel).await {        Ok(bs) => {
            let names: Vec<&str> = bs.iter().map(|b| b.iface.as_str()).collect();
            items.push(DoctorItem {
                name: "网桥".into(),
                ok: names.contains(&state.cfg.pve.wan_bridge.as_str()),
                detail: format!("现有: {}；wan 需要 {}", names.join(", "), state.cfg.pve.wan_bridge),
            });
        }
        Err(e) => items.push(DoctorItem {
            name: "网桥".into(),
            ok: false,
            detail: e.to_string(),
        }),
    }

    match state.pve.storages(&cancel).await {
        Ok(ss) => {
            let usable: Vec<String> = ss
                .iter()
                .filter(|s| s.supports("rootdir"))
                .map(|s| format!("{}({})", s.storage, s.kind))
                .collect();
            items.push(DoctorItem {
                name: "存储".into(),
                ok: !usable.is_empty(),
                detail: if usable.is_empty() {
                    "没有支持 rootdir 的存储".into()
                } else {
                    usable.join(", ")
                },
            });
        }
        Err(e) => items.push(DoctorItem {
            name: "存储".into(),
            ok: false,
            detail: e.to_string(),
        }),
    }

    // 模板（vztmpl）必须有可落地的存储。真机教训：rootfs 用 lvmthin 时
    // `pveam download local-lvm` 直接 255——模板和 rootfs 是两种内容类型，
    // 真实环境可能一体（dir/NFS）也可能分开（约定放 local），doctor 把
    // 现状列出来，哪个存储能放模板一目了然。
    match state.pve.storages(&cancel).await {
        Ok(ss) => {
            let tpl: Vec<String> = ss
                .iter()
                .filter(|s| s.supports("vztmpl"))
                .map(|s| format!("{}({})", s.storage, s.kind))
                .collect();
            items.push(DoctorItem {
                name: "模板存储".into(),
                ok: !tpl.is_empty(),
                detail: if tpl.is_empty() {
                    "没有支持 vztmpl 的存储——模板将无处下载，spec 里可用 template_storage 指定".into()
                } else {
                    format!("可放模板: {}；spec 默认跟随 rootfs 存储或用 template_storage 指定", tpl.join(", "))
                },
            });
        }
        Err(e) => items.push(DoctorItem {
            name: "模板存储".into(),
            ok: false,
            detail: e.to_string(),
        }),
    }

    // 快照能力单独列一条：厚 LVM 的 `pct snapshot` 会失败，而"执行时打快照"
    // 是可选开关——用户勾了才发现不行，比事先知道要糟得多。
    match state.pve.storages(&cancel).await {
        Ok(ss) => {
            let yes: Vec<&str> = ss
                .iter()
                .filter(|s| s.supports_snapshot())
                .map(|s| s.storage.as_str())
                .collect();
            let no: Vec<&str> = ss
                .iter()
                .filter(|s| !s.supports_snapshot())
                .map(|s| s.storage.as_str())
                .collect();
            items.push(DoctorItem {
                name: "快照能力".into(),
                ok: !yes.is_empty(),
                detail: format!(
                    "可快照: {}；不支持: {}",
                    if yes.is_empty() { "无".into() } else { yes.join(", ") },
                    if no.is_empty() { "无".into() } else { no.join(", ") }
                ),
            });
        }
        Err(e) => items.push(DoctorItem {
            name: "快照能力".into(),
            ok: false,
            detail: e.to_string(),
        }),
    }

    // 本机命令是否都在
    for (cmd, path) in [
        ("pct", "/usr/sbin/pct"),
        ("pvesh", "/usr/bin/pvesh"),
        ("pveam", "/usr/bin/pveam"),
        ("ip", "/usr/sbin/ip"),
        ("nft", "/usr/sbin/nft"),
    ] {
        let exists = std::path::Path::new(path).exists()
            || std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {cmd}"))
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        items.push(DoctorItem {
            name: format!("命令 {cmd}"),
            ok: exists,
            detail: if exists { "可用".into() } else { "缺失".into() },
        });
    }

    Json(serde_json::json!({ "items": items })).into_response()
}

// ───────────────────────── OpenAPI（手写，够前端生成类型）─────────────────────────

async fn openapi() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "openapi": "3.1.0",
        "info": {"title": "labyard", "version": env!("CARGO_PKG_VERSION")},
        "paths": {
            "/api/health": {"get": {"summary": "健康检查"}},
            "/api/auth/login": {"post": {"summary": "登录"}},
            "/api/state": {"get": {"summary": "已登记步骤的状态"}},
            "/api/spec": {
                "get": {"summary": "读取已保存的规格"},
                "put": {"summary": "保存规格（先校验，原子写）"}
            },
            "/api/preview": {"post": {"summary": "解析 spec 并返回计划"}},
            "/api/apply": {"post": {"summary": "执行（返回 run_id）"}},
            "/api/runs/{id}/events": {"get": {"summary": "SSE 日志流"}},
        }
    }))
}

// ───────────────────────── 静态资源 ─────────────────────────

#[derive(Deserialize)]
struct StaticQuery {
    #[serde(flatten)]
    _rest: HashMap<String, String>,
}

async fn static_handler(axum::extract::OriginalUri(uri): axum::extract::OriginalUri, _q: Query<StaticQuery>) -> Response {
    let path = uri.path().trim_start_matches('/');
    // SPA：非 /api 且无扩展名的路径一律回 index.html
    let (name, is_spa) = if path.is_empty() || !path.contains('.') {
        ("index.html", true)
    } else {
        (path, false)
    };

    match WebAssets::get(name) {
        Some(f) => {
            let mime = mime_guess::from_path(name).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                f.data.into_owned(),
            )
                .into_response()
        }
        None if is_spa || path.starts_with("api/") => {
            match WebAssets::get("index.html") {
                Some(f) => ([(header::CONTENT_TYPE, "text/html")], f.data.into_owned()).into_response(),
                None => (StatusCode::NOT_FOUND, "前端未构建").into_response(),
            }
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_matches_key_prefix() {
        assert_eq!(phase_of_key("ct:main:gw"), "ct");
        assert_eq!(phase_of_key("bridge:main"), "bridge");
        assert_eq!(phase_of_key("garbage"), "garbage");
    }
}
