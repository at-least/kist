//! `kist` 命令列入口：解析參數、取得密碼與 client id，然後把工作交給 `kist-core`。
//!
//! 所有行為都在 `kist-core`；這裡只做輸入輸出。錯誤一律用 `anyhow` 往上拋，
//! 在 `main` 印成一行後以非 0 結束。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod password;

// 診斷用（bench/memory 歸因，暫時）：--features dhat 時改用 dhat 分配器，
// main 結束寫出 dhat.json。
#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use kist_app::client_id;
use kist_app::duration::parse_duration;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use kist_backend::{Backend, RepoLocation};
use kist_core::{
    BackupOptions, CheckOptions, ForgetOptions, ForgetSummary, InitOptions, PruneOptions,
    PruneReport, Repository, RestoreOptions, RetentionPolicy, SourceSpec,
};

/// 結束碼（沿用 restic 的慣例）：0 成功；1 失敗；3 backup / restore 完成但有項目被略過或還原失敗。
const EXIT_FAILURE: i32 = 1;
const EXIT_INCOMPLETE: i32 = 3;

/// `run` 回傳「成功但不完整」時用這個錯誤型別告訴 `main` 要用結束碼 3。
#[derive(Debug)]
struct Incomplete(String);

impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Incomplete {}

/// kist: deduplicating, encrypted backups to object storage.
#[derive(Debug, Parser)]
#[command(name = "kist", version, about, long_about = None)]
struct Cli {
    /// Output results as JSON on stdout. Errors still go to stderr and exit
    /// codes are unchanged (0 ok, 1 failure, 3 incomplete).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct PruneArgs {
    /// Minimum time between marking an object and deleting it. Must be longer than your longest
    /// backup, and match `backup --gc-grace`.
    #[arg(long, value_name = "DURATION", default_value = "72h", value_parser = parse_duration)]
    grace: std::time::Duration,
    /// Clients without a snapshot for this long no longer hold back deletion.
    #[arg(long, value_name = "DURATION", default_value = "30d", value_parser = parse_duration)]
    inactive_after: std::time::Duration,
    /// Tolerated clock difference between clients and the prune host.
    #[arg(long, value_name = "DURATION", default_value = "1h", value_parser = parse_duration)]
    clock_skew: std::time::Duration,
    /// Repack packs whose live data is below this percentage (0 disables repacking).
    #[arg(long, value_name = "PERCENT", default_value_t = 50, value_parser = clap::value_parser!(u8).range(0..=100))]
    repack_below: u8,
}

impl PruneArgs {
    fn options(&self, dry_run: bool) -> PruneOptions {
        PruneOptions {
            grace: self.grace,
            inactive_after: self.inactive_after,
            clock_skew: self.clock_skew,
            repack_below_percent: self.repack_below,
            dry_run,
            now: None,
        }
    }
}

/// 每個需要 repo 的命令共用的參數。
#[derive(Debug, Args)]
struct RepoArgs {
    /// Repository location: a local directory, `s3://bucket[/prefix]`,
    /// `sftp://[user@]host[:port]/path`, or `rclone://[remote/]path` (kist spawns
    /// `rclone serve sftp --stdio` itself; the rclone bridge has weaker conditional-
    /// write atomicity, see docs/decisions/014-rclone-bridge.md).
    /// For S3 set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_DEFAULT_REGION,
    /// plus AWS_ENDPOINT (and AWS_ALLOW_HTTP=true) for MinIO and other S3-compatible services.
    /// For SFTP set KIST_SFTP_PASSWORD or KIST_SFTP_KEY (see `kist_backend::sftp` docs).
    /// For rclone set KIST_RCLONE_BIN if rclone is not on PATH.
    #[arg(long, short = 'r', env = "KIST_REPO", global = true)]
    repo: Option<String>,

    /// Read the repository password from this file (first line).
    /// Otherwise the KIST_PASSWORD environment variable is used, or you are prompted.
    #[arg(long, env = "KIST_PASSWORD_FILE", global = true)]
    password_file: Option<PathBuf>,

