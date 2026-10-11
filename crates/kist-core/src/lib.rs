//! kist 的核心流程：init / backup / restore / check / snapshots / forget / prune。
//!
//! 這個 crate 把 `kist-format`（bytes 長什麼樣）、`kist-crypto`（怎麼加解密）、
//! `kist-chunker`（怎麼切）、`kist-backend`（放哪裡）串起來，實作真正的備份邏輯。
//! CLI 只是薄薄一層參數解析，所有行為都在這裡，也都在這裡的整合測試裡驗證。
//!
//! 設計要點：
//! - 所有 I/O 是 async（tokio）；chunk / hash / 壓縮 / 加密等 CPU 密集工作一律丟進
//!   `spawn_blocking`，不在 async task 裡做重計算。
//! - backup 的寫入順序固定：packs → trees → index → snapshot。snapshot 是唯一的 commit point。
//! - 記憶體：檔案以串流切塊，在飛的 pack 最多 2 個（各 ≤ 64 MiB）；
//!   index 有本地快取（排序表、按需讀取）時不整份載入記憶體。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod backup;
pub mod cache;
pub mod check;
mod dirhandle;
pub mod filter;
pub mod forget;
pub mod fsmeta;
pub mod index;
pub mod pack;
pub mod prune;
pub mod reach;
pub mod rebuild;
pub mod repo;
pub mod restore;
pub mod snapshots;
pub mod v4;

pub use backup::{
    BackupOptions, BackupProgress, BackupSummary, Exclusions, Marker, PreparedBackup,
    ProgressCallback, SourceSpec, DEFAULT_GC_GRACE,
};
pub use check::{CheckOptions, CheckReport};
pub use filter::{PathFilter, PatternError};
pub use forget::{ForgetOptions, ForgetSummary, RetentionPolicy};
pub use index::{ChunkIndex, ChunkLocation};
/// parity 份數的上限。CLI 與 kist-app 的設定檢查都用它，不各寫字面值
/// （kist-app 不直接依賴 kist-format，所以從這裡轉出去）。
pub use kist_format::parity::MAX_PARITY_SHARDS;
pub use prune::{PruneOptions, PrunePlan, PruneReport, MAX_EFFECTIVE_BLOBS};
pub use rebuild::RebuildSummary;
pub use repo::{InitOptions, Repository};
pub use restore::{Planned, ReloadableIndex, RestoreOptions, RestoreSummary};
pub use snapshots::SnapshotInfo;

use std::path::PathBuf;

