//! 运维类接口：网关策略、镜像连线、retire/revive、规格文件 CRUD。
//!
//! 一条贯穿所有写操作的原则：**运行时改动写回规格文件**。
//! 出网开关、端口映射、VPN、镜像连线都是声明式字段，接口只是"改规格 +
//! 立刻让它生效"。不在容器里存平台的影子状态，也就不会出现"规格说一套、
//! 跑的是另一套"这种最难排查的分叉。

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{build_plan, current_user, spawn_plan, AppState};
use crate::api::specs;
use crate::core::plan::Phase;
use crate::core::spec::Spec;
use crate::resources::Executor;

type ApiResult<T> = Result<T, (StatusCode, String)>;

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

fn authed(state: &Arc<AppState>, headers: &axum::http::HeaderMap) -> ApiResult<()> {
    if current_user(state, headers).is_none() {
        return Err((StatusCode::UNAUTHORIZED, "未登录".into()));
    }
    Ok(())
}

fn render<T: IntoResponse>(r: ApiResult<T>) -> Response {
    match r {
        Ok(v) => v.into_response(),
        Err((code, msg)) => (code, Json(serde_json::json!({ "error": msg }))).into_response(),
    }
}

/// `render` 的异步版：把 `async { ... }` 的结果直接转成 Response。
async fn render_async<T, F>(fut: F) -> Response
where
    F: std::future::Future<Output = ApiResult<T>>,
    T: IntoResponse,
{
    render(fut.await)
}

// ───────────────────────── 规格文件 ─────────────────────────

fn spec_dir(state: &AppState) -> std::path::PathBuf {
    state.cfg.paths.specs.clone()
}

/// 校验并落盘。**先解析后写**：非法 YAML 一律拒绝且不碰旧文件。
fn save_spec(state: &AppState, name: &str, yaml: &str) -> ApiResult<()> {
    Spec::parse(yaml)
        .and_then(|s| s.resolve_all(&state.cfg.pve.wan_bridge))
        .map_err(|e| bad(e))?;
    specs::write(&spec_dir(state), name, yaml).map_err(|e| bad(e))
}

/// 找到 zone 所在的规格。多份规格时按名字顺序取第一个包含它的。
pub(crate) fn locate_zone(state: &AppState, zone: &str) -> ApiResult<(String, String)> {
    let dir = spec_dir(state);
    for meta in specs::list(&dir, &state.cfg.pve.wan_bridge) {
        if !meta.valid {
            continue;
        }
        let Ok(Some(text)) = specs::read(&dir, &meta.name) else {
            continue;
        };
        let Ok(sp) = Spec::parse(&text) else { continue };
        if sp.zones.contains_key(zone) {
            return Ok((meta.name, text));
        }
    }
    Err(err(
        StatusCode::NOT_FOUND,
        format!("所有规格里都找不到 zone `{zone}`"),
    ))
}

pub async fn list_specs(State(state): State<Arc<AppState>>, headers: axum::http::HeaderMap) -> Response {
    render(authed(&state, &headers).map(|_| {
        Json(serde_json::json!({ "specs": specs::list(&spec_dir(&state), &state.cfg.pve.wan_bridge) }))
    }))
}

#[derive(Deserialize)]
pub struct NewSpec {
    pub name: String,
    #[serde(default)]
    pub yaml: Option<String>,
    /// 从现有规格复制一份（做实验的变体最常见）
    #[serde(default)]
    pub copy_of: Option<String>,
}

pub async fn create_spec(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<NewSpec>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        specs::check_name(&req.name).map_err(|e| bad(e))?;
        if specs::exists(&spec_dir(&state), &req.name) {
            return Err(err(
                StatusCode::CONFLICT,
                format!("规格 `{}` 已存在", req.name),
            ));
        }
        let yaml = if let Some(y) = req.yaml {
            y
        } else if let Some(src) = req.copy_of {
            specs::read(&spec_dir(&state), &src)
                .map_err(|e| bad(e))?
                .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("源规格 `{src}` 不存在")))?
        } else {
            DEFAULT_TEMPLATE.to_string()
        };
        save_spec(&state, &req.name, &yaml)?;
        Ok(Json(serde_json::json!({ "ok": true, "name": req.name })))
    })())
}

/// 新建规格的骨架。给一个能直接 apply 的最小例子，比空白页有用。
const DEFAULT_TEMPLATE: &str = r#"# labyard 规格：一个 zone = 一张内网 + 一个网关 + 若干主机
# 三层合并：hosts[x] > zones[x] > defaults，逐字段覆盖
defaults:
  cores: 1
  memory: 512
  disk: 4
  template: debian-13-standard
  storage: local-lvm
  # rootfs 用 lvmthin 时模板要单独放（vztmpl 内容类型）
  template_storage: local
  timezone: Asia/Shanghai
  lang: zh_CN.UTF-8
  packages: [vim, curl, jq]

zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      server1: { role: server }
"#;

pub async fn get_spec_named(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let yaml = specs::read(&spec_dir(&state), &name).map_err(|e| bad(e))?;
        Ok(Json(match yaml {
            Some(y) => serde_json::json!({ "yaml": y, "exists": true, "name": name }),
            None => serde_json::json!({ "yaml": null, "exists": false, "name": name }),
        }))
    })())
}

/// 结构化视图：给「可视化编排」用的 JSON 形态。
///
/// 为什么不把 YAML 丢给前端解析：解析规则（哪层是 zone、`{ role: gateway }`
/// 这种简写、默认值从哪继承）只应该有一份，在后端。前端拿到的是
/// `zones.<名字>.hosts[...]` 这种朴素结构。
///
/// 也不返回解析后的 `ResolvedSpec`：那份是**补全过默认值**的形态，拿它生成
/// YAML 会把用户没写的字段全写出来，`v6: auto` 还会被展开成具体前缀。
/// 所以这里给的是原文的 JSON 投影——没写的字段仍然不存在。
pub async fn get_model(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let yaml = specs::read(&spec_dir(&state), &name).map_err(bad)?;
        let Some(text) = yaml else {
            return Ok(Json(serde_json::json!({
                "exists": false, "model": null, "yaml": null, "name": name
            })));
        };
        let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).map_err(bad)?;
        let model = serde_json::to_value(&v)
            .map_err(|e| bad(format!("规格里有无法转成 JSON 的结构: {e}")))?;
        Ok(Json(serde_json::json!({
            "exists": true, "model": model, "yaml": text, "name": name
        })))
    })())
}