    /// Directory for the local index cache (default: the user cache directory, e.g. ~/.cache/kist).
    #[arg(long, env = "KIST_CACHE_DIR", global = true)]
    cache_dir: Option<PathBuf>,

    /// Do not use a local index cache; read every index object from the repository.
    #[arg(long, global = true)]
    no_cache: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new, empty repository.
    Init {
        #[command(flatten)]
        repo: RepoArgs,
        /// FastCDC chunk sizes in bytes (min/avg/max). All clients of one
        /// repository must agree; the values are bound into the master key
        /// AAD, so a tampered config fails to open instead of silently
        /// breaking deduplication. Defaults: 524288 / 2097152 / 8388608.
        #[arg(long, value_name = "BYTES", default_value_t = 512 * 1024)]
        chunker_min: u32,
        #[arg(long, value_name = "BYTES", default_value_t = 2 * 1024 * 1024)]
        chunker_avg: u32,
        #[arg(long, value_name = "BYTES", default_value_t = 8 * 1024 * 1024)]
        chunker_max: u32,
    },
    /// Back up one or more paths into a new snapshot. A single `sftp://` or
    /// `s3://` URL backs up that remote source directly (the client reads,
    /// chunks and encrypts; keys never leave this machine).
    Backup {
        #[command(flatten)]
        repo: RepoArgs,
        /// Files or directories to back up, or one remote source URL.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// File holding this machine's client id (created on first use).
        #[arg(long, env = "KIST_CLIENT_ID_FILE")]
        client_id_file: Option<PathBuf>,
        /// Packs marked for deletion longer ago than this are treated as already gone.
        /// Must match the `--grace` used by `prune`.
        #[arg(long, value_name = "DURATION", default_value = "72h", value_parser = parse_duration)]
        gc_grace: std::time::Duration,
        /// Reed-Solomon parity shards per pack, out of 16 data shards
        /// (0 = none; 2 = 12.5% overhead, repairs up to 2 damaged sixteenths).
        #[arg(long, value_name = "0..=8", default_value = "0")]
        parity: u8,
    },
    /// List snapshots.
    Snapshots {
        #[command(flatten)]
        repo: RepoArgs,
    },
    /// Browse snapshots as a read-only filesystem (Linux/macOS).
    #[cfg(unix)]
    Mount {
        #[command(flatten)]
        repo: RepoArgs,
        /// Existing empty directory to mount at.
        mountpoint: PathBuf,
    },
    /// Restore a snapshot into a target directory.
    Restore {
        #[command(flatten)]
        repo: RepoArgs,
        /// Snapshot to restore: `latest`, a full id, or a unique timestamp prefix.
        snapshot: String,
        /// Directory to restore into (the original absolute paths are recreated beneath it).
        /// Should be empty: existing files are overwritten and existing symlinks are followed.
        target: PathBuf,
    },
    /// Verify the repository's integrity.
    Check {
        #[command(flatten)]
        repo: RepoArgs,
        /// Also download every pack and verify every chunk (slow).
        #[arg(long)]
        read_data: bool,
        /// Rewrite damaged packs from their parity objects (implies --read-data).
        /// Needs Put access; objects under S3 Object Lock cannot be repaired.
        #[arg(long)]
        repair: bool,
    },
    /// Remove snapshots, by id or by retention policy. Data is reclaimed later by `prune`.
    Forget {
        #[command(flatten)]
        repo: RepoArgs,
        /// Snapshots to remove (`latest`, a full id, or a unique timestamp prefix).
        snapshots: Vec<String>,
        /// Keep the newest N snapshots of each client/path group.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        keep_last: Option<u32>,
        /// Keep the newest snapshot of each of the last N hours.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        keep_hourly: Option<u32>,
        /// Keep the newest snapshot of each of the last N days.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        keep_daily: Option<u32>,
        /// Keep the newest snapshot of each of the last N ISO weeks.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        keep_weekly: Option<u32>,
        /// Keep the newest snapshot of each of the last N months.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        keep_monthly: Option<u32>,
        /// Keep the newest snapshot of each of the last N years.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        keep_yearly: Option<u32>,
        /// Keep every snapshot newer than this (e.g. `36h`, `14d`, `2w`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        keep_within: Option<std::time::Duration>,
        /// Show what would be removed without removing anything.
        #[arg(long)]
        dry_run: bool,
        /// Run `prune` afterwards (the prune options below apply).
        #[arg(long)]
        prune: bool,
        #[command(flatten)]
        prune_args: PruneArgs,
    },
    /// Reclaim space: mark unreferenced data, delete what was marked longer ago than the
    /// grace period, and repack mostly-unused packs. Safe to run while backups are running.
    Prune {
        #[command(flatten)]
        repo: RepoArgs,
        #[command(flatten)]
        prune: PruneArgs,
        /// Report what would happen without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebuild the index from the pack files (after index objects were lost or corrupted).
    RebuildIndex {
        #[command(flatten)]
        repo: RepoArgs,
    },
    /// Run the jobs described in a config file: on their cron schedules (daemon), or once each.
    Run {
        /// Path to the TOML config file (see README).
        #[arg(long, short = 'c', env = "KIST_CONFIG")]
        config: PathBuf,
        /// Run every configured job once (backup, forget, prune) and exit; for external cron.
        #[arg(long)]
        once: bool,
    },
    /// Run the configured jobs on their schedules and serve `/metrics` over HTTP
    /// (the long-running daemon; the web UI will live here too).
    Serve {
        /// Path to the TOML config file (see README).
        #[arg(long, short = 'c', env = "KIST_CONFIG")]
        config: PathBuf,
        /// Address for the HTTP server. Loopback by default: `/metrics` has no
        /// authentication and reveals the repo location and job schedule.
        #[arg(long, env = "KIST_HTTP", default_value = "127.0.0.1:9898")]
        http: std::net::SocketAddr,
    },
    /// Print version information.
    Version,
}

fn main() {
    #[cfg(feature = "dhat")]
    let _dhat_profiler = dhat::Profiler::builder().build();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    // blocking pool 收斂到 4 執行緒：每個 glibc arena 都會保留自己的高水位，
    // 預設上限 512 條會讓峰值記憶體隨 arena 數放大（M5 記憶體目標）。
    // 加密/切塊/壓縮都是 CPU 密集，4 條已能餵飽 async 端的上傳。
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(4)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start async runtime: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = runtime.block_on(run(cli)) {
        if e.downcast_ref::<Incomplete>().is_some() {
            eprintln!("warning: {e:#}");
            std::process::exit(EXIT_INCOMPLETE);
        }
        eprintln!("error: {e:#}");
        std::process::exit(EXIT_FAILURE);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let Cli { json, command } = cli;
    match command {
        Command::Version => {
            println!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Run { config, once } => {
            let cfg = kist_app::Config::load(&config)?;
            let mut daemon = kist_app::Daemon::new(cfg)?;
            if once {
                // --json：每件工作的結果已經是 JSON，最後印整個陣列
                let outcomes = daemon
                    .run_once(|o| {
                        if !json {
                            emit_outcome(o);
                        }
                    })
                    .await;
                if json {
                    print_json(&outcomes)?;
                }
                let failed = outcomes
                    .iter()
                    .filter(|o| o.status == kist_app::JobStatus::Failure)
                    .count();
                let incomplete = outcomes
                    .iter()
                    .filter(|o| o.status == kist_app::JobStatus::Incomplete)
                    .count();
                if failed > 0 {
                    bail!("{failed} job(s) failed");
                }
                if incomplete > 0 {
                    return Err(Incomplete(format!("{incomplete} job(s) incomplete")).into());
                }
                return Ok(());
            }
            // 沒有排程的 daemon 只會等 Web UI 的觸發；`run` 沒有 UI，常駐沒意義
            if !daemon.has_schedules() {
                bail!(
                    "no section has a schedule; use `run --once`, add schedule = \"...\", or use `serve`"
                );
            }
            print_next_runs(&daemon);
            let (tx, rx) = tokio::sync::watch::channel(false);
            spawn_ctrl_c(tx.clone());
            daemon.run(rx, emit_outcome_wrap(json)).await?;
            Ok(())
        }
        Command::Serve { config, http } => {
            let cfg = kist_app::Config::load(&config)?;
            let mut daemon = kist_app::Daemon::new(cfg)?;
            print_next_runs(&daemon);
            let listener = tokio::net::TcpListener::bind(http)
                .await
                .with_context(|| format!("cannot bind {http}"))?;
            let local = listener.local_addr()?;
            if !local.ip().is_loopback() {
                eprintln!(
                    "warning: /metrics has no authentication and reveals the repo location and \
                     job schedule; do not expose it to untrusted networks"
                );
            }
            eprintln!("listening on http://{local} (/metrics, /healthz)");
            let (tx, rx) = tokio::sync::watch::channel(false);
            spawn_ctrl_c(tx.clone());
            let ui = kist_app::server::ui_config(daemon.config(), local)?;
            let state = kist_app::server::ServeState {
                metrics: daemon.metrics(),
                daemon: daemon.handle(),
                ui: std::sync::Arc::new(ui),
            };
            let server = tokio::spawn(kist_app::server::serve_http(state, listener, rx.clone()));
            // server 半路掛掉時：記下錯誤、發 shutdown 叫醒 daemon（目前工作會做完），
            // 結束後把錯誤帶出去。不能等 daemon 自己結束才檢查——那樣 monitoring 只會
            // 看到 scrape 失敗，backup 卻還在跑。
            let server_err = std::sync::Arc::new(std::sync::Mutex::new(None::<anyhow::Error>));
            {
                let server_err = std::sync::Arc::clone(&server_err);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let err = match server.await {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(anyhow::Error::new(e).context("http server failed")),
                        Err(j) => Some(j.into()),
                    };
                    if let Some(err) = err {
                        if let Ok(mut slot) = server_err.lock() {
                            *slot = Some(err);
                        }
                        let _ = tx.send(true);
                    }
                });
            }
            let result = daemon.run(rx, emit_outcome_wrap(json)).await;
            let server_err = match server_err.lock() {
                Ok(mut slot) => slot.take(),
                Err(_) => None,
            };
            result.map_err(anyhow::Error::from)?;
            if let Some(e) = server_err {
                return Err(e);
            }
            Ok(())
        }
        Command::Init {
            repo,
            chunker_min,
            chunker_avg,
            chunker_max,
        } => {
            let backend = open_backend(&repo).await?;
            let password = password::obtain(&repo.password_file, true)?;
            // config 是唯一可覆寫的物件；被蓋掉就打不開 repo。kist 自己驗不了 bucket 設定，只能提醒。
            // （rclone:// 的 config 讀回驗證只擋同時 init，保護 config 還是要靠遠端本身的版本能力。）
            let show_versioning_note = matches!(backend.location(), RepoLocation::S3 { .. });
            let opts = InitOptions {
                chunker: kist_format::config::ChunkerParams {
                    min: chunker_min,
                    avg: chunker_avg,
                    max: chunker_max,
                },
                ..InitOptions::default()
            };
            Repository::init(backend, password.as_bytes(), opts).await?;
            println!("repository initialized at {}", repo_display(&repo)?);
            if show_versioning_note {
                eprintln!(
                    "note: enable bucket versioning or Object Lock so that `config` cannot be \
                     overwritten or deleted, and keep a copy of the `config` object somewhere safe"
                );
            }
            Ok(())
        }
        Command::Backup {
            repo,
            paths,
            client_id_file,
            gc_grace,
            parity,
        } => {
            if parity > 8 {
                anyhow::bail!("--parity must be 0..=8");
            }
            // 遠端來源：第一個路徑是 `sftp://` 或 `s3://` URL → 整個 backup
            // 的來源就是那個 URL（單一 root；client 讀遠端 → 切塊 → 加密 → 上傳）。
            if paths.len() > 1
                && paths
                    .first()
                    .and_then(|p| p.to_str())
                    .is_some_and(|u| u.starts_with("sftp://") || u.starts_with("s3://"))
            {
                anyhow::bail!(
                    "a remote source URL (sftp:// or s3://) is the only path a remote backup takes"
                );
            }
            let (source, paths) = match paths.first().and_then(|p| p.to_str()) {
                Some(url) if url.starts_with("sftp://") || url.starts_with("s3://") => {
                    let url = url.to_owned();
                    (SourceSpec::Url(url.clone()), vec![PathBuf::from(url)])
                }
                _ => (SourceSpec::LocalPaths, paths),
            };
            let r = open_repo(&repo).await?;
            let client_id = client_id::load_or_create(client_id_file.as_deref())?;
            let _lock = client_id::lock(client_id_file.as_deref())?;
            let opts = BackupOptions {
                client_id,
                hostname: hostname(),
                username: username(),
                now: None,
                gc_grace,
                parity,
                progress: None,
                source,
            };
            let summary = r.backup(&paths, opts).await?;
            let s = summary.stats;
            let rep = summary.report;
            if json {
                print_json(&summary)?;
            } else {
                println!("snapshot {}", short_snapshot_id(&summary.snapshot_key));
                println!(
                    "  {} files, {} dirs, {} symlinks, {} total",
                    s.files,
                    s.dirs,
                    s.symlinks,
                    human_bytes(s.bytes)
                );
                println!(
                    "  new: {} in {} chunks, {} packs written",
                    human_bytes(rep.bytes_stored),
                    rep.chunks_new,
                    rep.packs_new
                );
            }
            if rep.errors > 0 {
                // snapshot 已經寫出（不含那些項目）；結束碼 3 讓排程器知道要看警告
                return Err(Incomplete(format!(
                    "{} item(s) could not be read and were skipped (see warnings above)",
                    rep.errors
                ))
                .into());
            }
            Ok(())
        }
        Command::Snapshots { repo } => {
            let r = open_repo(&repo).await?;
            let snaps = r.list_snapshots().await?;
            if json {
                let items: Vec<SnapshotJson> = snaps.iter().map(SnapshotJson::new).collect();
                print_json(&items)?;
                return Ok(());
            }
            if snaps.is_empty() {
                println!("no snapshots");
                return Ok(());
            }
            println!(
                "{:<8} {:<25} {:<19} {:<12} {:>8} {:>10}  PATHS",
                "CLIENT", "TIMESTAMP", "TIME (UTC)", "HOST", "FILES", "SIZE"
            );
            for s in snaps {
                let paths: Vec<String> = s
                    .snapshot
                    .roots
                    .iter()
                    .map(|r| String::from_utf8_lossy(r.path.as_slice()).into_owned())
                    .collect();
                println!(
                    "{:<8} {:<25} {:<19} {:<12} {:>8} {:>10}  {}",
                    &s.client_hex()[..8.min(s.client_hex().len())],
                    s.timestamp(),
                    display_time_ns(s.snapshot.time_ns),
                    s.snapshot.host,
                    s.snapshot.stats.files,
                    human_bytes(s.snapshot.stats.bytes),
                    paths.join(", ")
                );
            }
            Ok(())
        }
        #[cfg(unix)]
        Command::Mount { repo, mountpoint } => {
            let r = open_repo(&repo).await?;
            let mounted =
                kist_mount::mount(r, &mountpoint, kist_mount::MountConfig::default()).await?;
            eprintln!(
                "mounted at {}; interrupt (Ctrl-C) to unmount",
                mountpoint.display()
            );
            tokio::select! {
                res = tokio::signal::ctrl_c() => {
                    res.context("installing the Ctrl-C handler")?;
                }
                res = sigterm() => {
                    res.context("installing the SIGTERM handler")?;
                }
            }
            // 有檔案還開著時 FUSE 會回 EBUSY——把原錯誤帶上人話。
            mounted.unmount().context(format!(
                "unmounting {}; is something still open under it?",
                mountpoint.display()
            ))
        }
        Command::Restore {
            repo,
            snapshot,
            target,
        } => {
            let r = open_repo(&repo).await?;
            let key = r.resolve_snapshot(&snapshot).await?;
            let summary = r.restore(&key, &target, RestoreOptions::default()).await?;
            if json {
                print_json(&summary)?;
            } else {
                println!(
                    "restored {} to {}: {} files, {} dirs, {} symlinks",
                    short_snapshot_id(&key),
                    target.display(),
                    summary.files,
                    summary.dirs,
                    summary.symlinks
                );
            }
            if !summary.errors.is_empty() {
                for e in &summary.errors {
                    eprintln!("error: {e}");
                }
                return Err(Incomplete(format!(
                    "{} item(s) could not be restored",
                    summary.errors.len()
                ))
                .into());
            }
            Ok(())
        }
        Command::Forget {
            repo,
            snapshots,
            keep_last,
            keep_hourly,
            keep_daily,
            keep_weekly,
            keep_monthly,
            keep_yearly,
            keep_within,
            dry_run,
            prune,
            prune_args,
        } => {
            let r = open_repo(&repo).await?;
            let mut keys = Vec::new();
            for spec in &snapshots {
                keys.push(r.resolve_snapshot(spec).await?);
            }
            let policy = RetentionPolicy {
                keep_last,
                keep_hourly,
                keep_daily,
                keep_weekly,
                keep_monthly,
                keep_yearly,
                keep_within,
            };
            let summary = r
                .forget(ForgetOptions {
                    snapshots: keys,
                    policy,
                    dry_run,
                })
                .await?;
            if json {
                // --prune 時 forget 的結果併進下面的合併 JSON，這裡不先印
                if !prune {
                    print_json(&ForgetJson::new(dry_run, &summary))?;
                }
            } else {
                let verb = if dry_run { "would remove" } else { "removed" };
                for key in &summary.removed {
                    println!("{verb} {}", short_snapshot_id(key));
                }
                for (key, reasons) in &summary.kept {
                    println!(
                        "keep    {} ({})",
                        short_snapshot_id(key),
                        reasons.join(", ")
                    );
                }
                println!(
                    "{verb} {} snapshot(s), kept {}",
                    summary.removed.len(),
                    summary.kept.len()
                );
            }
            if prune {
                let prune_result = r.prune(prune_args.options(dry_run)).await;
                if json {
                    // stdout 只能有一個 JSON 值：forget 與 prune 包在一起。
                    // prune 失敗時 snapshot 已經刪了，仍要把 forget 的結果印出來
                    //（prune 為 null、結束碼 1、錯誤在 stderr），消費者才知道刪了哪些。
                    let prune_json = prune_result
                        .as_ref()
                        .ok()
                        .map(|report| PruneJson { dry_run, report });
                    print_json(&serde_json::json!({
                        "forget": ForgetJson::new(dry_run, &summary),
                        "prune": prune_json,
                    }))?;
                }
                let report = prune_result?;
                if !json {
                    output_prune(&report, dry_run)?;
                }
                prune_warnings_and_exit(&report)
            } else {
                if !json && !dry_run && !summary.removed.is_empty() {
                    println!("run `kist prune` to reclaim the space");
                }
                Ok(())
            }
        }
        Command::Prune {
            repo,
            prune,
            dry_run,
        } => {
            let r = open_repo(&repo).await?;
            let report = r.prune(prune.options(dry_run)).await?;
            if json {
                print_json(&PruneJson {
                    dry_run,
                    report: &report,
                })?;
            } else {
                output_prune(&report, dry_run)?;
            }
            prune_warnings_and_exit(&report)
        }
        Command::RebuildIndex { repo } => {
            let r = open_repo(&repo).await?;
            let s = r.rebuild_index().await?;
            if json {
                print_json(&s)?;
            } else {
                println!(
                    "rebuilt index from {} packs ({} chunks); {} old index object(s) superseded",
                    s.packs, s.chunks, s.superseded
                );
            }
            Ok(())
        }
        Command::Check {
            repo,
            read_data,
            repair,
        } => {
            let r = open_repo(&repo).await?;
            let report = r.check(CheckOptions { read_data, repair }).await?;
            for id in &report.repaired {
                println!("repaired pack {id} from parity");
            }
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "checked {} snapshots, {} trees, {} packs, {} chunks{}",
                    report.snapshots,
                    report.trees,
                    report.packs,
                    report.chunks,
                    if read_data { " (data read)" } else { "" }
                );
            }
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            if report.errors.is_empty() {
                if !json {
                    println!("no errors found");
                }
                Ok(())
            } else {
                for e in &report.errors {
                    eprintln!("error: {e}");
                }
                bail!("{} error(s) found", report.errors.len());
            }
        }
    }
}

