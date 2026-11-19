//! 后台镜像巡检：让"规格里说该有的镜像"和"宿主上真的在跑的镜像"长期一致。
//!
//! 手工改规格、apply 中途失败、容器重建……任何一种都能让镜像实例掉队：
//! 规格里写着抓，宿主上单元没了（漏抓），或者规格早删了、单元还在跑（孤儿
//! 流量）。前端有刷新按钮，但"靠人想起来去点"不是设计，是运气。
//!
//! 所以这里做一个低频（5 分钟）对账循环：
//!
//! 1. 解析全部**合法**规格，算出期望的镜像实例集合（实例名 + conf 全文）。
//!    有一份规格解析失败就整轮跳过——用户正在编辑到一半的 YAML 不该被当成
//!    "删除所有镜像"的指令。
//! 2. 读取宿主上真实的 `/etc/labyard/mirror/*.conf`。
//! 3. 差异 = 缺失（期望有、实际没有 / conf 内容变了）→ 只重跑那些 Mirror
//!    步骤；孤儿（实际有、期望没有）→ 停掉并删配置。
//! 4. 只补装**容器已建好**且两端都没 retire 的连线：容器都不存在的 spec
//!    （从没 apply 过）缺镜像不是漂移，是还没轮到它。
//!
//! 与 run 的互斥：`run_gate` 没有空闲许可（正在 apply）就本轮跳过。巡检自己
//! 修漂移时通过 `spawn_plan` 拿同一个闸，不会跟用户的操作叠跑。

use super::{spawn_plan, AppState};
use crate::core::plan::{Plan, Phase, StepKind};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// 巡检周期。实验环境里主机增减不频繁，5 分钟足够"自动"；
/// 手动动作（开关监控、加连线）本身就带一次落地，不依赖这个周期。
const INTERVAL: Duration = Duration::from_secs(300);
/// 服务启动后第一轮巡检的延迟：把启动窗口让给 PVE 与首次登录
const FIRST_DELAY: Duration = Duration::from_secs(90);

/// 一次巡检的结果，`/api/mirrors` 会带出去给前端显示。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReconcileInfo {
    /// 上次巡检完成时间（RFC3339）；null = 还没跑过
    #[serde(rename = "at")]
    pub at: Option<String>,
    /// 补装了几个镜像实例
    pub applied: usize,
    /// 清掉了几个孤儿单元
    pub removed: usize,
    /// 本轮没跑的原因（正在 apply / 有规格解析失败）
    pub skipped: Option<String>,
    /// 巡检执行中出的错
    pub error: Option<String>,
}

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(tick_loop(state));
}

async fn tick_loop(state: Arc<AppState>) {
    tokio::time::sleep(FIRST_DELAY).await;
    let mut it = tokio::time::interval(INTERVAL);
    it.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        it.tick().await;
        let info = run_once(&state).await;
        *state.reconcile.lock().await = info;
    }
}

/// 宿主上当前安装的镜像配置：实例名 → conf 全文。
///
/// 读取输出是"实例名行 + 5 行 conf"交替出现的流；conf 的最后一行恒为
/// `TARGET_IFACE=`，用它作为一条记录结束的哨兵。
async fn read_installed(state: &Arc<AppState>) -> Result<BTreeMap<String, String>, String> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let out = state
        .runner
        .run(
            "reconcile:read-confs",
            &crate::core::runner::CommandSpec::new(
                "sh",
                ["-c", "for f in /etc/labyard/mirror/*.conf; do [ -e \"$f\" ] || continue; printf '%s\\n' \"$(basename \"$f\" .conf)\"; cat \"$f\"; done"],
            )
            .with_timeout(Duration::from_secs(15)),
            None,
            &cancel,
            true,
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut map = BTreeMap::new();
    let mut name: Option<String> = None;
    let mut buf = String::new();
    let mut collecting = false;
    for line in out.stdout.lines() {
        let t = line.trim_end();
        if collecting {
            buf.push_str(t);
            buf.push('\n');
            if t.starts_with("TARGET_IFACE=") {
                if let Some(n) = name.take() {
                    map.insert(n, buf.clone());
                }
                buf.clear();
                collecting = false;
            }
        } else if t.starts_with("SRC_BRIDGE=") {
            buf.clear();
            buf.push_str(t);
            buf.push('\n');
            collecting = true;
        } else if !t.is_empty() {
            name = Some(t.to_string());
        }
    }
    Ok(map)
}