#[derive(Deserialize)]
pub struct ModelBody {
    pub model: serde_json::Value,
}

/// 可视化模式保存：JSON → YAML → 与手工粘贴走同一条校验 → 原子写。
///
/// 注释会丢（YAML 库不保留注释），所以响应里把生成的 YAML 一起返回，
/// 前端可以立刻切到 YAML 模式看一眼。"丢注释"这件事在界面上也明说了。
pub async fn put_model(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ModelBody>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let v = serde_yaml_ng::to_value(&body.model).map_err(bad)?;
        let text = serde_yaml_ng::to_string(&v).map_err(bad)?;
        // 与 `/api/specs/{name}` 的 PUT 同一条路：先解析、再展开，
        // 任何一处报错都不落盘。所以"图形化能存进去、应用却失败"不会发生。
        let sp = Spec::parse(&text).map_err(bad)?;
        sp.resolve_all(&state.cfg.pve.wan_bridge).map_err(bad)?;
        specs::write(&spec_dir(&state), &name, &text).map_err(bad)?;
        Ok(Json(serde_json::json!({ "ok": true, "yaml": text })))
    })())
}

#[derive(Deserialize)]
pub struct SpecBody {
    pub yaml: String,
}

pub async fn put_spec_named(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<SpecBody>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        save_spec(&state, &name, &body.yaml)?;
        Ok(Json(serde_json::json!({ "ok": true })))
    })())
}

#[derive(Deserialize)]
pub struct ZoneBody {
    pub zone: String,
    pub yaml: String,
}

/// 取单个 zone 的片段。前端"编辑 zone"用它做编辑框的起点——
/// 没有这个接口，UI 就只能让用户从头手写一份，等于没有编辑功能。
pub async fn get_zone(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((name, zone)): Path<(String, String)>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let cur = specs::read(&spec_dir(&state), &name)
            .map_err(|e| bad(e))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("规格 `{name}` 不存在")))?;
        let snip = specs::zone_snippet(&cur, &zone).map_err(|e| bad(e))?;
        Ok(Json(match snip {
            Some(y) => serde_json::json!({ "zone": zone, "yaml": y, "exists": true }),
            None => serde_json::json!({ "zone": zone, "yaml": null, "exists": false }),
        }))
    })())
}

pub async fn add_zone(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ZoneBody>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let cur = specs::read(&spec_dir(&state), &name)
            .map_err(|e| bad(e))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("规格 `{name}` 不存在")))?;
        let out = specs::upsert_zone(&cur, &body.zone, &body.yaml).map_err(|e| bad(e))?;
        save_spec(&state, &name, &out)?;
        Ok(Json(serde_json::json!({ "ok": true })))
    })())
}

pub async fn update_zone(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((name, zone)): Path<(String, String)>,
    Json(body): Json<SpecBody>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let cur = specs::read(&spec_dir(&state), &name)
            .map_err(|e| bad(e))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("规格 `{name}` 不存在")))?;
        if !Spec::parse(&cur).map(|s| s.zones.contains_key(&zone)).unwrap_or(false) {
            return Err(err(StatusCode::NOT_FOUND, format!("规格里没有 zone `{zone}`")));
        }
        let out = specs::upsert_zone(&cur, &zone, &body.yaml).map_err(|e| bad(e))?;
        save_spec(&state, &name, &out)?;
        Ok(Json(serde_json::json!({ "ok": true })))
    })())
}

pub async fn delete_zone(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((name, zone)): Path<(String, String)>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let cur = specs::read(&spec_dir(&state), &name)
            .map_err(|e| bad(e))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("规格 `{name}` 不存在")))?;
        let out = specs::delete_zone(&cur, &zone).map_err(|e| bad(e))?;
        // 只从规格里删掉，**不动 PVE**：删资源是 destroy 的事（两次确认）。
        save_spec(&state, &name, &out)?;
        Ok(Json(serde_json::json!({ "ok": true })))
    })())
}

pub async fn delete_spec(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Response {
    render((|| {
        authed(&state, &headers)?;
        let removed = specs::remove(&spec_dir(&state), &name).map_err(|e| bad(e))?;
        if !removed {
            return Err(err(StatusCode::NOT_FOUND, format!("规格 `{name}` 不存在")));
        }
        Ok(Json(serde_json::json!({ "ok": true })))
    })())
}

// ───────────────────────── 网关 ─────────────────────────

/// 只取某几个阶段的子计划。
///
/// 网关/镜像的读写都要"改完立刻生效"：用户点了开关却在等下一次全量 apply，
/// 会以为功能坏了。所以这两类操作自己起一个只含相关阶段的 run——其余步骤
/// 哈希未变，本来也不会跑。
async fn steps_plan(
    state: &AppState,
    zone: &str,
    yaml: &str,
    phases: &[Phase],
) -> ApiResult<crate::core::plan::Plan> {
    let (_, plan) = build_plan(state, yaml, false).map_err(|e| bad(e))?;
    let mut sub = plan.for_zone(zone);
    sub.steps.retain(|s| phases.contains(&s.phase));
    if sub.steps.is_empty() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("zone `{zone}` 没有对应阶段的步骤"),
        ));
    }
    Ok(sub)
}

async fn gateway_step_plan(state: &AppState, zone: &str, yaml: &str) -> ApiResult<crate::core::plan::Plan> {
    steps_plan(state, zone, yaml, &[Phase::Gateway, Phase::Mirror]).await
}

