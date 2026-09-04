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
use kist_core::{BackupOptions, CheckOptions, InitOptions, Repository, RestoreOptions};

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
    /// Repository location (a local directory for now).
    #[arg(long, short = 'r', env = "KIST_REPO", global = true)]
    repo: Option<PathBuf>,

    /// Read the repository password from this file (first line).
    /// Otherwise the KIST_PASSWORD environment variable is used, or you are prompted.
    #[arg(long, env = "KIST_PASSWORD_FILE", global = true)]
    password_file: Option<PathBuf>,
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
        eprintln!("error: {e:#}");
        std::process::exit(1);
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
            Repository::init(backend, password.as_bytes(), InitOptions::default()).await?;
            println!("repository initialized at {}", repo_display(&repo)?);
            Ok(())
        }
        Command::Backup {
            repo,
            paths,
            client_id_file,
        } => {
            let r = open_repo(&repo).await?;
            let client_id = client_id::load_or_create(client_id_file.as_deref())?;
            let opts = BackupOptions {
                client_id,
                hostname: hostname(),
                username: username(),
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
                // snapshot 已經寫出（不含那些項目）；用非 0 結束讓排程器知道要看警告
                bail!(
                    "{} item(s) could not be read and were skipped (see warnings above)",
                    s.errors
                );
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
            r.restore(&key, &target, RestoreOptions::default()).await?;
            println!(
                "restored {} to {}",
                short_snapshot_id(&key),
                target.display()
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

fn repo_path(args: &RepoArgs) -> Result<&PathBuf> {
    args.repo
        .as_ref()
        .context("no repository given: use --repo <path> or set KIST_REPO")
}

fn repo_display(args: &RepoArgs) -> Result<String> {
    Ok(repo_path(args)?.display().to_string())
}

fn open_backend(args: &RepoArgs) -> Result<Backend> {
    Ok(Backend::local(repo_path(args)?)?)
}

async fn open_repo(args: &RepoArgs) -> Result<Repository> {
    let backend = open_backend(args)?;
    let password = password::obtain(&args.password_file, false)?;
    Ok(Repository::open(backend, password.as_bytes()).await?)
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
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}