fn output_prune(p: &PruneReport, dry_run: bool) -> Result<()> {
    let would = if dry_run { "would " } else { "" };
    println!(
        "{} snapshots, {} live trees, {} live packs",
        p.snapshots, p.live_trees, p.live_packs
    );
    println!(
        "{would}marked {} object(s) ({}) for deletion; {} marker(s) revived, {} stale",
        p.marked,
        human_bytes(p.marked_bytes),
        p.revived,
        p.stale_marks
    );
    println!(
        "{would}deleted {} object(s) ({}); {} waiting for the grace period, {} held back by active clients",
        p.deleted,
        human_bytes(p.deleted_bytes),
        p.waiting,
        p.blocked
    );
    println!(
        "{would}repacked {} pack(s) ({} of live data moved into {} new pack(s))",
        p.repacked_packs,
        human_bytes(p.repacked_bytes),
        p.new_packs
    );
    prune_warnings_and_exit(p)
}

/// 刪不掉的物件走 stderr + 結束碼 3，兩種輸出模式都一樣。
fn prune_warnings_and_exit(p: &PruneReport) -> Result<()> {
    for s in &p.skipped {
        eprintln!("warning: {s}");
    }
    if !p.skipped.is_empty() {
        return Err(Incomplete(format!(
            "{} object(s) could not be deleted (object lock or permissions); their markers were kept",
            p.skipped.len()
        ))
        .into());
    }
    Ok(())
}