/// 改规格 → 校验 → 落盘 → 立刻重跑该 zone 的 gateway/mirror 步骤。
async fn apply_gateway_change<F>(state: Arc<AppState>, zone: &str, edit: F) -> ApiResult<Json<serde_json::Value>>
where
    F: FnOnce(&str, &str) -> Result<String, String>,
{
    let (spec_name, cur) = locate_zone(&state, zone)?;
    let next = edit(&cur, zone).map_err(|e| bad(e))?;
    save_spec(&state, &spec_name, &next)?;
    let plan = gateway_step_plan(&state, zone, &next).await?;
    let run_id = spawn_plan(&state, &plan, &format!("gateway:{zone}")).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "spec": spec_name,
    })))
}

#[derive(Deserialize)]
pub struct EgressReq {
    pub on: bool,
}

pub async fn gateway_egress(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(zone): Path<String>,
    Json(req): Json<EgressReq>,
) -> Response {
    if let Err(e) = authed(&state, &headers) {
        return render::<Json<serde_json::Value>>(Err(e));
    }
    let on = req.on;
    render(
        apply_gateway_change(state, &zone, move |yaml, z| {
            specs::set_egress(yaml, z, on)
        })
        .await,
    )
}

#[derive(Deserialize)]
pub struct MonitorReq {
    /// true = 开启整网采集；false = 关闭
    pub on: bool,
    /// 新建监控机的名字（不写用默认 monitor）
    #[serde(default)]
    pub name: Option<String>,
    /// 新建监控机管理口挂哪：wan / 某 zone 名；不写 = 本 zone 内网
    #[serde(default)]
    pub mgmt: Option<String>,
    /// 复用已有监控机：`zone:host`（JSON 键是 `use`，与规格字段同名）
    #[serde(rename = "use", default)]
    pub use_host: Option<String>,
}

/// 整网流量监控开关：写规格 + 立刻落地。
///
/// 开启 = 建抓包桥（每个被监控网络一块）、给监控机装抓包网卡、装采集单元；
/// 关闭 = 规格里移除 monitor，旧抓包桥与单元在下一次 apply 时清理。
/// 阶段与 add_mirror 相同（Bridge/Container/Mirror）：只跑 Mirror 会拿到
/// "桥和网卡都还不存在"的单元，而单元自己会静默退出。
pub async fn set_zone_monitor(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(zone): Path<String>,
    Json(req): Json<MonitorReq>,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        // 互斥在这里挡（而不是等 specs::set_zone_monitor）：前端不会同时传，
        // 但手打请求会——两个都传说明调用方没想清楚，直接 400。
        if req.use_host.is_some() && req.name.is_some() {
            return Err(bad("monitor 的 use 与 name 只能写一个"));
        }
        let (spec_name, cur) = locate_zone(&state, &zone)?;
        let monitor = if req.on {
            let mut obj = serde_json::Map::new();
            if let Some(u) = &req.use_host {
                obj.insert("use".into(), serde_json::json!(u));
            } else {
                obj.insert(
                    "name".into(),
                    serde_json::json!(req.name.clone().unwrap_or(crate::core::spec::DEFAULT_MONITOR.to_string())),
                );
                if let Some(mg) = &req.mgmt {
                    obj.insert("mgmt".into(), serde_json::json!(mg));
                }
            }
            Some(serde_json::Value::Object(obj))
        } else {
            None
        };
        let next = specs::set_zone_monitor(&cur, &zone, monitor.as_ref()).map_err(bad)?;
        save_spec(&state, &spec_name, &next)?;
        let run_id = match build_plan(&state, &next, false).map_err(|e| bad(e)) {
            Ok((_, mut plan)) => {
                plan.steps.retain(|s| {
                    matches!(s.phase, Phase::Bridge | Phase::Container | Phase::Mirror)
                });
                if plan.steps.is_empty() {
                    None
                } else {
                    Some(spawn_plan(&state, &plan, &format!("monitor:{zone}")).await)
                }
            }
            // 规格已写进去：落地失败是运行态问题，不该回滚开关
            Err(_) => None,
        };
        Ok(Json(
            serde_json::json!({ "ok": true, "spec": spec_name, "run_id": run_id }),
        ))
    })
    .await
}

#[derive(Deserialize)]
pub struct PubReq {
    pub proto: String,
    pub port: u16,
    pub to: String,
}

pub async fn gateway_pub_add(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(zone): Path<String>,
    Json(req): Json<PubReq>,
) -> Response {
    if let Err(e) = authed(&state, &headers) {
        return render::<Json<serde_json::Value>>(Err(e));
    }
    if !matches!(req.proto.as_str(), "tcp" | "udp") {
        return render::<Json<serde_json::Value>>(Err(bad("proto 只支持 tcp / udp")));
    }
    // 片段以 YAML 形态拼出来，走与 zone 编辑同一条路径（单一实现）
    let frag = format!(
        "{{ proto: {}, port: {}, to: \"{}\" }}",
        req.proto, req.port, req.to.replace('"', "")
    );
    render(
        apply_gateway_change(state, &zone, move |yaml, z| {
            specs::add_pub(yaml, z, &frag)
        })
        .await,
    )
}

pub async fn gateway_pub_del(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((zone, idx)): Path<(String, usize)>,
) -> Response {
    if let Err(e) = authed(&state, &headers) {
        return render::<Json<serde_json::Value>>(Err(e));
    }
    render(
        apply_gateway_change(state, &zone, move |yaml, z| {
            specs::remove_pub(yaml, z, idx)
        })
        .await,
    )
}

#[derive(Deserialize)]
pub struct VpnReq {
    /// wireguard | openvpn
    pub proto: String,
    /// on | off
    pub action: String,
}

pub async fn gateway_vpn(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(zone): Path<String>,
    Json(req): Json<VpnReq>,
) -> Response {
    if let Err(e) = authed(&state, &headers) {
        return render::<Json<serde_json::Value>>(Err(e));
    }
    let on = match req.action.as_str() {
        "on" | "enable" => true,
        "off" | "disable" => false,
        _ => return render::<Json<serde_json::Value>>(Err(bad("action 只支持 on / off"))),
    };
    let proto = req.proto.clone();
    render(
        apply_gateway_change(state, &zone, move |yaml, z| {
            specs::set_vpn(yaml, z, &proto, on)
        })
        .await,
    )
}

