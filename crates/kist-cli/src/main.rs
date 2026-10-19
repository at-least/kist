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
use kist_app::size::parse_size;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use kist_backend::Backend;
use kist_core::retention::RetentionPolicy;
use kist_core::v4::check::CheckOptions;
use kist_core::v4::{
    BackupOptions, ForgetOptions, ForgetSummary, InitOptions, MaintOptions, PruneOptions,
    PruneReport, Repo, SettingsChange,
};
use kist_core::{Exclusions, PathFilter, Planned, RestoreOptions, RestoreSummary, SourceSpec};

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
    /// Repack old packs whose live data is below this percentage (0 disables ratio repacks;
    /// damaged packs that their parity repairs are always repacked).
    #[arg(long, value_name = "PERCENT", default_value_t = kist_core::v4::prune::DEFAULT_REPACK_BELOW, value_parser = clap::value_parser!(u8).range(0..=100))]
    repack_below: u8,
    /// Also read every live data pack in full and verify every chunk it holds (slow: downloads all
    /// the data). A damaged pack that its parity repairs is repacked; a chunk that cannot be repaired
    /// is reported and dropped from the record, so the next prune refuses until the snapshots that
    /// need it are forgotten. Stops half-way through the time limit and continues next time.
    /// Metadata packs are verified by every prune already.
    #[arg(long)]
    read_data: bool,
}