/// 結果序列化成 pretty JSON（`--json`）。錯誤照舊走 stderr。
fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(v).context("cannot encode result as JSON")?;
    println!("{text}");
    Ok(())
}

/// daemon（`run` / `serve`）每件工作結束的輸出：人類可讀一行，或 NDJSON 一行。
fn emit_outcome_wrap(json: bool) -> impl FnMut(&kist_app::JobOutcome) {
    move |o: &kist_app::JobOutcome| {
        if json {
            match serde_json::to_string(o) {
                Ok(line) => println!("{line}"),
                Err(e) => eprintln!("error: cannot encode outcome as JSON: {e}"),
            }
        } else {
            emit_outcome(o);
        }
    }
}

fn emit_outcome(o: &kist_app::JobOutcome) {
    println!(
        "{} {} ({:.1}s){}",
        o.job.name(),
        o.status.name(),
        o.duration_secs,
        o.error
            .as_ref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    );
}

fn spawn_ctrl_c(tx: tokio::sync::watch::Sender<bool>) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("shutting down after the current job");
            let _ = tx.send(true);
        }
    });
}

fn print_next_runs(daemon: &kist_app::Daemon) {
    for (job, next) in daemon.next_runs(time_now()) {
        eprintln!(
            "{}: next run {}",
            job.name(),
            next.map(|t| t.to_string())
                .unwrap_or_else(|| "never".to_owned())
        );
    }
}