pub async fn gateway_status(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(zone): Path<String>,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        let (spec_name, cur) = locate_zone(&state, &zone)?;
        let sp = Spec::parse(&cur).map_err(|e| bad(e))?;
        let resolved = sp
            .resolve_all(&state.cfg.pve.wan_bridge)
            .map_err(|e| bad(e))?;
        let z = resolved
            .zones
            .iter()
            .find(|z| z.name == zone)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("解析后没有 zone `{zone}`")))?;
        let gw = z
            .gateway
            .as_ref()
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("zone `{zone}` 没有网关主机")))?;
        let host = z
            .hosts
            .iter()
            .find(|h| h.role == crate::core::spec::Role::Gateway)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "没有 gateway 角色主机".to_string()))?;
        let vmid = host
            .vmid
            .or_else(|| state.db.vmid_of(&zone, &host.name));

        // VPN 的"真的在跑吗"只能问容器。查不到就当没起——这里不猜。
        let cancel = CancellationToken::new();
        let mut wg_active = false;
        let mut ovpn_active = false;
        if let Some(v) = vmid {
            if let Ok(out) = state
                .pve
                .ct_exec(
                    "gw-active",
                    v,
                    &[
                        "sh",
                        "-c",
                        // 每条都 `|| true`：`systemctl is-active` 对未启动的单元
                        // 返回非零，而 `ct_exec` 把非零当成失败整体丢弃——那样
                        // "wg0 明明在跑"也会被报成未运行。这里要的是**输出**，
                        // 不是退出码。
                        "systemctl is-active wg-quick@wg0 2>/dev/null || true; \
                         systemctl is-active openvpn-server@labyard 2>/dev/null || true",
                    ],
                    std::time::Duration::from_secs(20),
                    &cancel,
                )
                .await
            {
                let lines: Vec<&str> = out.stdout.lines().map(|l| l.trim()).collect();
                wg_active = lines.first().map(|l| *l == "active").unwrap_or(false);
                ovpn_active = lines.get(1).map(|l| *l == "active").unwrap_or(false);
            }
        }

        let retired_hosts: Vec<&str> = z
            .hosts
            .iter()
            .filter(|h| state.db.is_retired(&zone, &h.name))
            .map(|h| h.name.as_str())
            .collect();

        Ok(Json(serde_json::json!({
            "zone": zone,
            "spec": spec_name,
            "gateway": { "host": host.name, "vmid": vmid, "ipv4": host.ipv4 },
            "v4_cidr": z.v4_cidr,
            "nat": z.nat,
            "egress": gw.egress,
            "pubs": gw.pubs.iter().map(|p| serde_json::json!({
                "proto": p.proto.as_str(),
                "port": p.port,
                "to": format!("{}:{}", p.to_ip, p.to_port),
                "spec": p.spec_string(),
            })).collect::<Vec<_>>(),
            "vpn": {
                "wireguard": gw.vpn.wireguard,
                "openvpn": gw.vpn.openvpn,
                "clients": gw.vpn.clients,
                "wireguard_active": wg_active,
                "openvpn_active": ovpn_active,
            },
            "retired_hosts": retired_hosts,
        })))
    })
    .await
}

#[derive(Deserialize)]
pub struct ClientQuery {
    #[serde(default)]
    pub client: Option<String>,
}

/// 下载 VPN 客户端配置。私钥只存在网关容器里，这里是**现读现拼**，平台不存副本。
pub async fn gateway_vpn_client(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((zone, proto)): Path<(String, String)>,
    Query(q): Query<ClientQuery>,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        if !matches!(proto.as_str(), "wireguard" | "openvpn") {
            return Err(bad("proto 只支持 wireguard / openvpn"));
        }
        let (_, cur) = locate_zone(&state, &zone)?;
        let sp = Spec::parse(&cur).map_err(|e| bad(e))?;
        let resolved = sp
            .resolve_all(&state.cfg.pve.wan_bridge)
            .map_err(|e| bad(e))?;
        let z = resolved
            .zones
            .iter()
            .find(|z| z.name == zone)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "zone 不存在".to_string()))?;
        let gw = z
            .gateway
            .as_ref()
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "没有网关".to_string()))?;
        let host = z
            .hosts
            .iter()
            .find(|h| h.role == crate::core::spec::Role::Gateway)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "没有网关主机".to_string()))?;
        let vmid = host
            .vmid
            .or_else(|| state.db.vmid_of(&zone, &host.name))
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "网关还没建（没有 vmid）".to_string()))?;
        let client = q
            .client
            .filter(|c| !c.is_empty())
            .or_else(|| gw.vpn.clients.first().cloned())
            .unwrap_or_else(|| "lab".to_string());
        if !client
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(bad("client 名非法"));
        }

        let cancel = CancellationToken::new();
        let (body, ext, mime) = if proto == "wireguard" {
            let text = state
                .pve
                .ct_read(vmid, &format!("/etc/wireguard/clients/{client}.conf"), &cancel)
                .await
                .map_err(|e| internal(format!("读取客户端配置失败（先在页面上打开一次 VPN）: {e}")))?;
            (text, "conf", "text/plain; charset=utf-8")
        } else {
            let ca = state.pve.ct_read(vmid, "/etc/openvpn/easy-rsa/pki/ca.crt", &cancel).await.map_err(|e| internal(e))?;
            let cert = state
                .pve
                .ct_read(vmid, &format!("/etc/openvpn/easy-rsa/pki/issued/{client}.crt"), &cancel)
                .await
                .map_err(|e| internal(e))?;
            let key = state
                .pve
                .ct_read(vmid, &format!("/etc/openvpn/easy-rsa/pki/private/{client}.key"), &cancel)
                .await
                .map_err(|e| internal(e))?;
            let tc = state.pve.ct_read(vmid, "/etc/openvpn/tc.key", &cancel).await.map_err(|e| internal(e))?;
            // 服务端证书里带 easy-rsa 附的文本包装，OpenVPN 能容忍，但这里
            // 只保留 PEM 块，避免一些客户端把说明当配置项
            let endpoint = state
                .pve
                .ct_exec(
                    "gw-endpoint",
                    vmid,
                    &[
                        "sh",
                        "-c",
                        "ip -4 -o addr show eth1 | awk '{print $4}' | cut -d/ -f1 | head -1",
                    ],
                    std::time::Duration::from_secs(20),
                    &cancel,
                )
                .await
                .map(|o| o.stdout.trim().to_string())
                .unwrap_or_default();
            let endpoint = if endpoint.is_empty() {
                host.ipv4.split('/').next().unwrap_or("127.0.0.1").to_string()
            } else {
                endpoint
            };
            let body = format!(
                "client\ndev tun\nproto udp\nremote {endpoint} {}\nresolv-retry infinite\nnobind\npersist-key\npersist-tun\nverb 3\n\n<ca>\n{}</ca>\n<cert>\n{}</cert>\n<key>\n{}</key>\n<tls-crypt>\n{}</tls-crypt>\n",
                crate::resources::gateway::OVPN_PORT,
                pem(ca),
                pem(cert),
                pem(key),
                pem(tc)
            );
            (body, "ovpn", "application/x-openvpn-profile")
        };

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{zone}-{client}.{ext}\""),
            )
            .body(axum::body::Body::from(body))
            .map_err(|e| internal(e))
    })
    .await
}

