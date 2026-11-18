//! labyard —— 声明式攻防实验网络编排（Proxmox VE）。
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
    let pve = Arc::new(Pve::new(
        runner.clone(),
        cfg.pve.node.clone(),
        cfg.pve.wan_bridge.clone(),
    ));

    let secret = Arc::new(api::auth::load_or_create_secret(&cfg.server.secret_file)?);

    let hash_path = cfg.paths.db.with_file_name("password.hash");
    if let Some(pw) = api::auth::bootstrap_password(&hash_path)? {
        println!("────────────────────────────────────────");
        println!("  初始管理员密码: {pw}");
        println!("  只显示这一次，请立即保存");
        println!("────────────────────────────────────────");
    }
    let password_hash = Arc::new(std::fs::read_to_string(&hash_path)?.trim().to_string());

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
        cfg: Arc::new(cfg),
        runs: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        concurrency,
        // 解析是最吃内存的路径，闸门固定 4（实测 4 并发下峰值回到 ~12 MB）
        preview_gate: Arc::new(tokio::sync::Semaphore::new(4)),
        // scrypt 单次 32 MB 工作内存，必须串行
        hash_gate: Arc::new(tokio::sync::Semaphore::new(1)),
    });

    let app = api::router(state);

    if tls {
        use axum_server::tls_rustls::RustlsConfig;
        let tls_cfg =
            RustlsConfig::from_pem_file(cert.unwrap(), key.unwrap()).await?;
        tracing::info!("HTTPS 监听 https://{addr}");
        axum_server::bind_rustls(addr.parse()?, tls_cfg)
            .serve(app.into_make_service())
            .await?;
    } else {
        tracing::info!("HTTP 监听 http://{addr}（未配 TLS，建议用 Caddy 反代）");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;
    }
    Ok(())
}

async fn doctor(config_path: PathBuf, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let (cfg, warn) = Config::load(&config_path);
    let runner = Arc::new(RealRunner::new());
    let pve = Pve::new(runner, cfg.pve.node.clone(), cfg.pve.wan_bridge.clone());
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
                format!("{}.{}（需要 ≥ 9.0）", v.version, v.release),
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
    let zones = spec.resolve(&cfg.pve.wan_bridge)?;

    let db = Arc::new(Db::open(&cfg.paths.db)?);
    let base = cfg.pve.vmid_base;
    let db2 = db.clone();
    let plan = Plan::build(
        &zones,
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
    let pve = Arc::new(Pve::new(
        runner.clone(),
        cfg.pve.node.clone(),
        cfg.pve.wan_bridge.clone(),
    ));
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