impl PruneArgs {
    fn options(&self) -> PruneOptions {
        PruneOptions {
            repack_below: self.repack_below,
            read_data: self.read_data,
            ..PruneOptions::default()
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
        /// Grace of generation 0 (at least 2h; default 3d). Set it here rather than lowering it
        /// with `settings` afterwards: lowering drains for one old grace, during which new marks
        /// still age by the old one.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        grace: Option<std::time::Duration>,
        /// 1 also writes a `.r1` copy of every mpack, snapshot and map shard (default: 1 for a
        /// local repository, 0 for S3, SFTP and rclone).
        #[arg(long, value_name = "0|1", value_parser = clap::value_parser!(u8).range(0..=1))]
        replicas: Option<u8>,
        /// Size at which a data pack is written out (64k to 4g; default 64m).
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        pack_target: Option<u64>,
        /// Size at which a metadata pack is written out (64k to 4g; default 8m).
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        mpack_target: Option<u64>,
    },
    /// Back up one or more paths into a new snapshot. A single `sftp://` or
    /// `s3://` URL backs up that remote source directly (the client reads,
    /// chunks and encrypts; keys never leave this machine).
    Backup {
        #[command(flatten)]
        repo: RepoArgs,
        /// Files or directories to back up, or one remote source URL.
        #[arg(required_unless_present_any = ["files_from", "files_from_verbatim", "files_from_raw"])]
        paths: Vec<PathBuf>,
        /// Read more source paths from FILE (`-` = standard input, at most once per command),
        /// one per line (repeatable). Surrounding whitespace is trimmed; empty lines and lines
        /// starting with `#` are skipped; a leading UTF-8 byte order mark and CRLF line ends
        /// are accepted. Paths are taken literally: wildcards are not expanded.
        #[arg(long, value_name = "FILE")]
        files_from: Vec<PathBuf>,
        /// Like --files-from, but every line is a path exactly as written (no trimming, `#`
        /// is not a comment; a CR before the line end is dropped); only empty lines are
        /// skipped.
        #[arg(long, value_name = "FILE")]
        files_from_verbatim: Vec<PathBuf>,
        /// Like --files-from, but every path ends with a NUL byte (as written by
        /// `find -print0`), so any file name works. An empty name, or a last name without its
        /// NUL, is an error.
        #[arg(long, value_name = "FILE")]
        files_from_raw: Vec<PathBuf>,
        /// File holding this machine's client id (created on first use).
        #[arg(long, env = "KIST_CLIENT_ID_FILE")]
        client_id_file: Option<PathBuf>,
        /// Reed-Solomon parity shards per pack, out of 16 data shards
        /// (0 = none; 2 = 12.5% overhead, repairs up to 2 damaged sixteenths).
        #[arg(long, value_name = "0..=8", default_value = "0")]
        parity: u8,
        /// Leave out files and directories matching PATTERN (repeatable). A leading `/`
        /// anchors it at the top of the snapshot path (for local sources the canonical
        /// absolute path); otherwise it matches at any depth. `*`, `?` and `[...]` stay
        /// within one path component, a `**` component spans any number. The named
        /// source paths themselves are never excluded.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Read exclude patterns from FILE (`-` = standard input), one per line (repeatable).
        /// Surrounding whitespace is trimmed; empty lines and lines starting with `#` are
        /// skipped; a leading UTF-8 byte order mark and CRLF line ends are accepted.
        #[arg(long, value_name = "FILE")]
        exclude_file: Vec<PathBuf>,
        /// Like --exclude, ignoring case (repeatable). For `!` patterns the order is:
        /// --exclude, --exclude-file lines, --iexclude, --iexclude-file lines; the last
        /// pattern matching a path decides.
        #[arg(long, value_name = "PATTERN")]
        iexclude: Vec<String>,
        /// Like --exclude-file, the patterns ignoring case (repeatable).
        #[arg(long, value_name = "FILE")]
        iexclude_file: Vec<PathBuf>,
        /// Leave out the contents of every directory that contains NAME (repeatable). With
        /// `NAME:HEADER`, only when NAME is a file that starts with HEADER. The directory
        /// and the marker itself are kept.
        #[arg(long, value_name = "NAME[:HEADER]")]
        exclude_if_present: Vec<String>,
        /// Leave out the contents of cache directories tagged with a CACHEDIR.TAG file that
        /// carries the standard signature (https://bford.info/cachedir/); the tag is kept.
        #[arg(long)]
        exclude_caches: bool,
        /// Leave out files larger than SIZE: bytes, or a number with k, m, g or t
        /// (powers of 1024). Named source files are kept.
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        exclude_larger_than: Option<u64>,
        /// Do not descend into other file systems (mount points, btrfs subvolumes): such
        /// directories are kept as empty directories. Local source paths on unix only.
        #[arg(long)]
        one_file_system: bool,
        /// sftp:// sources only: reuse a file's previous contents without reading it when its
        /// size and modification time match the parent snapshot and that second ended before the
        /// parent backup started. Off by default: whoever writes a file can set both (`touch -d`,
        /// `cp -p`), so a changed file whose size and time stay the same is missed until the next
        /// full read; a server clock behind this machine's weakens the check by that much.
        #[arg(long)]
        trust_mtime: bool,
        /// With --trust-mtime: read everything again when this machine's last full read of the
        /// source is older than DURATION (default 7d), or when there is no record of one (no
        /// local cache, --no-cache, or the first backup from here).
        #[arg(
            long,
            value_name = "DURATION",
            value_parser = parse_duration,
            requires = "trust_mtime"
        )]
        reread_every: Option<std::time::Duration>,
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
        /// Snapshot to restore: `latest`, a full id, or a unique timestamp prefix. Append
        /// `:PATH` (a directory in the snapshot, e.g. `latest:/home/me/docs`) to restore only
        /// that directory's contents straight into TARGET.
        snapshot: String,
        /// Directory to restore into (the original absolute paths are recreated beneath it).
        /// Should be empty: existing files are overwritten; symlinks in the way (pre-existing or snapshot-planted) are refused, not followed.
        target: PathBuf,
        /// Restore only nodes matching PATTERN and everything below them (repeatable); the
        /// directories on the way are created with their recorded metadata. Patterns match
        /// the node's path in the snapshot (for local sources the original absolute path),
        /// with the same rules as `backup --exclude`, also with SNAPSHOT:PATH. A pattern
        /// that selects nothing (no match, or every match excluded) is reported and the
        /// exit code is 3.
        #[arg(long, value_name = "PATTERN")]
        include: Vec<String>,
        /// Read --include patterns from FILE (`-` = standard input), one per line (repeatable;
        /// same format as `backup --exclude-file`). A file without any pattern is an error:
        /// it would restore everything.
        #[arg(long, value_name = "FILE")]
        include_file: Vec<PathBuf>,
        /// Like --include, ignoring case (repeatable).
        #[arg(long, value_name = "PATTERN")]
        iinclude: Vec<String>,
        /// Like --include-file, the patterns ignoring case (repeatable).
        #[arg(long, value_name = "FILE")]
        iinclude_file: Vec<PathBuf>,
        /// Do not restore nodes matching PATTERN (a directory with everything below it;
        /// repeatable). Wins over --include.
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Read --exclude patterns from FILE (`-` = standard input), one per line (repeatable).
        #[arg(long, value_name = "FILE")]
        exclude_file: Vec<PathBuf>,
        /// Like --exclude, ignoring case (repeatable).
        #[arg(long, value_name = "PATTERN")]
        iexclude: Vec<String>,
        /// Like --exclude-file, the patterns ignoring case (repeatable).
        #[arg(long, value_name = "FILE")]
        iexclude_file: Vec<PathBuf>,
        /// List what would be restored (`dir`, `file`, `link` and the path under TARGET)
        /// without writing anything; TARGET is not even created. Only trees are read, so
        /// errors that need file data (a damaged chunk) do not show up.
        #[arg(long)]
        dry_run: bool,
    },
    /// Verify the repository's integrity (always reads the repository, never the local cache).
    Check {
        #[command(flatten)]
        repo: RepoArgs,
        /// Also download every pack and verify every chunk (slow).
        #[arg(long)]
        read_data: bool,
        /// Create missing copies (a head's or a map shard's or an mpack's body from its verified
        /// `.r1`, or the `.r1` from the body) (implies --read-data). Never overwrites: damaged
        /// objects that parity or a `.r1` can repair are reported, and prune rewrites what it
        /// reads. Maintenance role; refused on rclone://.
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
        /// Also remove a snapshot named on the command line that is the newest of its client and
        /// paths. That retires those paths for that client: do it only while that client has no
        /// backup running (its next backup reuses the newest snapshot as its parent).
        #[arg(long)]
        retire: bool,
        /// Run `prune` afterwards (the prune options below apply; not with --dry-run).
        #[arg(long, conflicts_with = "dry_run")]
        prune: bool,
        #[command(flatten)]
        prune_args: PruneArgs,
    },
    /// Reclaim space (format-v4.md §11.3): mark unreferenced packs, delete what was marked more
    /// than a grace ago on both the backend and the writer clock, and repack mostly-unused
    /// packs. Safe to run while backups are running. Maintenance role; refused on rclone://.
    Prune {
        #[command(flatten)]
        repo: RepoArgs,
        #[command(flatten)]
        prune: PruneArgs,
    },
    /// Rebuild the map after the newest generation's map shards were lost (format-v4.md §11.4).
    /// Deletes nothing; holds back deletion for a while afterwards.
    Rebuild {
        #[command(flatten)]
        repo: RepoArgs,
    },
    /// Show the repository settings, or change them (a new generation; nothing is deleted).
    Settings {
        #[command(flatten)]
        repo: RepoArgs,
        /// Time between marking a pack and deleting it (at least 2h). Raising takes effect at
        /// once; lowering drains for one old grace. Every backup must finish within it minus an
        /// hour.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        grace: Option<std::time::Duration>,
        /// 1 also writes a `.r1` copy of every mpack, snapshot and map shard (0 or 1).
        #[arg(long, value_name = "0|1", value_parser = clap::value_parser!(u8).range(0..=1))]
        replicas: Option<u8>,
        /// Size at which a data pack is written out (64k to 4g).
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        pack_target: Option<u64>,
        /// Size at which a metadata pack is written out (64k to 4g).
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        mpack_target: Option<u64>,
    },
    /// Manage the password slots: every slot opens the repository with its own password.
    Key {
        #[command(flatten)]
        repo: RepoArgs,
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Mark packs written by a newer kist (unknown trailer version) for deletion by later
    /// prunes. Use only when no newer kist uses this repository.
    RemoveUnknownPacks {
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

#[derive(Debug, Subcommand)]
enum KeyAction {
    /// Add a slot for a new password (prompted twice, or read from --new-password-file).
    Add {
        /// Read the new password from this file (first line).
        #[arg(long)]
        new_password_file: Option<PathBuf>,
    },
    /// List the slots; `*` marks the one this command opened the repository with.
    List,
    /// Remove a slot (maintenance role). The slot used to open the repository cannot be removed,
    /// so the last slot always stays.
    Remove {
        /// Slot name, as `key list` shows it.
        slot: String,
    },
}

/// `--files-from*` 的三種切法（ADR 024，名稱同 restic；都不展開萬用字元）。文字的兩種：開頭的
/// UTF-8 BOM 去掉，行尾的 `\r` 去掉（[`text_lines`]）。
#[derive(Clone, Copy)]
enum Listing {
    /// 一行一個；去掉前後空白，空行與 `#` 開頭的行略過。
    Plain,
    /// 一行一個，照原樣；只略過空行。
    Verbatim,
    /// NUL 結尾，照原樣（名字裡可以有換行；[`raw_names`]）。
    Raw,
}

/// `--files-from*` 的每個檔案（`-` 是標準輸入）裡列的來源路徑。
fn listed_paths(files: &[PathBuf], how: Listing) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for file in files {
        let bytes = read_input(file, "source paths")?;
        let names: Vec<&[u8]> = match how {
            Listing::Plain => text_lines(&bytes)
                .map(trim_line)
                .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
                .collect(),
            Listing::Verbatim => text_lines(&bytes).filter(|line| !line.is_empty()).collect(),
            Listing::Raw => {
                raw_names(&bytes).with_context(|| format!("--files-from-raw {}", file.display()))?
            }
        };
        for name in names {
            out.push(
                path_from_bytes(name)
                    .with_context(|| format!("a source path listed in {}", file.display()))?,
            );
        }
    }
    Ok(out)
}

/// 清單或樣式檔的內容；`-` 是標準輸入（[`stdin_at_most_once`] 先確定只有一個 `-`）。
fn read_input(file: &std::path::Path, what: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    if file.as_os_str() == "-" {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {what} from standard input"))?;
        return Ok(bytes);
    }
    std::fs::read(file).with_context(|| format!("reading {what} from {}", file.display()))
}

/// 標準輸入只能讀一次：`-` 給了不只一次時直接報錯，不然後面的默默讀到空的（審查修正）。
fn stdin_at_most_once<'a>(files: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
    if files.into_iter().filter(|f| f.as_os_str() == "-").count() > 1 {
        bail!("`-` (standard input) is given more than once; standard input can be read only once");
    }
    Ok(())
}

