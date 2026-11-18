//! 资源执行器：把 `Plan` 里的步骤落到真实系统上。
//!
//! 每个步骤走三段：**probe（现状）→ decide（是否要做）→ apply（做）**。
//! 判定依据是 (记录里的 input_hash == 本次的 input_hash) **且** 资源确实还在——
//! 只信记录会在"手工删了容器"之后谎报已完成，只信现状则改参数不会重跑。

pub mod assets;
pub mod net;

use crate::core::plan::{Phase, Step, StepKind};
use crate::core::runner::{CommandSpec, Event, Runner};
use crate::db::Db;
use crate::pve::Pve;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct Executor {
    pub pve: Arc<Pve>,
    pub db: Arc<Db>,
    pub runner: Arc<dyn Runner>,
}

/// 开始执行前拉一次的现状快照。
///
/// **只拉一次**是硬约束：`/etc/pve` 是单线程 FUSE，20 台机各查一次会把节点拖慢。
/// 执行过程中新建的资源直接更新进这个结构，不再回查。
#[derive(Debug, Default)]
pub struct Snapshot {
    pub vmids: HashSet<u32>,
    pub bridges: HashSet<String>,
    /// (storage, 模板简名) → 完整 volid
    pub templates: HashMap<(String, String), Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 已完成且输入未变
    Done,
    /// 需要执行，附上原因（给 UI 显示"为什么又跑了一次"）
    Apply(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Skipped,
    Applied,
    Failed,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub total: usize,
    pub skipped: usize,
    pub applied: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

impl Summary {
    pub fn ok(&self) -> bool {
        self.failed == 0
    }
}

impl Executor {
    pub fn new(pve: Arc<Pve>, db: Arc<Db>, runner: Arc<dyn Runner>) -> Self {
        Self { pve, db, runner }
    }

    /// 拉一次现状。模板需要逐 storage 查，但每个 storage 只查一次。
    pub async fn load_snapshot(
        &self,
        pairs: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<Snapshot, crate::pve::PveError> {
        let mut snap = Snapshot::default();

        for r in self.pve.cluster_vms(cancel).await? {
            if r.is_lxc() {
                if let Some(v) = r.vmid_parsed() {
                    snap.vmids.insert(v);
                }
            }
        }
        for b in self.pve.bridges(cancel).await? {
            snap.bridges.insert(b.iface);
        }

        let mut storages: Vec<String> = pairs.iter().map(|(s, _)| s.clone()).collect();
        storages.sort();
        storages.dedup();
        for storage in storages {
            let vols = self
                .pve
                .templates_on(&storage, cancel)
                .await
                .unwrap_or_default();
            for (s, t) in pairs.iter().filter(|(s2, _)| s2 == &storage) {
                let found = vols.iter().find(|v| v.contains(t.as_str())).cloned();
                snap.templates.insert((s.clone(), t.clone()), found);
            }
        }
        Ok(snap)
    }

    /// 纯判定：不发任何命令，只比对记录与快照。方便单测。
    pub fn decide(&self, step: &Step, snap: &Snapshot) -> Decision {
        let recorded = self.db.step_hash(&step.key);
        let hash_ok = recorded.as_deref() == Some(step.input_hash.as_str());
        if !hash_ok {
            return Decision::Apply(if recorded.is_some() {
                "参数已变化"
            } else {
                "从未执行"
            });
        }
        let exists = match &step.kind {
            StepKind::Bridge { name, .. } => snap.bridges.contains(name),
            StepKind::Container { vmid, .. } => snap.vmids.contains(vmid),
            StepKind::Template { storage, template } => snap
                .templates
                .get(&(storage.clone(), template.clone()))
                .map(|v| v.is_some())
                .unwrap_or(true),
            // Net / Bootstrap / Mirror / Snapshot 的"存在"检查需要进容器或查快照，
            // 代价高且不常被人手工破坏，这里信任记录。
            _ => true,
        };
        if exists {
            Decision::Done
        } else {
            Decision::Apply("记录存在但资源已消失")
        }
    }

    /// 执行整份计划。阶段顺序固定，阶段内并发（受 `Phase::weight` 约束）。
    pub async fn execute(
        &self,
        plan: &crate::core::plan::Plan,
        run_id: &str,
        tx: mpsc::Sender<Event>,
        cancel: CancellationToken,
        concurrency: usize,
    ) -> Summary {
        let pairs: Vec<(String, String)> = plan
            .steps
            .iter()
            .filter_map(|s| match &s.kind {
                StepKind::Template { storage, template } => Some((storage.clone(), template.clone())),
                _ => None,
            })
            .collect();

        let snap = match self.load_snapshot(&pairs, &cancel).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tx
                    .send(Event::Failed {
                        key: "probe".into(),
                        error: format!("无法读取集群现状: {e}"),
                    })
                    .await;
                return Summary {
                    failed: 1,
                    errors: vec![("probe".into(), e.to_string())],
                    ..Default::default()
                };
            }
        };

        let mut summary = Summary {
            total: plan.steps.len(),
            ..Default::default()
        };
        let snap = Arc::new(tokio::sync::Mutex::new(snap));

        for phase in [
            Phase::Bridge,
            Phase::Template,
            Phase::Container,
            Phase::Net,
            Phase::Bootstrap,
            Phase::Mirror,
            Phase::Snapshot,
        ] {
            let steps: Vec<Step> = plan.by_phase(phase).cloned().collect();
            if steps.is_empty() {
                continue;
            }
            // 建容器最重：并发压低；建桥/网络很轻：可以放宽。
            let limit = match phase {
                Phase::Bridge | Phase::Template | Phase::Mirror => 1,
                Phase::Container | Phase::Bootstrap => concurrency.max(1),
                _ => concurrency.max(1) * 2,
            };

            let results = self
                .run_phase(steps, limit, run_id, &tx, &cancel, &snap)
                .await;

            for (key, state, err) in results {
                match state {
                    StepState::Skipped => summary.skipped += 1,
                    StepState::Applied => summary.applied += 1,
                    StepState::Failed => {
                        summary.failed += 1;
                        if let Some(e) = err {
                            summary.errors.push((key, e));
                        }
                    }
                }
            }

            if cancel.is_cancelled() {
                break;
            }
        }

        let _ = tx
            .send(Event::Finished {
                key: format!("run:{run_id}"),
                code: Some(if summary.failed == 0 { 0 } else { 1 }),
                duration_ms: 0,
            })
            .await;
        summary
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_phase(
        &self,
        steps: Vec<Step>,
        limit: usize,
        run_id: &str,
        tx: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
        snap: &Arc<tokio::sync::Mutex<Snapshot>>,
    ) -> Vec<(String, StepState, Option<String>)> {
        use futures_util::stream::{self, StreamExt};

        let results: Vec<(String, StepState, Option<String>)> = stream::iter(steps)
            .map(|step| {
                let pve = self.pve.clone();
                let db = self.db.clone();
                let runner = self.runner.clone();
                let tx = tx.clone();
                let cancel = cancel.clone();
                let snap = snap.clone();
                let run_id = run_id.to_string();
                async move {
                    execute_one(pve, db, runner, step, &run_id, tx, cancel, snap).await
                }
            })
            .buffer_unordered(limit)
            .collect()
            .await;

        results
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_one(
    pve: Arc<Pve>,
    db: Arc<Db>,
    runner: Arc<dyn Runner>,
    step: Step,
    run_id: &str,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
    snap: Arc<tokio::sync::Mutex<Snapshot>>,
) -> (String, StepState, Option<String>) {
    if cancel.is_cancelled() {
        return (step.key, StepState::Skipped, None);
    }

    // probe
    let decision = {
        let guard = snap.lock().await;
        let ex = Executor {
            pve: pve.clone(),
            db: db.clone(),
            runner: runner.clone(),
        };
        ex.decide(&step, &guard)
    };

    if decision == Decision::Done {
        db.record_run_step(run_id, &step.key, "skipped", None);
        let _ = tx
            .try_send(Event::Line {
                key: step.key.clone(),
                stream: crate::core::runner::Stream::Stdout,
                line: format!("[跳过] {} —— 已完成且参数未变", step.key),
            });
        return (step.key, StepState::Skipped, None);
    }

    let reason = match decision {
        Decision::Apply(r) => r,
        Decision::Done => "",
    };
    let _ = tx
        .try_send(Event::Started {
            key: step.key.clone(),
            command: format!("{}（{}）", step.key, reason),
            pid: 0,
        });

    let result = match apply_step(&pve, &runner, &step, &tx, &cancel).await {
        Ok(()) => Ok(()),
        Err(e) => Err(e),
    };

    match result {
        Ok(()) => {
            db.record_step(&step.key, &step.input_hash, "done", None);
            db.record_run_step(run_id, &step.key, "done", None);
            // 更新快照：后续步骤能看到刚建出来的东西
            {
                let mut g = snap.lock().await;
                match &step.kind {
                    StepKind::Bridge { name, .. } => {
                        g.bridges.insert(name.clone());
                    }
                    StepKind::Container { vmid, .. } => {
                        g.vmids.insert(*vmid);
                    }
                    StepKind::Template { storage, template } => {
                        g.templates
                            .insert((storage.clone(), template.clone()), Some(template.clone()));
                    }
                    _ => {}
                }
            }
            (step.key, StepState::Applied, None)
        }
        Err(e) => {
            let msg = e.to_string();
            db.record_step(&step.key, &step.input_hash, "failed", Some(&msg));
            db.record_run_step(run_id, &step.key, "failed", Some(&msg));
            let _ = tx.try_send(Event::Failed {
                key: step.key.clone(),
                error: msg.clone(),
            });
            (step.key, StepState::Failed, Some(msg))
        }
    }
}

async fn apply_step(
    pve: &Pve,
    runner: &Arc<dyn Runner>,
    step: &Step,
    tx: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match &step.kind {
        StepKind::Bridge { name, .. } => {
            pve.ensure_bridge(name, cancel).await?;
        }

        StepKind::Template { storage, template } => {
            pve.ensure_template(storage, template, cancel).await?;
        }

        StepKind::Container {
            vmid,
            hostname,
            cores,
            memory,
            disk,
            template,
            storage,
            bridge,
            uplink_bridge,
            ssh_keys,
            password,
            ..
        } => {
            // 模板的完整 volid 从快照里取（probe 阶段已经查过）
            let vols = pve.templates_on(storage, cancel).await?;
            let vol = vols
                .iter()
                .find(|v| v.contains(template.as_str()))
                .ok_or_else(|| format!("存储 {storage} 上找不到模板 {template}"))?;
            pve.ensure_ct(
                *vmid,
                hostname,
                *cores,
                *memory,
                *disk,
                storage,
                vol,
                bridge,
                uplink_bridge.as_deref(),
                password.as_deref(),
                ssh_keys,
                cancel,
            )
            .await?;
        }

        StepKind::Net {
            vmid,
            host,
            ipv4,
            gateway,
            ipv6_prefix,
            is_gateway,
            default_route,
            nat,
            ..
        } => {
            let script = if *is_gateway {
                net::gateway_script(&zone_cidr_from(ipv4), ipv6_prefix.as_deref(), "eth1", *nat)
            } else {
                net::host_script(ipv4, gateway.as_deref(), ipv6_prefix.as_deref(), *default_route)
            };
            pve.ct_exec(
                &format!("net:{host}"),
                *vmid,
                &["sh", "-c", &script],
                Duration::from_secs(120),
                cancel,
            )
            .await?;
        }

        StepKind::Bootstrap {
            vmid,
            provisioner,
            args,
            ..
        } => {
            let base = assets::Assets::text("base/bootstrap.sh")
                .ok_or("内置 bootstrap.sh 缺失（编译产物不完整）")?;
            let mut env = String::new();
            for (k, v) in args {
                env.push_str(&format!(
                    "export LABYARD_{}=\"{}\"\n",
                    k.to_uppercase().replace('-', "_"),
                    v.replace('"', "\\\"")
                ));
            }
            let script = format!("{env}\n{base}");
            pve.push_and_run(
                &format!("bootstrap:{vmid}"),
                *vmid,
                &script,
                Duration::from_secs(900),
                cancel,
            )
            .await?;

            if let Some(p) = assets::Assets::text(&format!("provisioners/{provisioner}/install.sh")) {
                pve.push_and_run(
                    &format!("provision:{vmid}"),
                    *vmid,
                    &p,
                    Duration::from_secs(1800),
                    cancel,
                )
                .await?;
            } else if provisioner != "none" {
                return Err(format!("找不到 provisioner `{provisioner}`").into());
            }
        }

        StepKind::Mirror {
            zone,
            source_vmid,
            target_vmid,
            target_iface,
            ..
        } => {
            // 宿主侧：写配置 + 起 systemd 模板实例
            let unit = format!("labmirror@{zone}-{source_vmid}-{target_vmid}");
            let conf_dir = "/etc/labyard/mirror";
            let conf = format!(
                "SOURCE_VMID={source_vmid}\nTARGET_VMID={target_vmid}\nTARGET_IFACE={target_iface}\n"
            );
            let script = format!(
                "set -e; mkdir -p {conf_dir}; printf '%s' '{conf}' > {conf_dir}/{unit}.conf; \
                 systemctl enable --now {unit}.service 2>/dev/null || systemctl restart {unit}.service"
            );
            runner
                .run(
                    &format!("mirror:{unit}"),
                    &CommandSpec::new("sh", ["-c", &script]).with_timeout(Duration::from_secs(60)),
                    Some(tx.clone()),
                    cancel,
                    false,
                )
                .await?;
        }

        StepKind::Snapshot { vmid, name, .. } => {
            pve.ensure_snapshot(*vmid, name, cancel).await?;
        }
    }
    Ok(())
}

/// `10.10.1.1/24` → `10.10.1.0/24`
fn zone_cidr_from(ip_with_len: &str) -> String {
    let (ip, len) = match ip_with_len.split_once('/') {
        Some((a, b)) => (a, b),
        None => (ip_with_len, "24"),
    };
    let mut parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 {
        parts[3] = "0";
        format!("{}/{}", parts.join("."), len)
    } else {
        format!("{ip}/{len}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_cidr_normalises_last_octet() {
        assert_eq!(zone_cidr_from("10.10.1.1/24"), "10.10.1.0/24");
        assert_eq!(zone_cidr_from("192.168.5.254/16"), "192.168.5.0/16");
    }
}
