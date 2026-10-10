//! v4 格式（`docs/format-v4.md`）。
//!
//! 與 v3 的模組並存：kist-core 等使用者在實作第 3–6 段遷移到這裡，之後 v3 的模組移除。和 v3 一樣只描述
//! bytes 長什麼樣、不持有任何金鑰；需要金鑰的判斷（切段用的 `K_cut`）以函式參數傳入。章節號碼（§n）指
//! `docs/format-v4.md`。
//!
//! 模組一覽：
//! - [`ids`]：`MetaId`、`ClientId`（`ChunkId`、`ObjectId` 與 v3 相同，從 crate 根重新匯出）。
//! - [`keys`]：§1 的命名空間：每種物件的 key、`.r1`、解析與驗證。
//! - [`config`]：§3 的 config 與金鑰槽（金鑰推導在 `kist-crypto` 的 `v4`）。
//! - [`pack`]：§6 的 pack 與 mpack 排版、trailer 與它的一致性檢查。
//! - [`parity`]：§13 的 parity（v3 的排版，`v` 是 4）。
//! - [`meta`]：§7、§8 的 metadata item 與 Entry、切段規則、建構與讀取端驗證。
//! - [`snapshot`]：§9 的 snapshot、Root、定位的切段與 restore 的落點、roots 的清單。
//! - [`index`]：§10 的世代 head、map 分片與記錄（含持有者的排序）、delta。

pub mod config;
pub mod ids;
pub mod index;
pub mod keys;
pub mod meta;
pub mod pack;
pub mod parity;
pub mod snapshot;

use crate::FormatError;

/// 這一代格式的版本：config、每個結構的 `v` 與 pack magic 一開始都是 4（§14）。
pub const VERSION: u32 = 4;

/// pack 與 mpack 的 trailer 密封時的 AAD（§5）。
pub const AAD_PACK_TRAILER: &[u8] = b"kist/v4/pack-trailer";
/// delta 的 AAD 前綴，後面接 client id 的 16 bytes（§5）。
pub const AAD_DELTA: &[u8] = b"kist/v4/delta";
/// map 分片的 AAD（§5）；分片號碼在明文裡，必須等於 head 指向它的位置。
pub const AAD_SHARD: &[u8] = b"kist/v4/shard";
/// 金鑰槽的 AAD 前綴，後面接 BLAKE3(config 的 bytes)（§3）。
pub const AAD_SLOT: &[u8] = b"kist/v4/slot";

/// v4 的錯誤。CBOR 的編解碼錯誤沿用 crate 根的 [`FormatError`]。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Format(#[from] FormatError),
    /// §14：讀取端遇到不認得的版本就拒絕。
    #[error("unsupported {what} version {version}")]
    Version { what: &'static str, version: u32 },
    /// 不符合規格的內容（讀取端驗證失敗），一律大聲失敗。
    #[error("corrupt {what}: {detail}")]
    Corrupt { what: &'static str, detail: String },
    /// 寫入端要寫的內容不符合規格：拒絕寫出。
    #[error("refusing to write {what}: {detail}")]
    Invalid { what: &'static str, detail: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// §14：`v` 不是 4 就拒絕。
pub fn check_version(what: &'static str, version: u32) -> Result<()> {
    if version == VERSION {
        Ok(())
    } else {
        Err(Error::Version { what, version })
    }
}

/// 寫入端拒絕寫出。
pub(crate) fn invalid(what: &'static str, detail: impl Into<String>) -> Error {
    Error::Invalid {
        what,
        detail: detail.into(),
    }
}

/// 讀取端驗證失敗。
pub(crate) fn corrupt(what: &'static str, detail: impl Into<String>) -> Error {
    Error::Corrupt {
        what,
        detail: detail.into(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn only_version_4_is_accepted() {
        assert!(check_version("config", 4).is_ok());
        for v in [0, 3, 5, u32::MAX] {
            assert!(
                matches!(check_version("config", v), Err(Error::Version { version, .. }) if version == v)
            );
        }
    }
}