/// 文字清單與樣式檔的行（同 restic）：開頭的 UTF-8 BOM 去掉，以 `\n` 切，行尾的一個 `\r`
/// 去掉（CRLF）。
fn text_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    bytes
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

/// 一行的前後空白去掉：Unicode 的空白（同 restic 的 TrimSpace）；不成 UTF-8 的行只去 ASCII
/// 空白。
fn trim_line(line: &[u8]) -> &[u8] {
    match std::str::from_utf8(line) {
        Ok(text) => text.trim().as_bytes(),
        Err(_) => line.trim_ascii(),
    }
}

/// `--files-from-raw` 的名字（同 restic）：每個都以 NUL 結尾，不能是空的。
fn raw_names(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let Some(body) = bytes.strip_suffix(b"\0") else {
        bail!("the last name does not end with a NUL byte");
    };
    body.split(|&b| b == 0)
        .map(|name| {
            if name.is_empty() {
                bail!("empty name (two NUL bytes in a row, or a NUL first)");
            }
            Ok(name)
        })
        .collect()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// 非 unix：檔名必須是 UTF-8。
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(
        String::from_utf8(bytes.to_vec()).context("not UTF-8")?,
    ))
}

/// 一種樣式的來源：(旗標名, 旗標的值, 樣式檔)。
type PatternSource<'a> = (&'a str, &'a [String], &'a [PathBuf]);

/// 樣式是選取（restore 的 include）還是排除。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Include,
    Exclude,
}

