//! kist 的 on-disk 格式：所有結構定義、CBOR 序列化與版本演進。
//! 其他 crate 只能透過這裡讀寫 repo 內容。
//!
//! 這個 crate 只描述「bytes 長什麼樣」，**不持有任何金鑰**：
//! 加密／解密由 `kist-crypto` 負責，這裡只提供明文結構與外層封裝的排版。
//! 完整規格見 `docs/format.md`；任何改動都必須同步更新該文件與 golden files。
//!
//! 模組一覽：
//! - [`ids`]：`ChunkId`（keyed hash）與 `ObjectId`（物件名稱 = 密文 hash）。
//! - [`cbor`]：所有 metadata 共用的 CBOR 編解碼。
//! - [`envelope`]：獨立物件（tree / index / snapshot / key slot / pack trailer）的外層封裝。
//! - [`pack`]：pack 檔的位元組排版與 trailer 結構。
//! - [`config`]、[`tree`]、[`snapshot`]、[`index`]：各種明文結構。
//! - [`keys`]：repo 內物件的 key（路徑）命名規則。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod cbor;
pub mod config;
pub mod envelope;
pub mod ids;
pub mod index;
pub mod keys;
pub mod pack;
pub mod snapshot;
pub mod tree;

pub use ids::{ChunkId, ObjectId};

/// 目前的格式版本。所有結構的 `version` 欄位在 v1 都是 1。
pub const FORMAT_VERSION: u32 = 1;

/// 格式層的錯誤：只跟 bytes 的排版與編解碼有關，不含 I/O 或加密錯誤。
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("CBOR encode failed: {0}")]
    Encode(String),
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    #[error("{what} is truncated: {actual} bytes, need at least {needed}")]
    Truncated {
        what: &'static str,
        needed: usize,
        actual: usize,
    },
    #[error("bad magic: not a kist {what}")]
    BadMagic { what: &'static str },
    #[error("unsupported {what} version {version}")]
    UnsupportedVersion { what: &'static str, version: u32 },
    #[error("unknown object kind {0}")]
    UnknownKind(u8),
    #[error("unknown compression {0}")]
    UnknownCompression(u8),
    #[error("invalid object name: {0}")]
    BadName(String),
    #[error("invalid timestamp: {0}")]
    BadTimestamp(String),
    #[error("invalid repository parameters: {0}")]
    InvalidParams(String),
}

pub type Result<T> = std::result::Result<T, FormatError>;