/// 从 easy-rsa 输出里抠出 PEM 块（文件里可能夹着 "subject=..." 之类的注释）。
fn pem(s: String) -> String {
    let start = s.find("-----BEGIN").unwrap_or(0);
    let end = s.rfind("-----END").map(|i| i).unwrap_or(s.len());
    let tail_end = s[end..].find('\n').map(|i| end + i + 1).unwrap_or(s.len());
    s[start..tail_end].to_string()
}

// ───────────────────────── 拓扑 ─────────────────────────

/// 全局拓扑：所有规格（或指定一份）解析后串成一张图。
pub async fn topology_view(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<SpecQuery>,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        let dir = spec_dir(&state);
        let mut resolved: Vec<(String, crate::core::spec::ResolvedSpec)> = Vec::new();
        for meta in specs::list(&dir, &state.cfg.pve.wan_bridge) {
            if let Some(only) = &q.spec {
                if &meta.name != only {
                    continue;
                }
            }
            if !meta.valid {
                continue;
            }
            let Some(text) = specs::read(&dir, &meta.name).map_err(|e| bad(e))? else {
                continue;
            };
            let sp = Spec::parse(&text).map_err(|e| bad(e))?;
            let mut r = sp
                .resolve_all(&state.cfg.pve.wan_bridge)
                .map_err(|e| bad(e))?;
            // 回填 DB 里已分配的 vmid（只读，不分配）：图里要能显示真实号
            for z in &mut r.zones {
                for h in &mut z.hosts {
                    if h.vmid.is_none() {
                        h.vmid = state.db.vmid_of(&z.name, &h.name);
                    }
                }
            }
            resolved.push((meta.name.clone(), r));
        }
        if resolved.is_empty() {
            return Err(err(StatusCode::NOT_FOUND, "没有可用的规格"));
        }

        let vms = state
            .pve
            .cluster_vms(&CancellationToken::new())
            .await
            .unwrap_or_default();
        let status_fn = |vmid: u32| {
            vms.iter()
                .find(|r| r.is_lxc() && r.vmid_parsed() == Some(vmid))
                .and_then(|r| r.status.clone())
        };
        let retired_fn = |z: &str, h: &str| state.db.is_retired(z, h);
        let st = crate::api::topology::State {
            wan_bridge: &state.cfg.pve.wan_bridge,
            status: &status_fn,
            retired: &retired_fn,
        };
        let (nodes, edges) = crate::api::topology::build(&resolved, &st);
        Ok(Json(serde_json::json!({
            "nodes": nodes,
            "edges": edges,
            "specs": resolved.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        })))
    })
    .await
}

// ───────────────────────── 镜像连线 ─────────────────────────

pub async fn list_mirrors(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<SpecQuery>,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        let items = collect_mirrors(&state, q.spec.as_deref()).map_err(|e| e)?;
        let reconcile = state.reconcile.lock().await.clone();
        let units = mirror_unit_states(&state).await;
        // 每行带上实例的现实状态：没查到 = 单元未安装
        let items: Vec<_> = items
            .into_iter()
            .map(|mut row| {
                let active = row
                    .get("instance")
                    .and_then(|v| v.as_str())
                    .and_then(|i| units.get(i))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "missing".into());
                row["active"] = serde_json::json!(active);
                row
            })
            .collect();
        Ok(Json(serde_json::json!({
            "mirrors": items,
            "reconcile": reconcile,
            "units": units,
        })))
    })
    .await
}

#[derive(Deserialize, Default)]
pub struct SpecQuery {
    #[serde(default)]
    pub spec: Option<String>,
}

/// 解析后一共有几条镜像连线。
///
/// 用来回答"这条连线到底删掉没有"：连线可能同时由顶层 `mirrors:` 与主机上的
/// `mirror: source/target` 生成，`remove_mirror` 只动前者。比较删前删后的条数，
/// 就能判断是否需要告诉用户"你去改主机那一行"。
fn count_links(state: &AppState, yaml: &str) -> Option<usize> {
    let sp = Spec::parse(yaml).ok()?;
    let resolved = sp.resolve_all(&state.cfg.pve.wan_bridge).ok()?;
    Some(resolved.mirrors.len())
}