/// `snapshots --json` 的輸出形狀：key 拆出 client 與 timestamp，路徑從 OS bytes
/// 轉成 UTF-8（非法位元組以 U+FFFD 取代）；root 等 id 在 JSON 是 hex 字串。
#[derive(serde::Serialize)]
struct SnapshotJson {
    key: String,
    client: String,
    timestamp: String,
    /// RFC 3339（backup 開始時間）。
    time: String,
    hostname: String,
    username: String,
    /// v3：roots 的定位字串（lossy UTF-8）。
    paths: Vec<String>,
    roots: Vec<kist_format::snapshot::Root>,
    stats: kist_format::snapshot::SnapshotStats,
}

impl SnapshotJson {
    fn new(s: &kist_core::SnapshotInfo) -> Self {
        Self {
            key: s.key.clone(),
            client: s.client_hex().to_owned(),
            timestamp: s.timestamp().to_owned(),
            time: display_time_ns(s.snapshot.time_ns),
            hostname: s.snapshot.host.clone(),
            username: s.snapshot.user.clone(),
            paths: s
                .snapshot
                .roots
                .iter()
                .map(|r| String::from_utf8_lossy(r.path.as_slice()).into_owned())
                .collect(),
            roots: s.snapshot.roots.clone(),
            stats: s.snapshot.stats,
        }
    }
}

