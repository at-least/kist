//! kist 的 on-disk 格式（v3）：所有結構定義、CBOR 序列化與位元組排版。
//! 其他 crate 只能透過這裡讀寫 repo 內容。
//!
//! 這個 crate 只描述「bytes 長什麼樣」，**不持有任何金鑰**：加密／解密由
//! `kist-crypto` 負責。完整規格見 `docs/format-v3-draft.md`（v3 草案；
//! 定案後取代 `docs/format.md`，與 Go 實作共用同一份）；任何改動都必須
//! 同步更新該文件、golden files，**以及 Go 實作**。
//!
//! 模組一覽：
//! - [`ids`]：`ChunkId` / `TreeId`（keyed hash）與 `ObjectId`（pack/index 的密文 hash）。
//! - [`cbor`]：所有 metadata 共用的規範 CBOR 編解碼（map keys 排序）。
//! - [`pack`]：pack 檔的位元組排版與 trailer 結構。
//! - [`config`]、[`tree`]、[`snapshot`]、[`index`]：各種明文結構。
//! - [`keys`]：repo 內物件的 key（路徑）命名規則。
//!
//! sealed 物件沒有 envelope header（v1 的舊設計已淘汰）：形式就是
//! `nonce(24) ‖ ciphertext ‖ tag(16)`，AAD 依角色由 [`AAD_*`] 常數或
//! 物件自己的 ID / key 路徑構成，見 `docs/format.md` §5。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod cbor;
pub mod config;
pub mod ids;
pub mod index;
pub mod keys;
pub mod pack;
pub mod parity;
pub mod snapshot;
pub mod tree;

pub use ids::{ChunkId, ObjectId, TreeId};

/// 目前的格式版本。所有結構的 `v` 欄位與 pack magic 版號在 v3 都是 3。
pub const FORMAT_VERSION: u32 = 3;

/// pack trailer 密封時的 AAD（角色常數）。
pub const AAD_PACK_TRAILER: &[u8] = b"kist/v3/pack-trailer";
/// index blob 密封時的 AAD（角色常數）。
pub const AAD_INDEX: &[u8] = b"kist/v3/index";
/// master key 封裝的 AAD（v3 起是常數）：不變式在**認證密文裡**
/// （`master(32) ‖ Invariants CBOR`），不靠 AAD 排版綁欄位——未來加
/// 不變式欄位不需要動這裡（v2 的列舉式 AAD 每加一欄就要改排版）。
pub const AAD_MASTER: &[u8] = b"kist/v3/master";

/// 明文壓縮演算法：以 1 byte 出現在 chunk 與 index blob 的明文開頭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Algorithm {
    Raw = 0,
    Zstd = 1,
}

impl Algorithm {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Zstd),
            other => Err(FormatError::UnknownAlgorithm(other)),
        }
    }
}

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
    #[error("unknown compression algorithm {0}")]
    UnknownAlgorithm(u8),
    #[error("invalid object name: {0}")]
    BadName(String),
    #[error("invalid timestamp: {0}")]
    BadTimestamp(String),
    #[error("invalid repository parameters: {0}")]
    InvalidParams(String),
    #[error("invalid tree entry: {0}")]
    InvalidEntry(String),
    #[error("invalid snapshot: {0}")]
    InvalidSnapshot(String),
    #[error("parity object is corrupt: {0}")]
    ParityCorrupt(String),
    #[error("pack cannot be repaired: {0}")]
    Unrepairable(String),
}

pub type Result<T> = std::result::Result<T, FormatError>;