fn collect_mirrors(state: &AppState, only: Option<&str>) -> ApiResult<Vec<serde_json::Value>> {
    let dir = spec_dir(state);
    let mut out = Vec::new();
    for meta in specs::list(&dir, &state.cfg.pve.wan_bridge) {
        if let Some(n) = only {
            if meta.name != n {
                continue;
            }
        }
        let Ok(Some(text)) = specs::read(&dir, &meta.name) else {
            continue;
        };
        let Ok(sp) = Spec::parse(&text) else { continue };
        let Ok(resolved) = sp.resolve_all(&state.cfg.pve.wan_bridge) else {
            continue;
        };
        for m in &resolved.mirrors {
            // 实例名要与执行器/巡检完全一致：{源zone}-{源vmid}-{目标vmid}。
            // vmid 优先用规格里写死的；没写就用 DB 里的分配号（与 build_plan
            // 同一条路径，幂等）——线上规格大多不写 vmid，全靠 DB。
            let base = state.cfg.pve.vmid_base;
            let vmid_of = |z: &str, h: &str| {
                resolved
                    .zones
                    .iter()
                    .find(|x| x.name == z)
                    .and_then(|x| x.hosts.iter().find(|hh| hh.name == h))
                    .map(|hh| hh.vmid.unwrap_or_else(|| state.db.vmid_for(z, h, base)))
            };
            let (src, dst) = (
                vmid_of(&m.from_zone, &m.from_host),
                vmid_of(&m.to_zone, &m.to_host),
            );
            let instance = match (src, dst) {
                (Some(s), Some(d)) => Some(format!("{}-{s}-{d}", m.from_zone)),
                _ => None,
            };
            out.push(serde_json::json!({
                "spec": meta.name,
                "from": format!("{}:{}", m.from_zone, m.from_host),
                "to": format!("{}:{}", m.to_zone, m.to_host),
                "src_bridge": m.src_bridge,
                "capture_bridge": m.capture_bridge,
                "source_iface": "eth0",
                "target_iface": m.to_iface,
                "instance": instance,
            }));
        }
    }
    Ok(out)
}

/// 宿主上 `labmirror@` 实例的运行状态：实例名 → active / inactive / failed。
///
/// 前端用它是为了回答"网络里哪些口真的在抓、哪些掉了"——规格是意图，
/// systemctl 是现实，两边都对得上才算健康。
async fn mirror_unit_states(state: &Arc<AppState>) -> BTreeMap<String, String> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let spec = crate::core::runner::CommandSpec::new(
        "sh",
        ["-c", "for f in /etc/labyard/mirror/*.conf; do [ -e \"$f\" ] || continue; n=$(basename \"$f\" .conf); printf '%s\\t%s\\n' \"$n\" \"$(systemctl is-active \"labmirror@$n\" 2>/dev/null)\"; done"],
    )
    .with_timeout(std::time::Duration::from_secs(15));
    let mut map = BTreeMap::new();
    if let Ok(out) = state
        .runner
        .run("mirrors:unit-states", &spec, None, &cancel, true)
        .await
    {
        for line in out.stdout.lines() {
            if let Some((n, s)) = line.split_once('\t') {
                map.insert(n.trim().to_string(), s.trim().to_string());
            }
        }
    }
    map
}

#[derive(Deserialize)]
pub struct MirrorReq {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub spec: Option<String>,
}

pub async fn add_mirror(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<MirrorReq>,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        // `from` 里写了 zone 就用它定位规格；否则按主机名找
        let zone_hint = req.from.split_once(':').map(|(z, _)| z.to_string());
        let (name, cur) = match (&req.spec, &zone_hint) {
            (Some(s), _) => (
                s.clone(),
                specs::read(&spec_dir(&state), s)
                    .map_err(|e| bad(e))?
                    .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("规格 `{s}` 不存在")))?,
            ),
            (None, Some(z)) => locate_zone(&state, z)?,
            (None, None) => return Err(bad("from 请写成 zone:host（多份规格时无法推断）")),
        };
        let next = specs::add_mirror(&cur, &req.from, &req.to).map_err(|e| bad(e))?;
        save_spec(&state, &name, &next)?;

        // 立刻把镜像实例装起来，而不是等下一次全量 apply：用户刚加的连线，
        // 打开「主机」页看不到 `labmirror@` 单元只会以为没生效。
        //
        // 阶段要带上 Bridge 与 Container：新连线的目标监控机可能第一次接这个
        // 网络，它需要一块新的抓包网卡、以及对应的两层桥。只跑 Mirror 阶段会
        // 拿到一个"桥不存在、网口不存在"的单元，而单元自己会静默退出。
        // 全量计划（不限 zone）里截取三个阶段：镜像连线可能跨 zone，
        // 目标监控机还可能在外网 zone，只截单 zone 会漏掉另一半。
        let run_id = match build_plan(&state, &next, false).map_err(|e| bad(e)) {
            Ok((_, mut plan)) => {
                plan.steps.retain(|s| {
                    matches!(s.phase, Phase::Bridge | Phase::Container | Phase::Mirror)
                });
                if plan.steps.is_empty() {
                    None
                } else {
                    Some(spawn_plan(&state, &plan, "mirror:add").await)
                }
            }
            // 规格已经写进去了：实例起不来是运行态问题，不该把写入回滚掉
            Err(_) => None,
        };
        Ok(Json(serde_json::json!({ "ok": true, "spec": name, "run_id": run_id })))
    })
    .await
}

