//! 资源执行器：把 `Plan` 里的步骤落到真实系统上。
//!
//! 每个步骤走三段：**probe（现状）→ decide（是否要做）→ apply（做）**。
//! 判定依据是 (记录里的 input_hash == 本次的 input_hash) **且** 资源确实还在——
//! 只信记录会在"手工删了容器"之后谎报已完成，只信现状则改参数不会重跑。

pub mod assets;
pub mod gateway;
pub mod net;

use crate::core::plan::{Phase, Step, StepKind};
use crate::core::spec::ResolvedZone;
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
        for b in self.pve.kernel_bridges(cancel).await? {
            snap.bridges.insert(b);
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
            StepKind::Template {
                template_storage,
                template,
            } => snap
                .templates
                .get(&(template_storage.clone(), template.clone()))
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
                StepKind::Template { template_storage, template } => {
                    Some((template_storage.clone(), template.clone()))
                }
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

        // 失败**或被跳过**的 key 集合，必须传递闭包：模板失败 → ct 跳过 →
        // ct 不算 failed，如果跳过不进集合，net 还会去 exec 一个不存在的容器。
        let mut failed_keys: std::collections::HashSet<String> = Default::default();

        for phase in [
            Phase::Bridge,
            Phase::Template,
            Phase::Container,
            Phase::Net,
            Phase::Bootstrap,
            Phase::Gateway,
            Phase::Mirror,
            Phase::Snapshot,
        ] {
            let steps: Vec<Step> = plan.by_phase(phase).cloned().collect();
            if steps.is_empty() {
                continue;
            }
            // 依赖里有失败步骤的，直接跳过（记 skipped，不写 DB——下次 run 上游
            // 成功了照常执行）
            let (blocked, runnable): (Vec<Step>, Vec<Step>) = steps.into_iter().partition(|s| {
                s.deps.iter().any(|d| failed_keys.contains(d))
            });
            for s in blocked {
                summary.skipped += 1;
                failed_keys.insert(s.key.clone());
                let _ = tx
                    .send(Event::UpstreamSkipped {
                        key: s.key.clone(),
                        reason: "上游步骤失败，本步骤跳过".into(),
                    })
                    .await;
            }
            if runnable.is_empty() {
                continue;
            }
            let steps = runnable;
            // 建容器最重：并发压低；建桥/网络很轻：可以放宽。
            let limit = match phase {
                Phase::Bridge | Phase::Template | Phase::Mirror | Phase::Gateway => 1,
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
                        failed_keys.insert(key.clone());
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
        // run 明细记不上不影响正确性（下次 run 会有新行），但显式标记意图
        let _ = db.record_run_step(run_id, &step.key, "skipped", None);
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
            // 状态写不进去就不算完成——谎报"完成"比失败更糟：下次 apply
            // 会因为记录存在而跳过，而那个"完成"从来没被持久化过。
            if let Err(e) = db.record_step(&step.key, &step.input_hash, "done", None) {
                return (
                    step.key,
                    StepState::Failed,
                    Some(format!("步骤已执行但状态记录失败: {e}")),
                );
            }
            if let Err(e) = db.record_run_step(run_id, &step.key, "done", None) {
                return (
                    step.key,
                    StepState::Failed,
                    Some(format!("步骤已执行但 run 记录失败: {e}")),
                );
            }
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
                    StepKind::Template {
                        template_storage,
                        template,
                    } => {
                        g.templates.insert(
                            (template_storage.clone(), template.clone()),
                            Some(template.clone()),
                        );
                    }
                    _ => {}
                }
            }
            (step.key, StepState::Applied, None)
        }
        Err(e) => {
            let msg = e.to_string();
            // 已经在处理失败，这里的记录失败没有更好的去处，显式丢弃
            let _ = db.record_step(&step.key, &step.input_hash, "failed", Some(&msg));
            let _ = db.record_run_step(run_id, &step.key, "failed", Some(&msg));
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
        StepKind::Bridge { name, mgmt_ip, .. } => {
            pve.ensure_bridge(name, mgmt_ip, cancel).await?;
        }

        StepKind::Template {
            template_storage,
            template,
        } => {
            pve.ensure_template(template_storage, template, cancel).await?;
        }

        StepKind::Container {
            vmid,
            hostname,
            cores,
            memory,
            disk,
            template,
            storage,
            template_storage,
            bridge,
            uplink_bridge,
            mirr_bridge,
            ssh_keys,
            password,
            ..
        } => {
            // 模板的完整 volid 从模板存储里取（probe 阶段已经查过）。
            // 真机教训：这里早期用的是 rootfs 存储，模板不在上面就报"找不到"。
            let vols = pve.templates_on(template_storage, cancel).await?;
            let vol = vols
                .iter()
                .find(|v| v.contains(template.as_str()))
                .ok_or_else(|| format!("存储 {template_storage} 上找不到模板 {template}"))?;
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
                mirr_bridge.as_deref(),
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
            dns,
            ..
        } => {
            // 地址/路由落成开机单元而不是"执行一次"：容器重启后这些运行时
            // 状态全没了，而本步骤是哈希门控的——apply 不会再跑一遍。
            let addressing = if *is_gateway {
                net::gateway_addressing(ipv4, ipv6_prefix.as_deref(), "eth1", dns)
            } else {
                net::host_script(
                    ipv4,
                    gateway.as_deref(),
                    ipv6_prefix.as_deref(),
                    *default_route,
                    dns,
                )
            };
            pve.ct_boot_script(
                &format!("net:{host}"),
                *vmid,
                "labyard-net",
                "labyard: 容器地址与路由",
                &addressing,
                Duration::from_secs(120),
                cancel,
            )
            .await?;

            if *is_gateway {
                // 基础策略与地址同时下发：网关一建立就是"默认拒绝入向"的状态，
                // 不给它留一个没有任何过滤的窗口。端口映射/VPN 由后续的
                // gateway 步骤用全量策略覆盖（同一套生成器，不会漂移）。
                //
                // nft 的规则表同样不随容器重启存活，所以也落成开机单元——
                // 否则重启后网关会静默 fail-open。
                let cidr = zone_cidr_from(ipv4);
                let no_vpn = crate::core::spec::ResolvedVpn::default();
                let base_policy = gateway::Policy {
                    lan: "eth0",
                    uplink: "eth1",
                    zone_cidr: &cidr,
                    nat: *nat,
                    egress: true,
                    v6_prefix: ipv6_prefix.as_deref(),
                    pubs: &[],
                    vpn: &no_vpn,
                };
                pve.ct_boot_script(
                    &format!("net-nft:{host}"),
                    *vmid,
                    "labyard-nft",
                    "labyard: 网关基础策略（入向默认拒绝）",
                    &gateway::nft_script(&base_policy),
                    Duration::from_secs(180),
                    cancel,
                )
                .await?;
            }
        }

        StepKind::Bootstrap {
            zone: bootstrap_zone,
            host: bootstrap_host,
            vmid,
            provisioner,
            args,
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

            // ── Web 终端：ttyd 静态二进制注入 + systemd 单元 ──
            // 绑定容器自己的内网地址而不是 0.0.0.0：网关的 eth1 接 WAN，
            // 监听全网卡等于把终端暴露给外部。
            {
                let (zone, host) = (bootstrap_zone, bootstrap_host);
                let bind_ip = args
                    .get("ipv4")
                    .map(|v| v.split('/').next().unwrap_or(v).to_string());
                if let Some(ip) = bind_ip {
                    match pve.ensure_terminal(*vmid, zone.as_str(), host.as_str(), &ip, cancel).await {
                        Ok(true) => {}
                        Ok(false) => {
                            let _ = tx
                                .send(Event::Line {
                                    key: step.key.clone(),
                                    stream: crate::core::runner::Stream::Stdout,
                                    line: "[终端] 宿主没有 /usr/local/bin/ttyd，跳过 Web 终端安装"
                                        .into(),
                                })
                                .await;
                        }
                        Err(e) => {
                            // 终端是增强功能，不该拖垮基础配置
                            let _ = tx
                                .send(Event::Line {
                                    key: step.key.clone(),
                                    stream: crate::core::runner::Stream::Stdout,
                                    line: format!("[终端] ttyd 安装失败（不影响主机可用）: {e}"),
                                })
                                .await;
                        }
                    }
                }
            }
        }

        StepKind::Gateway {
            vmid,
            host,
            zone_cidr,
            nat,
            egress,
            v6_prefix,
            pubs,
            vpn,
            uplink,
            ..
        } => {
            let policy = gateway::Policy {
                lan: "eth0",
                uplink,
                zone_cidr,
                nat: *nat,
                egress: *egress,
                v6_prefix: v6_prefix.as_deref(),
                pubs,
                vpn,
            };
            // 规则整份重建：改一条映射也走同一路径，不存在增量漂移。
            // 同时覆盖 net 阶段装的那份基础策略单元——同一个单元名，
            // 开机跑的就是"最后一次下发"的完整策略。
            pve.ct_boot_script(
                &format!("gw-nft:{vmid}"),
                *vmid,
                "labyard-nft",
                "labyard: 网关策略（NAT / 映射 / 出网 / VPN）",
                &gateway::nft_script(&policy),
                Duration::from_secs(180),
                cancel,
            )
            .await?;

            // VPN 的"开"和"关"都要落地。只把规格里的开关关掉、容器里的 wg0
            // 还在跑，是最糟的一种不一致：平台说没有，实际还开着。
            if vpn.enabled() {
                // VPN 需要 /dev/net/tun。补设备必须重启容器才生效——少了这一步，
                // 表现是"配置写得好好的，wg0 就是起不来"，且容器内没有任何线索。
                match pve.ensure_tun(*vmid, cancel).await {
                    Ok(true) => {
                        let _ = tx
                            .send(Event::Line {
                                key: step.key.clone(),
                                stream: crate::core::runner::Stream::Stdout,
                                line: format!(
                                    "[VPN] 已为 {host} 补上 /dev/net/tun 并重启容器（设备需要重启才可见）"
                                ),
                            })
                            .await;
                    }
                    Ok(false) => {}
                    Err(e) => return Err(format!("补 /dev/net/tun 失败: {e}").into()),
                }
            }
            // 无论如何都跑一遍 VPN 同步脚本：开启时它建隧道，关闭时它把
            // 单元停掉。脚本本身对"没装过"是幂等的（全部 || true）。
            pve.push_and_run(
                &format!("gw-vpn:{vmid}"),
                *vmid,
                &gateway::vpn_script(&policy),
                Duration::from_secs(1500),
                cancel,
            )
            .await?;
        }

        StepKind::Mirror {
            zone,
            bridge,
            source_vmid,
            target_vmid,
            ..
        } => {
            // labmirror 的脚本与单元模板是二进制内嵌资产——版本永远与
            // labyart 二进制同步，不会出现"装了旧脚本配新配置"。
            let script_body = assets::Assets::text("base/labmirror.sh")
                .ok_or("内置 labmirror.sh 缺失（编译产物不完整）")?;
            let unit_body = assets::Assets::text("base/labmirror@.service")
                .ok_or("内置 labmirror@.service 缺失（编译产物不完整）")?;
            // 实例名与单元名必须分开：单元是 labmirror@<instance>，
            // conf 文件名是 <instance>.conf（脚本按 %i 找 conf，不带前缀）。
            // 早期两处都用带前缀的名字，单元永远找不到 conf。
            let instance = format!("{zone}-{source_vmid}-{target_vmid}");
            let unit = format!("labmirror@{instance}");
            let conf_dir = "/etc/labyard/mirror";
            // 落点不写死 veth 名：analysis 重建后 veth1002i1→veth1005i1，
            // 旧名字挂不上规则且不报错。让 labmirror 按 mirr 桥现探。
            // SRC_BRIDGE 必须是桥名（lablab）而不是 zone 名（lab）——两者
            // 只在没配 bridge 前缀时碰巧相同，用错了单元会静默 inactive
            let conf = format!(
                "SRC_BRIDGE={bridge}\nMIRR_BRIDGE=mirr\nSOURCE_VMID={source_vmid}\nTARGET_VMID={target_vmid}\n"
            );
            let b64_script = crate::pve::base64_encode(script_body.as_bytes());
            let b64_unit = crate::pve::base64_encode(unit_body.as_bytes());
            let cmd = format!(
                "set -e\n\
                 mkdir -p {conf_dir}\n\
                 echo {b64_script} | base64 -d > /usr/local/bin/labmirror\n\
                 chmod +x /usr/local/bin/labmirror\n\
                 echo {b64_unit} | base64 -d > /etc/systemd/system/labmirror@.service\n\
                 rm -f {conf_dir}/labmirror@*.conf\n\
                 printf '%s' '{conf}' > {conf_dir}/{instance}.conf\n\
                 systemctl daemon-reload\n\
                 systemctl enable {unit}.service 2>/dev/null || true\n\
                 systemctl restart {unit}.service 2>&1 || {{ journalctl -u {unit}.service -n 20 --no-pager; exit 1; }}"
            );
            let out = runner
                .run(
                    &format!("mirror:{unit}"),
                    &CommandSpec::new("sh", ["-c", cmd.as_str()])
                        .with_timeout(Duration::from_secs(60)),
                    Some(tx.clone()),
                    cancel,
                    false,
                )
                .await?;
            // 判定靠退出码而不是 stdout：这条调用把行转发到事件流（Web/CLI
            // 都能看到），返回体里的 stdout 是空的——早期用 stdout 判 active，
            // 结果永远拿到空字符串，参数对也被判失败。
            if out.code != Some(0) {
                return Err(format!(
                    "labmirror 实例启动失败（退出码 {:?}，journal 已随事件流输出）",
                    out.code
                )
                .into());
            }
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

// ═════════════════════════ 镜像实例 ═════════════════════════

impl Executor {
    /// 停掉并移除一条镜像实例（规格里删掉连线时调用）。
    ///
    /// 只删规格不停实例，是"平台不知道它在跑"的最坏形态：流量还在灌，
    /// 平台却认为没有这条连线，排查时两边对不上。
    pub async fn remove_mirror_instance(
        &self,
        zone: &str,
        src_vmid: u32,
        dst_vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let instance = format!("{zone}-{src_vmid}-{dst_vmid}");
        self.stop_units(&[instance], cancel).await
    }

    /// 停掉指定前缀的全部镜像实例，返回停掉的实例名。
    /// `prefix` 形如 `main-`（整个 zone）或 `main-1001-`（某台主机当源）。
    /// 停掉匹配给定 glob 的镜像实例（宿主侧 `labmirror@` 单元）。
    ///
    /// 为什么要接受多个 pattern：单元名是 `{zone}-{源vmid}-{目标vmid}`。
    /// 废弃**源机**要按前缀匹配（`lab-102-*`），废弃**分析机**要按后缀匹配
    /// （`lab-*-100`）。只做前缀时，retire 一台 analysis 会"成功"但单元还在跑，
    /// 症状是平台说废弃了、流量照旧灌进去。
    pub async fn stop_mirror_instances(
        &self,
        globs: &[String],
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, String> {
        let files: String = globs
            .iter()
            .map(|g| format!("/etc/labyard/mirror/{g} "))
            .collect();
        let cmd = format!(
            "for f in {files}; do \
               [ -e \"$f\" ] || continue; \
               i=$(basename \"$f\" .conf); \
               systemctl disable --now \"labmirror@$i.service\" >/dev/null 2>&1 || true; \
               echo \"$i\"; \
             done"
        );
        let out = self
            .runner
            .run(
                &format!("mirror-stop:{}", globs.join(",")),
                &CommandSpec::new("sh", ["-c", cmd.as_str()])
                    .with_timeout(Duration::from_secs(60)),
                None,
                cancel,
                true,
            )
            .await
            .map_err(|e| e.to_string())?;
        // 两个 pattern 可能命中同一个文件，去重后再报给用户
        let mut seen = std::collections::BTreeSet::new();
        Ok(out
            .stdout
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && seen.insert(s.clone()))
            .collect())
    }

    /// 停单元 + 删配置。实例名拼法与执行镜像步骤时严格一致。
    async fn stop_units(&self, instances: &[String], cancel: &CancellationToken) -> Result<(), String> {
        let mut cmd = String::from("set -e\n");
        for i in instances {
            cmd.push_str(&format!(
                "systemctl disable --now labmirror@{i}.service >/dev/null 2>&1 || true\n\
                 rm -f /etc/labyard/mirror/{i}.conf\n\
                 systemctl reset-failed labmirror@{i}.service >/dev/null 2>&1 || true\n"
            ));
        }
        cmd.push_str("echo removed\n");
        self.runner
            .run(
                "mirror-remove",
                &CommandSpec::new("sh", ["-c", cmd.as_str()])
                    .with_timeout(Duration::from_secs(60)),
                None,
                cancel,
                true,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ═════════════════════════ destroy ═════════════════════════

/// 一个 zone 的销毁结果。`kept_bridge` 非 None 表示桥被保留（还有别的成员）。
#[derive(Debug, Default)]
pub struct DestroyReport {
    pub destroyed_cts: Vec<(String, u32)>,
    pub absent_cts: Vec<String>,
    pub bridge_deleted: bool,
    pub kept_bridge: Option<String>,
}

impl Executor {
    /// 删除一个 zone 的全部资源（CT + 桥 + 分配记录 + 步骤状态）。
    ///
    /// 设计原则（docs/design.md §4.6）：**硬检查不满足就整体中止**，不做
    /// "尽力而为"——删到一半发现还有别人引用，剩下的不删，状态保持一致。
    /// 模板不删：它是共享资产（多个 zone 可能用同一个），留着无害。
    pub async fn destroy_zone(
        &self,
        zone: &ResolvedZone,
        all_zones: &[ResolvedZone],
        cancel: &CancellationToken,
    ) -> Result<DestroyReport, String> {
        // ── 硬检查 1：被其他 zone 当 uplink 引用 → 拒绝 ──
        let dependents: Vec<&str> = all_zones
            .iter()
            .filter(|z| z.name != zone.name && z.uplink_bridge.as_deref() == Some(zone.bridge.as_str()))
            .map(|z| z.name.as_str())
            .collect();
        if !dependents.is_empty() {
            return Err(format!(
                "zone `{}` 的桥 {} 仍被 {} 上联引用——先改它们的 uplink 再销毁",
                zone.name,
                zone.bridge,
                dependents.join(", ")
            ));
        }

        let mut report = DestroyReport::default();

        // ── 容器：存在才删（stop → destroy --purge），不存在记 absent ──
        let vms = self.pve.cluster_vms(cancel).await.map_err(|e| e.to_string())?;
        for h in &zone.hosts {
            // spec 固定 vmid 或 DB 分配过，二者都没有 = 从没建过
            let vmid = h.vmid.or_else(|| self.db.vmid_of(&zone.name, &h.name));
            let Some(vmid) = vmid else {
                report.absent_cts.push(h.name.clone());
                continue;
            };
            let hostname = format!("{}-{}", zone.name, h.name);
            let exists = vms
                .iter()
                .any(|r| r.is_lxc() && r.vmid_parsed() == Some(vmid));
            if !exists {
                report.absent_cts.push(hostname);
            } else {
                match self.pve.ct_status(vmid, cancel).await {
                    Ok(Some(st)) if st == "running" => {
                        self.pve
                            .ct_action(vmid, "stop", cancel)
                            .await
                            .map_err(|e| format!("停 {hostname} 失败: {e}"))?;
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("查 {hostname} 状态失败: {e}")),
                }
                self.pve
                    .ct_action(vmid, "destroy", cancel)
                    .await
                    .map_err(|e| format!("删 {hostname} 失败: {e}"))?;
                report.destroyed_cts.push((hostname, vmid));
            }
            // 分配与步骤状态一律释放——即使 PVE 上本来就不存在（状态不一致时
            // 正好借 destroy 收拾干净，下次 apply 从零重建）
            self.db.release_vmid(&zone.name, &h.name);
            for prefix in ["ct", "net", "bootstrap", "snapshot"] {
                self.db
                    .forget_step(&format!("{prefix}:{}:{}", zone.name, h.name));
            }
        }

        // ── mirr 桥：analysis 的 eth2 随容器消失，桥空了就一并删 ──
        if let Ok(m) = self.pve.bridge_members("mirr", cancel).await {
            if m.is_empty() {
                let _ = self.pve.destroy_bridge("mirr", cancel).await;
            }
        }

        // ── 硬检查 2：桥上还有别的成员口 → 不删，打印剩余成员 ──
        let members = self
            .pve
            .bridge_members(&zone.bridge, cancel)
            .await
            .map_err(|e| e.to_string())?;
        if members.is_empty() {
            self.pve
                .destroy_bridge(&zone.bridge, cancel)
                .await
                .map_err(|e| e.to_string())?;
            self.db.forget_step(&format!("bridge:{}", zone.name));
            report.bridge_deleted = true;
        } else {
            report.kept_bridge = Some(format!(
                "{} 仍被 {} 占用（可能是容器重建或残留 veth），桥已保留",
                zone.bridge,
                members.join(", ")
            ));
        }

        Ok(report)
    }
}
