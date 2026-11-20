//! labyard —— 声明式网络编排（Proxmox VE）。
//!
//! 三条命令：`serve`（Web 界面）、`doctor`（环境自检）、`apply`（无界面执行）。

mod api;
mod config;
mod core;
mod db;
mod pve;
mod resources;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::core::plan::Plan;
use crate::core::runner::RealRunner;
use crate::core::spec::Spec;
use crate::db::Db;
use crate::pve::Pve;
use crate::resources::{assets, Executor};

#[derive(Parser)]
#[command(name = "labyard", version, about = "声明式实验网络编排（Proxmox VE 9.x）")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动 Web 界面
    Serve {
        #[arg(long, default_value = "/etc/labyard/config.toml")]
        config: PathBuf,
        /// 覆盖监听端口
        #[arg(long)]
        port: Option<u16>,
        /// 并发上限（建容器的并发数）
        #[arg(long, default_value_t = 3)]
        concurrency: usize,
    },
    /// 环境自检：PVE 版本、命令可用性、存储与桥
    Doctor {
        #[arg(long, default_value = "/etc/labyard/config.toml")]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// 无界面执行一份规格（幂等）
    Apply {
        #[arg(short, long)]
        file: PathBuf,
        /// 只打印计划，不执行
        #[arg(long)]
        dry_run: bool,
        /// 跳过确认
        #[arg(long)]
        yes: bool,
        #[arg(long, default_value = "/etc/labyard/config.toml")]
        config: PathBuf,
        #[arg(long)]
        snapshot: bool,
        #[arg(long, default_value_t = 3)]
        concurrency: usize,
    },
    /// 删除一个 zone 的全部资源（容器、网桥、分配记录）。不可逆。
    Destroy {
        /// 规格文件（用于解析 zone 的主机与桥名）
        #[arg(long, short = 'f')]
        file: PathBuf,
        #[arg(long, default_value = "/etc/labyard/config.toml")]
        config: PathBuf,
        /// 要销毁的 zone 名——必须与规格里的名字一致，这是第二道确认
        #[arg(long)]
        zone: String,
        /// 确认执行（缺了直接拒绝）
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("无法创建 tokio runtime");
    if let Err(e) = rt.block_on(run(cli)) {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.cmd {
        Cmd::Serve {
            config,
            port,
            concurrency,
        } => serve(config, port, concurrency).await,
        Cmd::Doctor { config, json } => doctor(config, json).await,
        Cmd::Apply {
            file,
            dry_run,
            yes: _,
            config,
            snapshot,
            concurrency,
        } => apply(file, dry_run, config, snapshot, concurrency).await,
        Cmd::Destroy { file, config, zone, yes } => {
            destroy_cmd(file, config, zone, yes).await
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

async fn serve(
    config_path: PathBuf,
    port: Option<u16>,
    concurrency: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let (mut cfg, warn) = Config::load(&config_path);
    if let Some(w) = warn {
        tracing::warn!("{w}");
    }
    if let Some(p) = port {
        cfg.server.port = p;
    }
    serve_with(cfg, concurrency).await
}

async fn serve_with(cfg: Config, concurrency: usize) -> Result<(), Box<dyn std::error::Error>> {
    let db = Arc::new(Db::open(&cfg.paths.db)?);
    db.mark_interrupted();

    let runner = Arc::new(RealRunner::new());
    let pve = Arc::new(Pve::new(runner.clone(), cfg.pve.node.clone()));
    // 读缓存的后台补新线程 + 开机预热：让"点开就有"成为常态，
    // 而不是每次过了缓存期都要看着转圈。
    pve.start_refresher();
    pve.warm();

    let secret = Arc::new(api::auth::load_or_create_secret(&cfg.server.secret_file)?);

    let hash_path = cfg.paths.db.with_file_name("password.hash");
    if let Some(pw) = api::auth::bootstrap_password(&hash_path)? {
        println!("────────────────────────────────────────");
        println!("  初始管理员密码: {pw}");
        println!("  只显示这一次，请立即保存");
        println!("────────────────────────────────────────");
    }
    let password_hash = Arc::new(std::sync::RwLock::new(
        std::fs::read_to_string(&hash_path)?.trim().to_string(),
    ));

    for d in [&cfg.paths.locks, &cfg.paths.tmp, &cfg.paths.specs] {
        let _ = std::fs::create_dir_all(d);
    }

    let addr = format!("{}:{}", cfg.server.bind, cfg.server.port);
    let tls = cfg.tls_enabled();
    let cert = cfg.server.tls_cert.clone();
    let key = cfg.server.tls_key.clone();

    let state = Arc::new(api::AppState {
        db,
        pve,
        runner,
        secret,
        password_hash,
        password_hash_path: hash_path,
        cfg: Arc::new(cfg),
        runs: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        concurrency,
        // 解析是最吃内存的路径，闸门固定 4（实测 4 并发下峰值回到 ~12 MB）
        preview_gate: Arc::new(tokio::sync::Semaphore::new(4)),
        // scrypt 单次 32 MB 工作内存，必须串行
        hash_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        // run 串行闸：一个 run 批内并发 3 个 pct exec（各 ~176 MB），两个
        // run 并发就是 1 GB+，真机被 oom-kill 过。见 spawn_plan 的注释。
        run_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        reconcile: Arc::new(tokio::sync::Mutex::new(Default::default())),
    });

    // 后台镜像巡检：每 5 分钟对一次账，规格说该有的镜像实例漏了就补装，
    // 宿主上多出来的孤儿单元就停掉。
    api::reconcile::spawn(state.clone());

    let app = api::router(state.clone());

    if tls {
        use axum_server::tls_rustls::RustlsConfig;
        let tls_cfg =
            RustlsConfig::from_pem_file(cert.unwrap(), key.unwrap()).await?;
        tracing::info!("HTTPS 监听 https://{addr}");
        // 优雅停机：先取消在途 run（落成 cancelled），再让 axum-server 收连接。
        let handle = axum_server::Handle::new();
        let h = handle.clone();
        let st = state.clone();
        tokio::spawn(async move {
            shutdown_signal(st).await;
            h.graceful_shutdown(Some(std::time::Duration::from_secs(8)));
        });
        axum_server::bind_rustls(addr.parse()?, tls_cfg)
            .handle(handle)
            .serve(app.into_make_service())
            .await?;
    } else {
        tracing::info!("HTTP 监听 http://{addr}（未配 TLS，建议用 Caddy 反代）");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        let st = state.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(st))
            .await?;
    }
    Ok(())
}

/// 停机：**先把在途 run 全部取消**，让执行器把它们落成 `cancelled`，
/// 再交给 axum 收连接。
///
/// 为什么必须这样：`systemctl stop/restart`（部署、升级都会做）只给进程
/// 发 SIGTERM，正在跑的 `pct exec` 会以非零退出码结束——执行器把这当成
/// "命令失败"，run 就被记成 failed。实测部署 4 次，运行历史里攒了 4 条
/// `gateway:lab 失败`，用户看到只会以为面板出了问题。
async fn shutdown_signal(state: Arc<api::AppState>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }

    let runs = state.runs.lock().await;
    let live = runs.len();
    for h in runs.values() {
        h.cancel.cancel();
    }
    drop(runs);
    if live > 0 {
        tracing::info!("停机：已取消 {live} 个在途 run，等它们落状态");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

async fn doctor(config_path: PathBuf, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let (cfg, warn) = Config::load(&config_path);
    let runner = Arc::new(RealRunner::new());
    let pve = Pve::new(runner, cfg.pve.node.clone());
    let cancel = CancellationToken::new();

    let mut rows: Vec<(String, bool, String)> = Vec::new();

    match pve.version(&cancel).await {
        Ok(v) => {
            let major: u32 = v
                .version
                .split('.')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            rows.push((
                "PVE 版本".into(),
                major >= 9,
                format!("{}（需要 ≥ 9.0）", v.version),
            ));
        }
        Err(e) => rows.push(("PVE 版本".into(), false, e.to_string())),
    }
    match pve.bridges(&cancel).await {
        Ok(bs) => {
            let names: Vec<String> = bs.iter().map(|b| b.iface.clone()).collect();
            rows.push((
                "网桥".into(),
                names.contains(&cfg.pve.wan_bridge),
                names.join(", "),
            ));
        }
        Err(e) => rows.push(("网桥".into(), false, e.to_string())),
    }
    match pve.storages(&cancel).await {
        Ok(ss) => {
            let list: Vec<String> = ss
                .iter()
                .filter(|s| s.supports("rootdir"))
                .map(|s| format!("{} ({})", s.storage, s.kind))
                .collect();
            rows.push(("存储(rootdir)".into(), !list.is_empty(), list.join(", ")));
            // 模板（vztmpl）必须有可落地的存储。真机教训：rootfs 用 lvmthin 时
            // `pveam download local-lvm` 直接 255——模板和 rootfs 是两种内容类型。
            let tpl: Vec<String> = ss
                .iter()
                .filter(|s| s.supports("vztmpl"))
                .map(|s| format!("{} ({})", s.storage, s.kind))
                .collect();
            rows.push((
                "模板存储(vztmpl)".into(),
                !tpl.is_empty(),
                if tpl.is_empty() {
                    "没有支持 vztmpl 的存储；spec 可用 template_storage 指定".into()
                } else {
                    tpl.join(", ")
                },
            ));
        }
        Err(e) => rows.push(("存储".into(), false, e.to_string())),
    }
    for cmd in ["pct", "pvesh", "pveam", "ip", "nft"] {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {cmd}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        rows.push((format!("命令 {cmd}"), ok, if ok { "ok" } else { "缺失" }.into()));
    }
    if let Some(w) = warn {
        rows.push(("配置".into(), false, w));
    }

    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(n, ok, d)| serde_json::json!({"name": n, "ok": ok, "detail": d}))
            .collect();
        println!("{}", serde_json::json!({"items": items}));
    } else {
        println!("{:<16} {:<6} {}", "检查项", "结果", "详情");
        println!("{}", "-".repeat(64));
        for (n, ok, d) in &rows {
            println!("{:<16} {:<6} {}", n, if *ok { "OK" } else { "FAIL" }, d);
        }
    }

    if rows.iter().any(|(_, ok, _)| !ok) {
        std::process::exit(1);
    }
    Ok(())
}

async fn apply(
    file: PathBuf,
    dry_run: bool,
    config_path: PathBuf,
    snapshot: bool,
    concurrency: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let (cfg, _) = Config::load(&config_path);
    let text = std::fs::read_to_string(&file)?;
    let spec = Spec::parse(&text)?;
    let resolved = spec.resolve_all(&cfg.pve.wan_bridge)?;

    let db = Arc::new(Db::open(&cfg.paths.db)?);
    let base = cfg.pve.vmid_base;
    let db2 = db.clone();
    let plan = Plan::build(
        &resolved.zones,
        &resolved.mirrors,
        &|p| assets::Assets::provisioner_hash(p),
        &|z, h| db2.vmid_for(z, h, base),
        snapshot,
    );

    println!("计划共 {} 步：", plan.steps.len());
    for s in &plan.steps {
        println!("  [{}] {:<34} {}", s.phase.as_str(), s.key, &s.input_hash[..8]);
    }

    if dry_run {
        println!("\n--dry-run：未执行任何操作");
        return Ok(());
    }

    let run_id = format!("cli-{}", chrono::Utc::now().format("%Y%m%d%H%M%S"));
    db.create_run(&run_id, &file.display().to_string());

    let runner = Arc::new(RealRunner::new());
    let pve = Arc::new(Pve::new(runner.clone(), cfg.pve.node.clone()));

    // 与 Web 路径同一道门槛：不做 PVE 8 兼容层，就在动手前拒绝。
    // CLI 更容易被脚本调用，静默跑在错版本上比报错糟得多。
    pve.require_pve9(&CancellationToken::new()).await?;

    let ex = Executor::new(pve, db.clone(), runner);

    let (tx, mut rx) = tokio::sync::mpsc::channel(500);
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let crate::core::runner::Event::Line { line, stream, .. } = ev {
                match stream {
                    crate::core::runner::Stream::Stdout => println!("{line}"),
                    crate::core::runner::Stream::Stderr => eprintln!("{line}"),
                }
            }
        }
    });

    let cancel = CancellationToken::new();
    let summary = ex.execute(&plan, &run_id, tx, cancel, concurrency).await;
    let _ = printer.await;

    db.finish_run(&run_id, if summary.ok() { "ok" } else { "failed" });

    println!(
        "\n结果：共 {} 步，执行 {}，跳过 {}，失败 {}",
        summary.total, summary.applied, summary.skipped, summary.failed
    );
    for (k, e) in &summary.errors {
        eprintln!("  ✗ {k}: {e}");
    }
    if !summary.ok() {
        std::process::exit(2);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn destroy_cmd(
    file: PathBuf,
    config_path: PathBuf,
    zone: String,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    // 硬检查：--yes 且 zone 名与规格一致（docs/design.md §4.6 的两次确认）
    if !yes {
        return Err("destroy 不可逆：必须带 --yes，且 --zone 指明要销毁的 zone 名".into());
    }
    let (cfg, _) = Config::load(&config_path);
    let text = std::fs::read_to_string(&file)?;
    let spec = Spec::parse(&text)?;
    let zones = spec.resolve(&cfg.pve.wan_bridge)?;
    let Some(z) = zones.iter().find(|z| z.name == zone) else {
        return Err(format!(
            "规格里没有 zone `{zone}`（现有: {}）",
            zones
                .iter()
                .map(|z| z.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    };

    let db = Arc::new(Db::open(&cfg.paths.db)?);
    let runner = Arc::new(RealRunner::new());
    let pve = Arc::new(Pve::new(runner.clone(), cfg.pve.node.clone()));
    let ex = Executor::new(pve, db.clone(), runner);

    let report = ex
        .destroy_zone(z, &zones, &CancellationToken::new())
        .await?;

    println!("zone `{}` 销毁结果：", z.name);
    for (name, vmid) in &report.destroyed_cts {
        println!("  ✓ 已删除 {name} (CT {vmid})");
    }
    for name in &report.absent_cts {
        println!("  · {name}: PVE 上不存在，只清理了分配记录");
    }
    if report.bridge_deleted {
        println!("  ✓ 已删除网桥 {}", z.bridge);
    }
    if let Some(reason) = &report.kept_bridge {
        println!("  ! 桥保留：{reason}");
    }
    println!("  vmid 分配与步骤状态已释放，下次 apply 会从零重建");
    Ok(())
}