/// 命令列的樣式加上樣式檔，合成一組樣式（ADR 020、023；backup 的 `--exclude`、restore 的
/// `--include`／`--exclude`）：先是區分大小寫的（旗標的值、再來樣式檔的行），然後是不分的
/// （`--i…`）——`!` 反向樣式看的就是這個順序。
fn pattern_filter(
    exact: PatternSource<'_>,
    ignore_case: PatternSource<'_>,
    role: Role,
) -> Result<PathFilter> {
    let filter = PathFilter::new(&pattern_lines(exact, role)?)?;
    Ok(filter.and_ignoring_case(&pattern_lines(ignore_case, role)?)?)
}

/// 旗標的值加上樣式檔（`-` 是標準輸入）的每一行。樣式檔一行一條（[`text_lines`]）：去掉前後
/// 空白，空行與 `#` 開頭的行略過；寫錯的在這裡就點名出處（旗標名，或 `檔案:行號`）。include 的
/// 樣式檔一條樣式都沒有時是錯（審查修正）：少了它們 include 就是空的，等於整份還原，而不是
/// 「只還原那些」。exclude 的空樣式檔只是什麼都不排除。
fn pattern_lines((flag, patterns, files): PatternSource<'_>, role: Role) -> Result<Vec<String>> {
    let mut all = patterns.to_vec();
    PathFilter::new(&all).context(flag.to_owned())?;
    for file in files {
        let bytes = read_input(file, "patterns")?;
        let before = all.len();
        for (n, line) in text_lines(&bytes).enumerate() {
            let line = trim_line(line);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            let at = || format!("{}:{}", file.display(), n + 1);
            let line = std::str::from_utf8(line).with_context(|| format!("{}: not UTF-8", at()))?;
            PathFilter::new(&[line]).with_context(at)?;
            all.push(line.to_owned());
        }
        if role == Role::Include && all.len() == before {
            bail!(
                "{flag}-file {}: no patterns (an include list without patterns would restore \
                 everything)",
                file.display()
            );
        }
    }
    Ok(all)
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
                // 約定：Ctrl-C 是乾淨收工（不再開下一件工作、
                // 進行中的做完），不是 unit 失敗。
                let (tx, rx) = tokio::sync::watch::channel(false);
                spawn_shutdown_watcher(tx.clone());
                let outcomes = daemon
                    .run_once(Some(rx.clone()), |o| {
                        if !json {
                            emit_outcome(o);
                        }
                    })
                    .await;
                let stopped = *rx.borrow();
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
                    // 真失敗贏過乾淨收工：Ctrl-C 不能把「本來就會失敗的
                    // 工作」洗成 exit 0。
                    bail!("{failed} job(s) failed");
                }
                if incomplete > 0 {
                    return Err(Incomplete(format!("{incomplete} job(s) incomplete")).into());
                }
                if stopped {
                    eprintln!("stopping after the current job");
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
            spawn_shutdown_watcher(tx.clone());
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
                    "warning: /metrics and (without [serve] password_file) the read-only web UI \
                     have no authentication; they reveal the repo location, backup source paths \
                     and job schedule; do not expose them to untrusted networks"
                );
            }
            eprintln!("listening on http://{local} (/metrics, /healthz)");
            let (tx, rx) = tokio::sync::watch::channel(false);
            spawn_shutdown_watcher(tx.clone());
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
            result?;
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
            grace,
            replicas,
            pack_target,
            mpack_target,
        } => {
            let backend = open_backend(&repo).await?;
            let password = password::obtain(&repo.password_file, true)?;
            // head 0 的 settings：沒給的用預設（replicas 看後端）。範圍由 Head::encode 檢查，不合就什麼都不寫。
            let defaults = kist_format::v4::index::Settings::defaults(
                kist_core::v4::default_replicas(backend.location()),
            );
            let settings = kist_format::v4::index::Settings {
                grace: grace.map_or(defaults.grace, |d| d.as_secs()),
                replicas: replicas.unwrap_or(defaults.replicas),
                pack_target: pack_target.unwrap_or(defaults.pack_target),
                mpack_target: mpack_target.unwrap_or(defaults.mpack_target),
            };
            let opts = InitOptions {
                chunker: kist_format::v4::config::Chunker {
                    min: chunker_min,
                    avg: chunker_avg,
                    max: chunker_max,
                },
                settings: Some(settings),
                ..InitOptions::default()
            };
            Repo::init(backend, password.as_bytes(), opts).await?;
            println!("repository initialized at {}", repo_url(&repo)?);
            // config 與金鑰槽不可覆寫、也不會被 kist 刪：遺失 config 就開不了 repo，密碼忘了也一樣。
            eprintln!(
                "note: keep a copy of the `config` object and of your password somewhere safe; \
                 without either the repository cannot be opened"
            );
            Ok(())
        }
        Command::Backup {
            repo,
            mut paths,
            files_from,
            files_from_verbatim,
            files_from_raw,
            client_id_file,
            parity,
            exclude,
            exclude_file,
            iexclude,
            iexclude_file,
            exclude_if_present,
            exclude_caches,
            exclude_larger_than,
            one_file_system,
            trust_mtime,
            reread_every,
        } => {
            if usize::from(parity) > kist_core::MAX_PARITY_SHARDS {
                anyhow::bail!("--parity must be 0..={}", kist_core::MAX_PARITY_SHARDS);
            }
            stdin_at_most_once(
                files_from
                    .iter()
                    .chain(&files_from_verbatim)
                    .chain(&files_from_raw)
                    .chain(&exclude_file)
                    .chain(&iexclude_file),
            )?;
            paths.extend(listed_paths(&files_from, Listing::Plain)?);
            paths.extend(listed_paths(&files_from_verbatim, Listing::Verbatim)?);
            paths.extend(listed_paths(&files_from_raw, Listing::Raw)?);
            if paths.is_empty() {
                bail!("no source paths to back up (the --files-from lists are empty)");
            }
            let exclude = Exclusions::new(
                pattern_filter(
                    ("--exclude", &exclude, &exclude_file),
                    ("--iexclude", &iexclude, &iexclude_file),
                    Role::Exclude,
                )?,
                &exclude_if_present,
                exclude_caches,
                exclude_larger_than,
                one_file_system,
            )?;
            // 遠端來源：第一個路徑是 `sftp://` 或 `s3://` URL → 整個 backup
            // 的來源就是那個 URL（單一 root；client 讀遠端 → 切塊 → 加密 → 上傳）。
            let source = SourceSpec::for_paths(&paths)?;
            let r = open_repo(&repo).await?;
            // 先鎖再載入：同 jobs.rs——首建並發各自 mint 不同 id 的視窗。
            let _lock = client_id::lock(client_id_file.as_deref())?;
            let client_id = client_id::load_or_create(client_id_file.as_deref())?;
            let mut opts = BackupOptions::new(
                kist_format::v4::ids::ClientId::from_bytes(client_id),
                client_id::hostname(),
                client_id::username(),
            );
            opts.parity = parity;
            opts.source = source;
            opts.exclude = exclude;
            opts.sftp_trust_mtime =
                trust_mtime.then(|| reread_every.unwrap_or(kist_core::v4::DEFAULT_REREAD_EVERY));
            let summary = r.backup(&paths, opts).await?;
            let s = summary.stats;
            let rep = summary.report;
            if json {
                print_json(&summary)?;
            } else {
                println!("snapshot {}", short_snapshot_id(&summary.snapshot));
                println!(
                    "  {} files, {} dirs, {} symlinks, {} total",
                    s.files,
                    s.dirs,
                    s.symlinks,
                    human_bytes(s.bytes)
                );
                println!(
                    "  new: {} in {} chunks, {} packs and {} metadata packs written",
                    human_bytes(rep.bytes_stored),
                    rep.chunks_new,
                    rep.packs_new,
                    rep.mpacks_new
                );
                if rep.excluded > 0 {
                    println!(
                        "  excluded: {} item(s) matching the exclude patterns",
                        rep.excluded
                    );
                }
                if let Some(t) = &summary.mtime_trust {
                    match t.reason {
                        None => println!(
                            "  trusted sizes and modification times: {} file(s) not read \
                             (last full read {} UTC)",
                            rep.files_reused,
                            t.last_full_read.map(display_time_ns).unwrap_or_default()
                        ),
                        Some(why) => println!("  read every file: {why}"),
                    }
                }
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
            let (snaps, errors) = r.list_snapshots_with_roots().await?;
            for e in &errors {
                eprintln!("warning: cannot read snapshot {e}");
            }
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
                let (client, ts) = snapshot_parts(&s.key);
                let paths: Vec<String> = s
                    .roots
                    .iter()
                    .map(|r| String::from_utf8_lossy(&r.path).into_owned())
                    .collect();
                println!(
                    "{:<8} {:<25} {:<19} {:<12} {:>8} {:>10}  {}",
                    &client[..8],
                    ts,
                    display_time_ns(s.snapshot.time),
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
            if !wait_for_shutdown_signal().await {
                anyhow::bail!("installing the Ctrl-C and SIGTERM handlers failed");
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
            include,
            include_file,
            iinclude,
            iinclude_file,
            exclude,
            exclude_file,
            iexclude,
            iexclude_file,
            dry_run,
        } => {
            // `SNAPSHOT:PATH`（同 restic）：snapshot 的寫法裡沒有 `:`。
            let (snapshot, subtree) = match snapshot.split_once(':') {
                Some((snapshot, path)) => (
                    snapshot.to_owned(),
                    kist_core::restore::subtree_components(path)?,
                ),
                None => (snapshot, Vec::new()),
            };
            stdin_at_most_once(
                include_file
                    .iter()
                    .chain(&iinclude_file)
                    .chain(&exclude_file)
                    .chain(&iexclude_file),
            )?;
            let opts = RestoreOptions {
                include: pattern_filter(
                    ("--include", &include, &include_file),
                    ("--iinclude", &iinclude, &iinclude_file),
                    Role::Include,
                )?,
                exclude: pattern_filter(
                    ("--exclude", &exclude, &exclude_file),
                    ("--iexclude", &iexclude, &iexclude_file),
                    Role::Exclude,
                )?,
                subtree,
            };
            let r = open_repo(&repo).await?;
            let key = r.resolve_snapshot(&snapshot).await?;
            let summary = if dry_run {
                let mut list = |path: &std::path::Path, kind: Planned| {
                    if !json {
                        let kind = match kind {
                            Planned::Dir => "dir ",
                            Planned::File => "file",
                            Planned::Symlink => "link",
                        };
                        println!("{kind} {}", path.display());
                    }
                };
                r.plan_restore(&key, &target, opts, &mut list).await?
            } else {
                r.restore(&key, &target, opts).await?
            };
            if json {
                print_json(&RestoreJson {
                    dry_run,
                    summary: &summary,
                })?;
            } else {
                println!(
                    "{} {} to {}: {} files, {} dirs, {} symlinks",
                    if dry_run { "would restore" } else { "restored" },
                    short_snapshot_id(&key.path()),
                    target.display(),
                    summary.files,
                    summary.dirs,
                    summary.symlinks
                );
            }
            let mut problems = Vec::new();
            if !summary.errors.is_empty() {
                for e in &summary.errors {
                    eprintln!("error: {e}");
                }
                problems.push(format!(
                    "{} item(s) could not be restored",
                    summary.errors.len()
                ));
            }
            if !summary.unmatched_includes.is_empty() {
                // 樣式可能來自 --include、--iinclude 或樣式檔，訊息不點名旗標。有錯誤時，失敗的
                // 目錄底下沒有走到，比對也許在那裡（審查修正）。
                let why = if summary.errors.is_empty() {
                    "no match in this snapshot, or every match is excluded or negated"
                } else {
                    "no match among the items reached; the errors above may hide matches"
                };
                for p in &summary.unmatched_includes {
                    eprintln!("warning: include pattern {p:?} selected nothing: {why}");
                }
                problems.push(format!(
                    "{} include pattern(s) selected nothing",
                    summary.unmatched_includes.len()
                ));
            }
            if !problems.is_empty() {
                return Err(Incomplete(problems.join("; ")).into());
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
            retire,
            prune,
            prune_args,
        } => {
            let r = open_repo(&repo).await?;
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
                    snapshots,
                    policy,
                    retire,
                    dry_run,
                })
                .await?;
            for u in &summary.unreadable {
                eprintln!(
                    "warning: {u}: its roots cannot be read, so the policy leaves it alone; \
                     forget it by its full key"
                );
            }
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
                let prune_result = r.prune(prune_args.options()).await;
                if json {
                    // stdout 只能有一個 JSON 值：forget 與 prune 包在一起。
                    // prune 失敗時 snapshot 已經刪了，仍要把 forget 的結果印出來
                    //（prune 為 null、結束碼 1、錯誤在 stderr），消費者才知道刪了哪些。
                    print_json(&serde_json::json!({
                        "forget": ForgetJson::new(dry_run, &summary),
                        "prune": prune_result.as_ref().ok(),
                    }))?;
                }
                let report = prune_result?;
                if !json {
                    output_prune(&report);
                }
                prune_warnings_and_exit(&report)
            } else {
                if !json && !dry_run && !summary.removed.is_empty() {
                    println!("run `kist prune` to reclaim the space");
                }
                Ok(())
            }
        }
        Command::Prune { repo, prune } => {
            let r = open_repo(&repo).await?;
            let report = r.prune(prune.options()).await?;
            if json {
                print_json(&report)?;
            } else {
                output_prune(&report);
            }
            prune_warnings_and_exit(&report)
        }
        Command::Rebuild { repo } => {
            let r = open_repo(&repo).await?;
            let report = r.rebuild(MaintOptions::default()).await?;
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            if json {
                print_json(&report)?;
            } else {
                match report.generation {
                    None => println!(
                        "the map of generation {} is intact; nothing to rebuild",
                        report.lost
                    ),
                    Some(g) => println!(
                        "rebuilt the map as generation {g} from generation {} ({}): {} records, \
                         {} held until the hold passes",
                        report.readable,
                        if report.kept_map {
                            "kept its map"
                        } else {
                            "every pack held as condemned"
                        },
                        report.records,
                        report.held
                    ),
                }
            }
            Ok(())
        }
        Command::Settings {
            repo,
            grace,
            replicas,
            pack_target,
            mpack_target,
        } => {
            let r = open_repo(&repo).await?;
            if grace.is_none()
                && replicas.is_none()
                && pack_target.is_none()
                && mpack_target.is_none()
            {
                let v = r.settings().await?;
                if json {
                    print_json(&v)?;
                } else {
                    println!("generation {}", v.generation);
                    print_settings(&v.settings);
                    if let Some(d) = v.drain {
                        println!(
                            "drain: lowered at generation {} from {}; new marks age by the old \
                             grace until it ends",
                            d.from,
                            human_duration(d.old_grace)
                        );
                    }
                    if let Some(h) = v.hold {
                        println!(
                            "hold: after a rebuild that lost generations {}..={}; no deletion \
                             until it passes",
                            h.from, h.lost
                        );
                    }
                }
                return Ok(());
            }
            let report = r
                .change_settings(
                    SettingsChange {
                        grace: grace.map(|d| d.as_secs()),
                        replicas,
                        pack_target,
                        mpack_target,
                    },
                    MaintOptions::default(),
                )
                .await?;
            if json {
                print_json(&report)?;
            } else {
                match report.generation {
                    None => println!("no change"),
                    Some(g) => println!("settings written as generation {g}"),
                }
                print_settings(&report.settings);
                if let Some(d) = report.drain {
                    println!(
                        "drain: the grace was lowered from {}; marks made until it ends still age \
                         by the old grace",
                        human_duration(d.old_grace)
                    );
                }
            }
            Ok(())
        }
        Command::Key { repo, action } => {
            let r = open_repo(&repo).await?;
            match action {
                KeyAction::Add { new_password_file } => {
                    let new = password::obtain_new(&new_password_file)?;
                    let name = r.key_add(new.as_bytes()).await?;
                    if json {
                        print_json(&serde_json::json!({ "added": name }))?;
                    } else {
                        println!("added key slot {name}");
                    }
                }
                KeyAction::List => {
                    let slots = r.key_list().await?;
                    if json {
                        print_json(&slots)?;
                    } else {
                        for s in slots {
                            println!("{} {}", if s.current { "*" } else { " " }, s.name);
                        }
                    }
                }
                KeyAction::Remove { slot } => {
                    r.key_remove(&slot).await?;
                    if json {
                        print_json(&serde_json::json!({ "removed": slot }))?;
                    } else {
                        println!("removed key slot {slot}");
                    }
                }
            }
            Ok(())
        }
        Command::RemoveUnknownPacks { repo } => {
            let r = open_repo(&repo).await?;
            let report = r.condemn_unknown(MaintOptions::default()).await?;
            if json {
                print_json(&report)?;
            } else if report.packs.is_empty() {
                println!("no pack with an unknown version");
            } else {
                for p in &report.packs {
                    println!("marked for deletion: {p}");
                }
                println!(
                    "{} pack(s) marked in generation {}; prune deletes them once a grace has passed",
                    report.packs.len(),
                    report.generation.unwrap_or_default()
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
            if json {
                print_json(&report)?;
            } else {
                // 人類可讀的輸出只在非 --json 模式印：--json 的 stdout 必須整份是 JSON（README 的契約）。
                for k in &report.repaired {
                    println!("created {k}");
                }
                for k in &report.repairable {
                    println!("repairable (not rewritten): {k}");
                }
                println!(
                    "checked generation {}: {} snapshots, {} metadata items, {} chunks, {} packs{}",
                    report.generation,
                    report.snapshots,
                    report.items,
                    report.chunks,
                    report.packs,
                    if read_data || repair {
                        " (data read)"
                    } else {
                        ""
                    }
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

fn output_prune(p: &PruneReport) {
    for w in &p.warnings {
        eprintln!("warning: {w}");
    }
    println!(
        "generation {} written (from {}): {} snapshots, {} metadata items, {} chunks",
        p.generation, p.base, p.snapshots, p.items, p.chunks
    );
    println!(
        "{} new pack(s) recorded; {} marked, {} revived, {} to delete",
        p.newcomers, p.condemned, p.revived, p.tombstoned
    );
    if p.read_packs > 0 {
        println!(
            "read {} data pack(s) in full and verified every chunk",
            p.read_packs
        );
    }
    if p.repacked > 0 || p.repaired > 0 {
        println!(
            "repacked {} pack(s) ({} repaired from parity first)",
            p.repacked, p.repaired
        );
    }
    println!(
        "deleted {} pack(s) ({}), {} delta(s), {} map shard(s), {} parity object(s), {} snapshot(s)",
        p.deleted_packs,
        human_bytes(p.deleted_bytes),
        p.deleted_deltas,
        p.deleted_shards,
        p.deleted_parity,
        p.deleted_snapshots
    );
}

/// 刪不掉的物件、沒做完的刪除走 stderr + 結束碼 3，兩種輸出模式都一樣。
fn prune_warnings_and_exit(p: &PruneReport) -> Result<()> {
    for s in &p.undeleted {
        eprintln!("warning: cannot delete {s}");
    }
    if let Some(u) = &p.unfinished {
        eprintln!("warning: {u}");
    }
    if !p.undeleted.is_empty() || p.unfinished.is_some() {
        return Err(Incomplete(format!(
            "{} object(s) could not be deleted{}; they are retried by the next prune",
            p.undeleted.len(),
            if p.unfinished.is_some() {
                " and the deletions did not finish"
            } else {
                ""
            }
        ))
        .into());
    }
    Ok(())
}

fn print_settings(s: &kist_format::v4::index::Settings) {
    println!("grace         {}", human_duration(s.grace));
    println!("replicas      {}", s.replicas);
    println!("pack_target   {}", human_bytes(s.pack_target));
    println!("mpack_target  {}", human_bytes(s.mpack_target));
}

/// 秒 → `72h`、`90m`、`45s` 這種寫法（能整除的最大單位）。
fn human_duration(secs: u64) -> String {
    match secs {
        s if s > 0 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s > 0 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s > 0 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
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

fn spawn_shutdown_watcher(tx: tokio::sync::watch::Sender<bool>) {
    tokio::spawn(async move {
        if wait_for_shutdown_signal().await {
            eprintln!("shutting down after the current job");
            let _ = tx.send(true);
        }
    });
}

/// `run`／`serve`／`mount` 的結束訊號：Ctrl-C（SIGINT）**或** SIGTERM。systemd／
/// docker 的 `stop` 送的是 SIGTERM——只等 SIGINT 的話，「跑完手邊工作再停」
/// 的約定在主要的部署訊號下不會發生。回 false 表示兩個 handler 都裝不起來。
async fn wait_for_shutdown_signal() -> bool {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                // ctrl_c 註冊失敗不算結束訊號：退回只等 SIGTERM，不憑空觸發。
                c = tokio::signal::ctrl_c() => match c {
                    Ok(()) => true,
                    Err(_) => term.recv().await.is_some(),
                },
                _ = term.recv() => true,
            },
            Err(_) => tokio::signal::ctrl_c().await.is_ok(),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.is_ok()
    }
}

fn print_next_runs(daemon: &kist_app::Daemon) {
    for (job, next) in daemon.next_runs(time::OffsetDateTime::now_utc()) {
        eprintln!(
            "{}: next run {}",
            job.name(),
            next.map(|t| t.to_string())
                .unwrap_or_else(|| "never".to_owned())
        );
    }
}

/// `snapshots --json` 的輸出形狀：key 拆出 client 與 timestamp，路徑從 bytes 轉成 UTF-8（非法位元組以
/// U+FFFD 取代）。
#[derive(serde::Serialize)]
struct SnapshotJson {
    key: String,
    client: String,
    timestamp: String,
    /// RFC 3339（backup 開始時間）。
    time: String,
    hostname: String,
    username: String,
    /// roots 的定位（lossy UTF-8）。
    paths: Vec<String>,
    roots: Vec<RootJson>,
    parent: Option<String>,
    stats: kist_format::v4::snapshot::SnapshotStats,
}

#[derive(serde::Serialize)]
struct RootJson {
    path: String,
    /// "file" | "dir" | "symlink"
    r#type: &'static str,
}

impl SnapshotJson {
    fn new(s: &kist_core::v4::SnapshotListing) -> Self {
        let (client, timestamp) = snapshot_parts(&s.key);
        Self {
            key: s.key.path(),
            client,
            timestamp,
            time: rfc3339_ns(s.snapshot.time),
            hostname: s.snapshot.host.clone(),
            username: s.snapshot.user.clone(),
            paths: s
                .roots
                .iter()
                .map(|r| String::from_utf8_lossy(&r.path).into_owned())
                .collect(),
            roots: s
                .roots
                .iter()
                .map(|r| RootJson {
                    path: String::from_utf8_lossy(&r.path).into_owned(),
                    r#type: match r.t {
                        kist_format::v4::meta::node_type::DIR => "dir",
                        kist_format::v4::meta::node_type::SYMLINK => "symlink",
                        _ => "file",
                    },
                })
                .collect(),
            parent: s.snapshot.parent.clone(),
            stats: s.snapshot.stats,
        }
    }
}

/// snapshot 的 key → (client 的 32 位 hex, 時間戳)。
fn snapshot_parts(key: &kist_format::v4::keys::Key) -> (String, String) {
    match key {
        kist_format::v4::keys::Key::Snapshot { client, ts, .. } => {
            (client.to_string(), ts.as_str().to_owned())
        }
        other => (other.path(), String::new()),
    }
}

/// `forget --json`：dry_run 標示 removed 是「會刪」還是「已刪」；內部的
/// (key, reasons) tuple 包成有名字的欄位，位置語意太脆弱。
#[derive(serde::Serialize)]
struct ForgetJson {
    dry_run: bool,
    removed: Vec<String>,
    kept: Vec<ForgetKept>,
    unreadable: Vec<String>,
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
            unreadable: s.unreadable.clone(),
        }
    }
}

/// `restore --json`：加上 dry_run（`--dry-run` 時計數是「會還原」的，什麼都沒寫）。
#[derive(serde::Serialize)]
struct RestoreJson<'a> {
    dry_run: bool,
    #[serde(flatten)]
    summary: &'a RestoreSummary,
}

fn repo_url(args: &RepoArgs) -> Result<&str> {
    args.repo
        .as_deref()
        .context("no repository given: use --repo <path|s3://bucket/prefix|sftp://host/path|rclone://remote/path> or set KIST_REPO")
}

async fn open_backend(args: &RepoArgs) -> Result<Backend> {
    Ok(Backend::from_url(repo_url(args)?).await?)
}

async fn open_repo(args: &RepoArgs) -> Result<Repo> {
    let backend = open_backend(args).await?;
    let password = password::obtain(&args.password_file, false)?;
    let cache_root = if args.no_cache {
        None
    } else {
        let root = args
            .cache_dir
            .clone()
            .or_else(|| dirs::cache_dir().map(|d| d.join("kist")));
        if root.is_none() {
            tracing::warn!(
                "cannot determine a cache directory; running without the local index cache"
            );
        }
        root
    };
    Ok(Repo::open(backend, password.as_bytes(), cache_root.as_deref()).await?)
}

/// `snapshots/<client>/<ts>` → `<client 前 8 碼>/<ts>`，給人看的簡短 id。
fn short_snapshot_id(key: &str) -> String {
    let mut parts = key.rsplit('/');
    let ts = parts.next().unwrap_or(key);
    let client = parts.next().unwrap_or("");
    format!("{}/{ts}", &client[..8.min(client.len())])
}

/// snapshot 的 i64 奈秒 → RFC 3339。
fn rfc3339_ns(ns: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns))
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// snapshot 的 i64 奈秒 → 列表用的時間字串（顯示到秒）。
fn display_time_ns(ns: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns))
        .map(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
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

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod shutdown_tests {
    /// systemd `stop`、docker `stop` 都送 SIGTERM：`run`/`serve` 的「跑完手邊
    /// 工作再停」約定必須對 SIGTERM 也成立（只有 SIGINT 的話，主要的部署
    /// 訊號會直接殺掉進行中的工作）。
    #[tokio::test]
    async fn sigterm_triggers_the_shutdown_channel() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        super::spawn_shutdown_watcher(tx);
        // 讓 watcher task 註冊好 signal handler
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let ok = std::process::Command::new("kill")
            .args(["-s", "TERM", &std::process::id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok, "kill -TERM 指令本身失敗了");
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed())
            .await
            .expect("SIGTERM 應觸發 shutdown 通道（graceful shutdown）")
            .unwrap();
        assert!(*rx.borrow(), "通道要被設成 true");
    }
}