use kist_format::ChunkId;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("not a kist repository (no config object found)")]
    NotARepository,
    #[error("a kist repository already exists at this location")]
    RepoExists,
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),
    #[error("snapshot {0} is ambiguous: matches {1} snapshots")]
    AmbiguousSnapshot(String, usize),
    #[error("chunk {0} is referenced but missing from the index")]
    ChunkMissing(ChunkId),
    /// backup 引用到的 pack 在 commit 前消失（或它的 GC 標記已超過 grace）：不寫 snapshot。
    #[error("pack {pack} referenced by this backup is gone or about to be deleted{}; rerun the backup", chunk.map(|c| format!(" (chunk {c})")).unwrap_or_default())]
    PackMissing {
        pack: kist_format::ObjectId,
        chunk: Option<ChunkId>,
    },
    #[error("object {key} is corrupt: {reason}")]
    Corrupt { key: String, reason: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// restore 的目標處（本機檔案系統）擋著東西：symlink、非目錄、目錄。
    /// 不是 repo 損壞，移開它再還原即可（ADR 019 A30；以前報成 Corrupt）。
    #[error("{path}: {reason}")]
    RestoreBlocked { path: PathBuf, reason: String },
    #[error("unsupported file name {0:?}: not valid Unicode on this platform")]
    BadFileName(PathBuf),
    #[error("background task failed: {0}")]
    Join(String),
    #[error("invalid repository configuration: {0}")]
    InvalidConfig(String),
    /// 使用者的要求本身不成立（例如 forget 沒給任何條件）。
    #[error("{0}")]
    Usage(String),
    /// prune 拒絕執行：引用不完整時分不出什麼是垃圾。
    #[error("refusing to prune: {0}")]
    Unsafe(String),
    /// backup 跑得比 GC 的 grace 還久：它寫過的東西可能已經被標記甚至刪掉，不寫 snapshot。
    #[error("backup took {elapsed_secs}s, longer than the GC grace period ({grace_secs}s); rerun it (uploaded data is reused) or raise --gc-grace and prune --grace (config file: [prune] grace, and [backup] gc_grace if set)")]
    BackupTooLong { elapsed_secs: i64, grace_secs: u64 },
    /// backup 寫過的 tree 在寫之前就被標記且標記已超過 grace：可能隨時被刪，不寫 snapshot。
    #[error("tree {0} written by this backup is about to be garbage-collected; rerun the backup")]
    TreeMarked(kist_format::ObjectId),
    #[error(transparent)]
    Backend(#[from] kist_backend::BackendError),
    #[error(transparent)]
    Crypto(#[from] kist_crypto::CryptoError),
    #[error(transparent)]
    Format(#[from] kist_format::FormatError),
    #[error(transparent)]
    Chunker(#[from] kist_chunker::ChunkerError),
    #[error(transparent)]
    FormatV4(#[from] kist_format::v4::Error),
    /// v4 的 B1、B4：最大世代的 head 用不了（讀不了、版本不認得、沒有 `.r1`、世代變小）。
    #[error("{0}")]
    Generation(String),
    /// v4 的 B4、B5：沿用或時間的條件不成立，不寫 snapshot。
    #[error("not committing this backup: {0}; rerun it")]
    NotCommitted(String),
    /// v4 的 B5：寫 snapshot 主體失敗，commit 與否不明（逾時的 PUT 可能已經寫進去，§12）。
    #[error(
        "writing snapshot {key} failed, so whether this backup was committed is unknown: {source}"
    )]
    CommitUnknown {
        key: String,
        #[source]
        source: kist_backend::BackendError,
    },
    /// v4 的讀取（§10）：一項沒有持有者，或每一份都驗證失敗。restore、mount 與 check 在這時重新載入一次世代
    /// 再試（[`v4::live`]）。
    #[error("no usable copy of {what}: {reasons}")]
    Unavailable { what: String, reasons: String },
    /// v4 的維護命令：輸掉柵欄（下一代已經有人寫了，§11.3 P8），或拒絕之前重新列出世代時發現已有更新的
    /// head（P3、P5）。什麼都沒刪；重跑就以新的世代為基礎。
    #[error("{0}; nothing was deleted, rerun to start from the newer generation")]
    Superseded(String),
}

impl CoreError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// repo 內容損壞：`key` 是出事的物件（或 `<chunk list>` 這類標記）。
    pub(crate) fn corrupt(key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Corrupt {
            key: key.into(),
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// tree 走訪（restore / check / prune）的最大巢狀深度。
///
/// 這是**實作上限**，不是格式規則。走訪是遞迴，每一層吃一份 stack frame：
/// 上限必須保證「頂到上限之前，任何 build profile（debug 測試、release 的
/// 2 MiB tokio worker）都還在 stack 預算內」——實測 debug 下 4 200 層就會
/// stack overflow（`tests/restore_hardening.rs` 釘死這個行為）。誠實來源
/// 其實可以合法超過它——Linux 上 PATH_MAX 內疊得出約 2 000 層——所以
/// backup 寫入端套同一把尺：過深的來源子目錄跳過並記警告，寫出的樹因此
/// 永不超限；超過上限的 chain 只能出自腐壞或敵意 repo：乾淨回錯，不是
/// process 陣亡。數值見 docs/format.md §8.4。
pub const MAX_TREE_DEPTH: usize = 256;

/// 把 CPU 密集工作丟到 blocking thread pool。
pub(crate) async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CoreError::Join(e.to_string()))?
}
