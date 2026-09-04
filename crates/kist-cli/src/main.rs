//! `kist` 命令列入口：解析參數、取得密碼與 client id，然後把工作交給 `kist-core`。
//!
//! 所有行為都在 `kist-core`；這裡只做輸入輸出。錯誤一律用 `anyhow` 往上拋，
//! 在 `main` 印成一行後以非 0 結束。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod client_id;
mod password;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use kist_backend::Backend;
use kist_core::{
    BackupOptions, CheckOptions, ForgetOptions, InitOptions, Repository, RestoreOptions,
    RetentionPolicy,
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
    #[command(subcommand)]
    command: Command,
}

/// 每個需要 repo 的命令共用的參數。
#[derive(Debug, Args)]
struct RepoArgs {
    /// Repository location: a local directory, or `s3://bucket[/prefix]`.
    /// For S3 set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_DEFAULT_REGION,
    /// plus AWS_ENDPOINT (and AWS_ALLOW_HTTP=true) for MinIO and other S3-compatible services.
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
    },
    /// Back up one or more paths into a new snapshot.
    Backup {
        #[command(flatten)]
        repo: RepoArgs,
        /// Files or directories to back up.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// File holding this machine's client id (created on first use).
        #[arg(long, env = "KIST_CLIENT_ID_FILE")]
        client_id_file: Option<PathBuf>,
        /// Packs marked for deletion longer ago than this are treated as already gone.
        /// Must match the `--grace` used by `prune`.
        #[arg(long, value_name = "DURATION", default_value = "72h", value_parser = parse_duration)]
        gc_grace: std::time::Duration,
    },
    /// List snapshots.
    Snapshots {
        #[command(flatten)]
        repo: RepoArgs,
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
    },
    /// Remove snapshots, by id or by retention policy. Data is reclaimed later by `prune`.
    Forget {
        #[command(flatten)]
        repo: RepoArgs,
        /// Snapshots to remove (`latest`, a full id, or a unique timestamp prefix).
        snapshots: Vec<String>,
        /// Keep the newest N snapshots of each client/path group.
        #[arg(long, value_name = "N")]
        keep_last: Option<u32>,
        /// Keep the newest snapshot of each of the last N hours.
        #[arg(long, value_name = "N")]
        keep_hourly: Option<u32>,
        /// Keep the newest snapshot of each of the last N days.
        #[arg(long, value_name = "N")]
        keep_daily: Option<u32>,
        /// Keep the newest snapshot of each of the last N ISO weeks.
        #[arg(long, value_name = "N")]
        keep_weekly: Option<u32>,
        /// Keep the newest snapshot of each of the last N months.
        #[arg(long, value_name = "N")]
        keep_monthly: Option<u32>,
        /// Keep the newest snapshot of each of the last N years.
        #[arg(long, value_name = "N")]
        keep_yearly: Option<u32>,
        /// Keep every snapshot newer than this (e.g. `36h`, `14d`, `2w`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        keep_within: Option<std::time::Duration>,
        /// Show what would be removed without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebuild the index from the pack files (after index objects were lost or corrupted).
    RebuildIndex {
        #[command(flatten)]
        repo: RepoArgs,
    },
    /// Print version information.
    Version,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
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
    match cli.command {
        Command::Version => {
            println!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Init { repo } => {
            let backend = open_backend(&repo)?;
            let password = password::obtain(&repo.password_file, true)?;
            let remote = backend.location().is_remote();
            Repository::init(backend, password.as_bytes(), InitOptions::default()).await?;
            println!("repository initialized at {}", repo_display(&repo)?);
            if remote {
                // config 是唯一可覆寫的物件；被蓋掉就打不開 repo。kist 自己驗不了 bucket 設定，只能提醒。
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
        } => {
            let r = open_repo(&repo).await?;
            let client_id = client_id::load_or_create(client_id_file.as_deref())?;
            let opts = BackupOptions {
                client_id,
                hostname: hostname(),
                username: username(),
                now: None,
                gc_grace,
            };
            let summary = r.backup(&paths, opts).await?;
            let s = summary.stats;
            println!("snapshot {}", short_snapshot_id(&summary.snapshot_key));
            println!(
                "  {} files, {} dirs, {} symlinks, {} total",
                s.files,
                s.dirs,
                s.symlinks,
                human_bytes(s.bytes_total)
            );
            println!(
                "  new: {} in {} chunks, {} packs written",
                human_bytes(s.bytes_new),
                s.chunks_new,
                s.packs_new
            );
            if s.errors > 0 {
                // snapshot 已經寫出（不含那些項目）；結束碼 3 讓排程器知道要看警告
                return Err(Incomplete(format!(
                    "{} item(s) could not be read and were skipped (see warnings above)",
                    s.errors
                ))
                .into());
            }
            Ok(())
        }
        Command::Snapshots { repo } => {
            let r = open_repo(&repo).await?;
            let snaps = r.list_snapshots().await?;
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
                    .paths
                    .iter()
                    .map(|p| String::from_utf8_lossy(p).into_owned())
                    .collect();
                println!(
                    "{:<8} {:<25} {:<19} {:<12} {:>8} {:>10}  {}",
                    &s.client_hex()[..8.min(s.client_hex().len())],
                    s.timestamp(),
                    display_time(&s.snapshot.time),
                    s.snapshot.hostname,
                    s.snapshot.stats.files,
                    human_bytes(s.snapshot.stats.bytes_total),
                    paths.join(", ")
                );
            }
            Ok(())
        }
        Command::Restore {
            repo,
            snapshot,
            target,
        } => {
            let r = open_repo(&repo).await?;
            let key = r.resolve_snapshot(&snapshot).await?;
            let summary = r.restore(&key, &target, RestoreOptions::default()).await?;
            println!(
                "restored {} to {}: {} files, {} dirs, {} symlinks",
                short_snapshot_id(&key),
                target.display(),
                summary.files,
                summary.dirs,
                summary.symlinks
            );
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
                    now: None,
                })
                .await?;
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
            if !dry_run && !summary.removed.is_empty() {
                println!("run `kist prune` to reclaim the space");
            }
            Ok(())
        }
        Command::RebuildIndex { repo } => {
            let r = open_repo(&repo).await?;
            let s = r.rebuild_index().await?;
            println!(
                "rebuilt index from {} packs ({} chunks); {} old index object(s) superseded",
                s.packs, s.chunks, s.superseded
            );
            Ok(())
        }
        Command::Check { repo, read_data } => {
            let r = open_repo(&repo).await?;
            let report = r.check(CheckOptions { read_data }).await?;
            println!(
                "checked {} snapshots, {} trees, {} packs, {} chunks{}",
                report.snapshots,
                report.trees,
                report.packs,
                report.chunks,
                if read_data { " (data read)" } else { "" }
            );
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            if report.errors.is_empty() {
                println!("no errors found");
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

/// `<number><unit>`，單位 s / m / h / d / w（週）。給 clap 用。
fn parse_duration(s: &str) -> std::result::Result<std::time::Duration, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{s:?}: missing unit (s, m, h, d, w)"))?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("{s:?}: not a number"))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(format!("{s:?}: unknown unit {unit:?} (use s, m, h, d, w)")),
    };
    n.checked_mul(secs)
        .map(std::time::Duration::from_secs)
        .ok_or_else(|| format!("{s:?}: too large"))
}

fn repo_url(args: &RepoArgs) -> Result<&str> {
    args.repo
        .as_deref()
        .context("no repository given: use --repo <path|s3://bucket/prefix> or set KIST_REPO")
}

fn repo_display(args: &RepoArgs) -> Result<String> {
    Ok(repo_url(args)?.to_owned())
}

fn open_backend(args: &RepoArgs) -> Result<Backend> {
    Ok(Backend::from_url(repo_url(args)?)?)
}

async fn open_repo(args: &RepoArgs) -> Result<Repository> {
    let backend = open_backend(args)?;
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
    let name = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_owned();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name
    }
}

fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}
