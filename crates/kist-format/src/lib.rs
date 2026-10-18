//! kist 的 on-disk 格式：所有結構定義、CBOR 序列化與位元組排版。
//! 其他 crate 只能透過這裡讀寫 repo 內容。
//!
//! 這個 crate 只描述「bytes 長什麼樣」，**不持有任何金鑰**：加密／解密由
//! `kist-crypto` 負責。完整規格見 `docs/format-v4.md`；任何改動都必須同步
//! 更新該文件與 golden files（`tests/golden-v4/`）。
//!
//! 模組一覽：
//! - [`v4`]：現行的格式（v3 已移除，見 ADR 026 實作第七段）。
//! - [`cbor`]：所有 metadata 共用的規範 CBOR 編解碼（依規格欄位表順序，不排序 keys）。
//! - [`ids`]：`ChunkId`（keyed hash）與 `ObjectId`（寫進 repo 的 bytes 的 hash）；其他識別碼在 [`v4::ids`]。
//! - [`chunker`]：FastCDC 的參數（`kist-chunker` 吃它）。
//! - [`parity`]：Reed-Solomon 同位的排版與修復（版本由 [`v4::parity`] 給）。
//!
//! sealed 物件沒有 envelope header：形式就是 `nonce(24) ‖ ciphertext ‖ tag(16)`，AAD 依角色由
//! [`v4`] 的 `AAD_*` 常數或物件自己的 ID / key 路徑構成，見 `docs/format-v4.md` §5。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod cbor;
pub mod chunker;
pub mod ids;
pub mod parity;
pub mod v4;

pub use ids::{ChunkId, ObjectId};

/// 明文壓縮演算法：以 1 byte 出現在每一項 sealed 內容的明文開頭（`docs/format-v4.md` §6）。
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

/// 格式層的錯誤：只跟 bytes 的排版與編解碼有關，不含 I/O 或加密錯誤。v4 結構的讀取端驗證有自己的
/// [`v4::Error`]（CBOR 的編解碼錯誤仍是這一個）。
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("CBOR encode failed: {0}")]
    Encode(String),
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    #[error("unknown compression algorithm {0}")]
    UnknownAlgorithm(u8),
    #[error("invalid object name: {0}")]
    BadName(String),
    #[error("invalid repository parameters: {0}")]
    InvalidParams(String),
    #[error("parity object is corrupt: {0}")]
    ParityCorrupt(String),
    #[error("pack cannot be repaired: {0}")]
    Unrepairable(String),
}

pub type Result<T> = std::result::Result<T, FormatError>;