pub async fn delete_mirror(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        // id 形如 `main:web->main:analysis`，与前端展示的一致，不用额外映射表
        let (from, to) = id
            .split_once("->")
            .ok_or_else(|| bad("id 应为 `from->to`"))?;
        let zone = from
            .split_once(':')
            .map(|(z, _)| z)
            .ok_or_else(|| bad("from 请写成 zone:host"))?;
        let (name, cur) = locate_zone(&state, zone)?;
        let next = specs::remove_mirror(&cur, from, to).map_err(|e| bad(e))?;
        // 一条连线有两个来源：顶层 `mirrors:`，以及主机上的 `mirror: source/target`。
        // `remove_mirror` 只删前者；如果删完连线还在（后者仍在生成它），必须报错——
        // 否则接口回 200、刷新后连线又出现，用户会以为平台在骗人。
        let before = count_links(&state, &cur);
        let after = count_links(&state, &next);
        if let (Some(b), Some(a)) = (before, after) {
            if a >= b {
                return Err(bad(format!(
                    "连线 {from} → {to} 是主机上的 `mirror:` 字段生成的，\
                     删顶层 mirrors 里的条目没用——请到规格里把对应主机的 mirror 去掉"
                )));
            }
        }
        save_spec(&state, &name, &next)?;
        // 规格里删了，宿主上的实例也要停——否则镜像还在跑，只是平台不知道了
        let sp = Spec::parse(&next).map_err(|e| bad(e))?;
        let resolved = sp
            .resolve_all(&state.cfg.pve.wan_bridge)
            .map_err(|e| bad(e))?;
        let src_zone = resolved.zones.iter().find(|z| z.name == zone);
        if let Some(sz) = src_zone {
            let src_host = from.split_once(':').map(|(_, h)| h).unwrap_or("");
            let dst_host = to.split_once(':').map(|(_, h)| h).unwrap_or("");
            let db = state.db.clone();
            let zv = db.vmid_of(zone, src_host).or_else(|| {
                sz.hosts
                    .iter()
                    .find(|h| h.name == src_host)
                    .and_then(|h| h.vmid)
            });
            let dv = to
                .split_once(':')
                .and_then(|(tz, th)| db.vmid_of(tz, th))
                .or_else(|| {
                    resolved
                        .zones
                        .iter()
                        .find(|z| z.name == to.split_once(':').map(|(z, _)| z).unwrap_or(""))
                        .and_then(|z| z.hosts.iter().find(|h| h.name == dst_host))
                        .and_then(|h| h.vmid)
                });
            if let (Some(s), Some(t)) = (zv, dv) {
                let ex = Executor::new(state.pve.clone(), state.db.clone(), state.runner.clone());
                let _ = ex.remove_mirror_instance(zone, s, t, &CancellationToken::new()).await;
            }
        }
        Ok(Json(serde_json::json!({ "ok": true, "spec": name })))
    })
    .await
}

// ───────────────────────── retire / revive ─────────────────────────

pub async fn retire_zone(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(zone): Path<String>,
) -> Response {
    render(do_retire(state, headers, zone, "*".to_string()).await)
}

pub async fn retire_host(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((zone, host)): Path<(String, String)>,
) -> Response {
    render(do_retire(state, headers, zone, host).await)
}

async fn do_retire(
    state: Arc<AppState>,
    headers: axum::http::HeaderMap,
    zone: String,
    host: String,
) -> ApiResult<Json<serde_json::Value>> {
    authed(&state, &headers)?;
    // 起停容器会改状态，先清读缓存；失败路径也清，免得留着半截状态
    state.pve.clear_cache();
    let (spec_name, cur) = locate_zone(&state, &zone)?;
    let sp = Spec::parse(&cur).map_err(|e| bad(e))?;
    let z = sp
        .resolve(&state.cfg.pve.wan_bridge)
        .map_err(|e| bad(e))?
        .into_iter()
        .find(|z| z.name == zone)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("规格里没有 zone `{zone}`")))?;

    let targets: Vec<&crate::core::spec::ResolvedHost> = z
        .hosts
        .iter()
        .filter(|h| host == "*" || h.name == host)
        .collect();
    if targets.is_empty() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("zone `{zone}` 里没有主机 `{host}`"),
        ));
    }

    let cancel = CancellationToken::new();
    let mut stopped = Vec::new();
    let mut absent = Vec::new();
    // 单台主机废弃时记下它的 vmid：镜像单元名里要按它匹配（见 mirror_unit_globs）
    let mut retire_vmid: Option<u32> = None;
    for h in &targets {
        let vmid = h.vmid.or_else(|| state.db.vmid_of(&zone, &h.name));
        let Some(vmid) = vmid else {
            absent.push(h.name.clone());
            continue;
        };
        if host != "*" {
            retire_vmid = Some(vmid);
        }
        match state.pve.ct_status(vmid, &cancel).await {
            Ok(Some(st)) if st == "running" => {
                state
                    .pve
                    .ct_action(vmid, "stop", &cancel)
                    .await
                    .map_err(|e| internal(format!("停 {} 失败: {e}", h.name)))?;
                stopped.push(h.name.clone());
            }
            Ok(_) => absent.push(format!("{}（本来就停着）", h.name)),
            Err(e) => return Err(internal(format!("查 {} 状态失败: {e}", h.name))),
        }
        // 步骤记录清掉：revive 时 gateway/mirror 会真的重跑一遍
        state.db.forget_step(&format!("gateway:{zone}"));
        if host == "*" {
            state
                .db
                .forget_steps_matching(&format!("mirror:{zone}:%->%"));
            state
                .db
                .forget_steps_matching(&format!("mirror:%->{zone}:%"));
        } else {
            state
                .db
                .forget_steps_matching(&format!("mirror:{zone}:{host}->%"));
            state
                .db
                .forget_steps_matching(&format!("mirror:%->{zone}:{host}"));
        }
    }

    // 摘掉镜像实例：废弃的机器不该继续往监控机里灌流量
    let ex = Executor::new(state.pve.clone(), state.db.clone(), state.runner.clone());
    let stopped_units = ex
        .stop_mirror_instances(&mirror_unit_globs(&zone, &host, retire_vmid), &cancel)
        .await
        .unwrap_or_default();

    state.db.retire(&zone, &host, None);
    Ok(Json(serde_json::json!({
        "ok": true,
        "spec": spec_name,
        "stopped": stopped,
        "absent": absent,
        "mirror_units_stopped": stopped_units,
        "note": "vmid、磁盘、快照、IP 占用都保留；revive 可原样回来",
    })))
}

pub async fn revive_zone(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(zone): Path<String>,
) -> Response {
    render(do_revive(state, headers, zone, "*".to_string()).await)
}

pub async fn revive_host(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((zone, host)): Path<(String, String)>,
) -> Response {
    render(do_revive(state, headers, zone, host).await)
}