/// `forget --json`：dry_run 標示 removed 是「會刪」還是「已刪」；內部的
/// (key, reasons) tuple 包成有名字的欄位，位置語意太脆弱。
#[derive(serde::Serialize)]
struct ForgetJson {
    dry_run: bool,
    removed: Vec<String>,
    kept: Vec<ForgetKept>,
}

#[derive(serde::Serialize)]
struct ForgetKept {
    snapshot: String,
    reasons: Vec<String>,
}

impl ForgetJson {
    fn new(dry_run: bool, s: &ForgetSummary) -> Self {
        Self {
            dry_run,
            removed: s.removed.clone(),
            kept: s
                .kept
                .iter()
                .map(|(key, reasons)| ForgetKept {
                    snapshot: key.clone(),
                    reasons: reasons.iter().map(|r| (*r).to_owned()).collect(),
                })
                .collect(),
        }
    }
}

/// `prune --json`：加上 dry_run（report 本身不帶，但消費者需要知道刪了沒）。
#[derive(serde::Serialize)]
struct PruneJson<'a> {
    dry_run: bool,
    #[serde(flatten)]
    report: &'a PruneReport,
}

fn repo_url(args: &RepoArgs) -> Result<&str> {
    args.repo
        .as_deref()
        .context("no repository given: use --repo <path|s3://bucket/prefix|sftp://host/path|rclone://remote/path> or set KIST_REPO")
}

