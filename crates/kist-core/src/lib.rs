//! kist 的核心流程：init / backup / restore / check / snapshots。
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
pub mod fsmeta;
pub mod index;
pub mod pack;
pub mod rebuild;
pub mod repo;
pub mod restore;
pub mod snapshots;

pub use backup::{BackupOptions, BackupSummary};
pub use check::{CheckOptions, CheckReport};
pub use index::{ChunkIndex, ChunkLocation};
pub use rebuild::RebuildSummary;
pub use repo::{InitOptions, Repository};
pub use restore::{RestoreOptions, RestoreSummary};
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
    #[error("object {key} is corrupt: {reason}")]
    Corrupt { key: String, reason: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported file name {0:?}: not valid Unicode on this platform")]
    BadFileName(PathBuf),
    #[error("background task failed: {0}")]
    Join(String),
    #[error("invalid repository configuration: {0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Backend(#[from] kist_backend::BackendError),
    #[error(transparent)]
    Crypto(#[from] kist_crypto::CryptoError),
    #[error(transparent)]
    Format(#[from] kist_format::FormatError),
    #[error(transparent)]
    Chunker(#[from] kist_chunker::ChunkerError),
}

impl CoreError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

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