async fn do_revive(
    state: Arc<AppState>,
    headers: axum::http::HeaderMap,
    zone: String,
    host: String,
) -> ApiResult<Json<serde_json::Value>> {
    authed(&state, &headers)?;
    state.pve.clear_cache();
    let (_, cur) = locate_zone(&state, &zone)?;
    let sp = Spec::parse(&cur).map_err(|e| bad(e))?;
    let z = sp
        .resolve(&state.cfg.pve.wan_bridge)
        .map_err(|e| bad(e))?
        .into_iter()
        .find(|z| z.name == zone)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("规格里没有 zone `{zone}`")))?;

    let cancel = CancellationToken::new();
    let mut started = Vec::new();
    // 和 do_retire 一样先确认主机真的在规格里：主机名写错时若直接返回
    // `{ok:true, started:[]}`，界面会显示"已恢复"而实际什么也没做。
    let targets: Vec<&crate::core::spec::ResolvedHost> = z
        .hosts
        .iter()
        .filter(|h| host == "*" || h.name == host)
        .collect();
    if targets.is_empty() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("zone `{zone}` 里没有主机 `{host}`"),
        ));
    }
    for h in targets {
        let vmid = h.vmid.or_else(|| state.db.vmid_of(&zone, &h.name));
        let Some(vmid) = vmid else { continue };
        // 已经 running 的不要重复 start（会报错，让整套 revive 失败）
        if let Ok(Some(st)) = state.pve.ct_status(vmid, &cancel).await {
            if st != "running" {
                state
                    .pve
                    .ct_action(vmid, "start", &cancel)
                    .await
                    .map_err(|e| internal(format!("启动 {} 失败: {e}", h.name)))?;
                started.push(h.name.clone());
            }
        }
    }
    state.db.unretire(&zone, &host);

    // 重新对账：gateway / mirror 的步骤记录在 retire 时被清掉了，
    // 这一步会把策略与镜像实例原样装回去；其余步骤哈希未变，直接跳过。
    let plan = gateway_step_plan(&state, &zone, &cur).await.ok();
    let run_id = match plan {
        Some(p) => Some(spawn_plan(&state, &p, &format!("revive:{zone}")).await),
        None => None,
    };
    Ok(Json(serde_json::json!({
        "ok": true,
        "started": started,
        "run_id": run_id,
    })))
}

/// 废弃主机时要停掉哪些镜像单元（宿主侧 `/etc/labyard/mirror/*.conf` 的 glob）。
///
/// 单元名是 `{zone}-{源vmid}-{目标vmid}`，于是两支都要照顾：
/// - 废弃**源机** → 前缀匹配 `lab-102-*`；
/// - 废弃**监控机（目标端）** → 后缀匹配 `lab-*-100`。只按前缀做的话，
///   retire 一台监控机会"成功"但单元还在跑，流量照旧灌进已停掉的容器。
/// - 整个 zone、或查不到 vmid → `lab-*`，宁可多停几个（下一次 apply 会按
///   当前规格重新装回来），也不能留下"平台说废弃了、实际还在跑"。
fn mirror_unit_globs(zone: &str, host: &str, vmid: Option<u32>) -> Vec<String> {
    match (host == "*", vmid) {
        (false, Some(v)) => vec![format!("{zone}-{v}-*"), format!("{zone}-*-{v}")],
        _ => vec![format!("{zone}-*")],
    }
}

/// 资源目录：可视化编排的下拉数据源。
///
/// 模板按存储分组返回，而不是一张平表——同一个模板名可能存在于多个
/// 存储上，`template` + `template_storage` 是一对，平表丢掉配对信息。
/// 请求代价有 TTL 缓存兜底（storage 120s / content 15s），反复打开
/// 编排页不会打爆 PVE。
pub async fn catalog(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    render_async(async {
        authed(&state, &headers)?;
        let cancel = CancellationToken::new();
        let all = state
            .pve
            .storages(&cancel)
            .await
            .map_err(internal)?;
        // 只要有 vztmpl 能力的存储才值得出现在"模板存储"下拉里；
        // rootfs 存储单独一组，两种用途不混。
        let mut tpl_storages = Vec::new();
        let mut disk_storages = Vec::new();
        let mut templates = Vec::new();
        for s in &all {
            let active = s.active.unwrap_or(0) == 1;
            if !active {
                continue;
            }
            let content = s.content.as_deref().unwrap_or("");
            let can_tpl = content.split(',').any(|c| c.trim() == "vztmpl");
            let can_disk = content.split(',').any(|c| c.trim() == "rootdir" || c.trim() == "images");
            if can_disk {
                disk_storages.push(serde_json::json!({
                    "storage": s.storage,
                    "kind": s.kind,
                    "avail": s.avail,
                    "total": s.total,
                }));
            }
            if can_tpl {
                tpl_storages.push(serde_json::json!({
                    "storage": s.storage,
                    "kind": s.kind,
                }));
                let vols = state
                    .pve
                    .templates_on(&s.storage, &cancel)
                    .await
                    .unwrap_or_default();
                for v in vols {
                    // volid 形如 local:vztmpl/debian-12-standard...，剥出文件名给 UI 显示
                    let label = v.rsplit('/').next().unwrap_or(&v).to_string();
                    templates.push(serde_json::json!({
                        "storage": s.storage,
                        "volid": v,
                        "label": label,
                    }));
                }
            }
        }
        Ok(Json(serde_json::json!({
            "template_storages": tpl_storages,
            "disk_storages": disk_storages,
            "templates": templates,
        })))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retiring_a_host_matches_it_as_source_and_as_target() {
        let g = mirror_unit_globs("lab", "t1", Some(102));
        assert_eq!(g, vec!["lab-102-*".to_string(), "lab-*-102".to_string()]);
    }

    #[test]
    fn retiring_the_analysis_host_matches_by_suffix() {
        // 真机上撞出来的：analysis 只出现在单元名的**后缀**位置，
        // 只按前缀匹配会漏掉它，镜像单元继续跑
        let g = mirror_unit_globs("lab", "cache", Some(100));
        assert!(
            g.iter().any(|p| p == "lab-*-100"),
            "废弃监控机必须能匹配目标端: {g:?}"
        );
    }

    #[test]
    fn retiring_a_whole_zone_uses_one_prefix() {
        assert_eq!(mirror_unit_globs("lab", "*", Some(101)), vec!["lab-*"]);
        assert_eq!(mirror_unit_globs("lab", "*", None), vec!["lab-*"]);
    }

    #[test]
    fn unknown_vmid_falls_back_to_the_whole_zone() {
        // 查不到 vmid 就保守处理：多停几个总比留下孤儿单元好
        assert_eq!(mirror_unit_globs("lab", "t1", None), vec!["lab-*"]);
    }
}