fn repo_display(args: &RepoArgs) -> Result<String> {
    Ok(repo_url(args)?.to_owned())
}

async fn open_backend(args: &RepoArgs) -> Result<Backend> {
    Ok(Backend::from_url(repo_url(args)?).await?)
}

/// `kist mount` 的第二個結束訊號（systemd/終端機都會送 SIGTERM）。
#[cfg(unix)]
async fn sigterm() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    term.recv().await;
    Ok(())
}

async fn open_repo(args: &RepoArgs) -> Result<Repository> {
    let backend = open_backend(args).await?;
    let password = password::obtain(&args.password_file, false)?;
    let cache_root = if args.no_cache {
        None
    } else {
        match &args.cache_dir {
            Some(d) => Some(d.clone()),
            None => dirs::cache_dir().map(|d| d.join("kist")),
        }
    };
    if cache_root.is_none() && !args.no_cache {
        tracing::warn!("cannot determine a cache directory; running without the local index cache");
    }
    Ok(Repository::open_with_cache(backend, password.as_bytes(), cache_root).await?)
}

/// `snapshots/<client>/<ts>` → `<client 前 8 碼>/<ts>`，給人看的簡短 id。
fn short_snapshot_id(key: &str) -> String {
    let mut parts = key.rsplit('/');
    let ts = parts.next().unwrap_or(key);
    let client = parts.next().unwrap_or("");
    format!("{}/{ts}", &client[..8.min(client.len())])
}

/// snapshot 的 i64 奈秒 → 列表用的時間字串（顯示到秒）。
fn display_time_ns(ns: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns))
        .map(|t| {
            t.to_offset(time::UtcOffset::UTC)
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default()
        })
        .map(|rfc| display_time(&rfc))
        .unwrap_or_default()
}

/// RFC 3339 的奈秒字串太長，列表只顯示到秒：`2026-09-04 15:04:02`。
fn display_time(rfc3339: &str) -> String {
    rfc3339
        .get(..19)
        .map(|s| s.replacen('T', " ", 1))
        .unwrap_or_else(|| rfc3339.to_owned())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn hostname() -> String {
    client_id::hostname()
}

fn username() -> String {
    client_id::username()
}

fn time_now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}