async fn run_once(state: &Arc<AppState>) -> ReconcileInfo {
    let mut info = ReconcileInfo::default();

    // 有 run 在执行就不凑热闹：apply 本身就会把镜像装到位，巡检此刻读到的
    // "缺失"很可能只是执行到一半。
    if state.run_gate.available_permits() == 0 {
        info.skipped = Some("有规格正在执行".into());
        return info;
    }

    // ── 期望集合 ──
    let desired = match collect_desired(state) {
        Ok(d) => d,
        Err(reason) => {
            info.skipped = Some(reason);
            return info;
        }
    };

    // ── 实际集合 ──
    let actual = match read_installed(state).await {
        Ok(a) => a,
        Err(e) => {
            info.error = Some(format!("读取宿主镜像配置失败: {e}"));
            return info;
        }
    };

    // ── 孤儿：宿主上有、规格里没有（或 conf 内容对不上） ──
    let orphans: Vec<String> = actual
        .iter()
        .filter(|(i, conf)| desired.get(*i).map(|c| c != *conf).unwrap_or(true))
        .map(|(i, _)| i.clone())
        .collect();
    if !orphans.is_empty() {
        let executor = crate::resources::Executor::new(
            state.pve.clone(),
            state.db.clone(),
            state.runner.clone(),
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        if let Err(e) = executor.remove_mirror_instances(&orphans, &cancel).await {
            info.error = Some(format!("清理孤儿镜像单元失败: {e}"));
            return info;
        }
        info.removed = orphans.len();
        tracing::info!("巡检清理孤儿镜像单元: {orphans:?}");
    }

    // ── 缺失：规格里有、宿主上没有（或刚被当作孤儿停掉） ──
    let missing = collect_missing(state, &desired, &actual, &orphans);
    if missing.is_empty() {
        info.at = Some(now());
        return info;
    }
    info.applied = missing.len();
    tracing::info!(
        "巡检补装 {} 个镜像实例: {:?}",
        missing.len(),
        missing.iter().map(|s| s.key.clone()).collect::<Vec<_>>()
    );
    let plan = Plan { steps: missing };
    let run_id = spawn_plan(state, &plan, "巡检").await;
    tracing::info!("巡检 run: {run_id}");
    info.at = Some(now());
    info
}

/// 一条期望中的镜像实例：实例名 → conf 全文。conf 全文参与比较：
/// 参数变了（改了抓包桥、目标口）等于"内容漂移"，也要重装。
fn collect_desired(state: &Arc<AppState>) -> Result<BTreeMap<String, String>, String> {
    let dir = &state.cfg.paths.specs;
    let mut instances = BTreeMap::new();
    for meta in crate::api::specs::list(dir, &state.cfg.pve.wan_bridge) {
        if !meta.valid {
            return Err(format!("规格 `{}` 无法解析，本轮跳过", meta.name));
        }
        let Some(text) = crate::api::specs::read(dir, &meta.name).map_err(|e| e.to_string())?
        else {
            continue;
        };
        let (_, plan) = crate::api::build_plan(state, &text, false)
            .map_err(|e| format!("规格 `{}` 规划失败: {e}", meta.name))?;
        for s in plan.steps.iter().filter(|s| s.phase == Phase::Mirror) {
            let StepKind::Mirror {
                zone,
                bridge,
                capture_bridge,
                source_vmid,
                target_vmid,
                target_iface,
                ..
            } = &s.kind
            else {
                continue;
            };
            instances.insert(
                format!("{zone}-{source_vmid}-{target_vmid}"),
                format!(
                    "SRC_BRIDGE={bridge}\nCAPTURE_BRIDGE={capture_bridge}\nSOURCE_VMID={source_vmid}\nTARGET_VMID={target_vmid}\nTARGET_IFACE={target_iface}\n"
                ),
            );
        }
    }
    Ok(instances)
}

/// 从全部规格里挑出真正需要补装的 Mirror 步骤。
fn collect_missing(
    state: &Arc<AppState>,
    desired: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
    orphans: &[String],
) -> Vec<crate::core::plan::Step> {
    let dir = &state.cfg.paths.specs;
    let mut out = Vec::new();
    for meta in crate::api::specs::list(dir, &state.cfg.pve.wan_bridge) {
        if !meta.valid {
            continue;
        }
        let Some(text) = crate::api::specs::read(dir, &meta.name).ok().flatten() else {
            continue;
        };
        let Ok((_, plan)) = crate::api::build_plan(state, &text, false) else {
            continue;
        };
        for s in plan.steps.into_iter().filter(|s| s.phase == Phase::Mirror) {
            // key 形如 `mirror:lab:t1->lab:cache`，两端都是 zone:host
            let body = s.key.strip_prefix("mirror:").unwrap_or(&s.key);
            let Some((from, to)) = body.split_once("->") else { continue };
            let Some((fz, fh)) = from.split_once(':') else { continue };
            let Some((tz, th)) = to.split_once(':') else { continue };

            let StepKind::Mirror { zone, source_vmid, target_vmid, .. } = &s.kind else { continue };
            let instance = format!("{zone}-{source_vmid}-{target_vmid}");
            let installed_ok = actual
                .get(&instance)
                .zip(desired.get(&instance))
                .map(|(a, d)| a == d)
                .unwrap_or(false);
            if installed_ok && !orphans.contains(&instance) {
                continue;
            }
            // retire 过的主机不补：retire 的语义就是"摘镜像、保留磁盘"
            if state.db.is_retired(fz, fh) || state.db.is_retired(tz, th) {
                continue;
            }
            // 容器必须真的建过（DB 里有 ct 步骤记录），否则装出来的是废单元
            if state.db.step_hash(&format!("ct:{fz}:{fh}")).is_none()
                || state.db.step_hash(&format!("ct:{tz}:{th}")).is_none()
            {
                continue;
            }
            out.push(s);
        }
    }
    out
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
