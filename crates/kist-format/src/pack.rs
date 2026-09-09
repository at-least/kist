//! pack 檔的位元組排版（v2）。
//!
//! ```text
//! +-------------------+----------------------+------------------+--------------------+-------------------+
//! | magic 8B          | chunk entry × N      | trailer (sealed) | trailer len u64 BE | magic 8B          |
//! +-------------------+----------------------+------------------+--------------------+-------------------+
//! ```
//!
//! - magic ＝ `"kistpk"` ‖ 版號 u16 big-endian（8 bytes），檔頭檔尾各一份，
//!   且 trailer 的 `v` 必須一致（三處一致）。
//! - chunk entry ＝ `nonce(24) ‖ AEAD(chunk key, nonce, AAD=ChunkId, payload)`；
//!   payload 的第一個 byte 是壓縮演算法（0 原文 / 1 zstd），之後才是資料。
//!   entry 之間沒有分隔符，位置只記在 trailer。
//! - trailer 以 index key 密封（AAD = [`crate::AAD_PACK_TRAILER`]），
//!   明文是 CBOR 的 [`PackTrailer`]，不壓縮。
//! - 檔尾 16 bytes = trailer 長度（big-endian u64）+ magic：一次 range read
//!   就能定位 trailer，不用下載整個 pack。
//! - pack 名稱 = BLAKE3(整檔 bytes)，見 [`crate::ObjectId::of`]。

use serde::{Deserialize, Serialize};

use crate::{ChunkId, FormatError, Result, FORMAT_VERSION};

pub const MAGIC_PREFIX: &[u8; 6] = b"kistpk";
pub const HEADER_LEN: usize = 8;
/// 檔尾固定部分：u64 trailer 長度 + magic。
pub const FOOTER_LEN: usize = 8 + 8;
/// 每個 chunk entry 開頭的 nonce 長度。
pub const CHUNK_NONCE_LEN: usize = 24;
/// AEAD tag 長度。
pub const TAG_LEN: usize = 16;
/// nonce + tag 以外的最小密文內容：algorithm byte 至少 1 byte。
pub const MIN_ENTRY_LEN: usize = CHUNK_NONCE_LEN + TAG_LEN + 1;

/// 依目前格式版本組出 8-byte magic。
pub fn magic() -> [u8; HEADER_LEN] {
    let mut m = [0u8; HEADER_LEN];
    m[..MAGIC_PREFIX.len()].copy_from_slice(MAGIC_PREFIX);
    let v = (FORMAT_VERSION as u16).to_be_bytes();
    m[6..].copy_from_slice(&v);
    m
}

/// trailer 內容：這個 pack 裡有哪些 chunk、各在哪裡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackTrailer {
    #[serde(rename = "v")]
    pub version: u32,
    #[serde(rename = "entries")]
    pub entries: Vec<PackEntry>,
}

impl PackTrailer {
    pub fn new(entries: Vec<PackEntry>) -> Self {
        Self {
            version: FORMAT_VERSION,
            entries,
        }
    }
}

/// 一個 chunk 在 pack 裡的位置。index blob 的 pack 條目也用同一個結構。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    /// chunk 的身分（keyed hash of 明文）。
    #[serde(rename = "i")]
    pub id: ChunkId,
    /// entry 在 pack 檔裡的起點（含 nonce；從檔頭 magic 之後起算的絕對位置）。
    #[serde(rename = "o")]
    pub offset: u64,
    /// entry 的總長度：nonce + 密文 + tag。
    #[serde(rename = "l")]
    pub length: u64,
    /// 明文長度（解壓縮後）。mount/進度不用先解密就知道大小。
    #[serde(rename = "r")]
    pub raw_len: u64,
}

/// 開新 pack：回傳已含 magic 的 buffer。
pub fn begin() -> Vec<u8> {
    magic().to_vec()
}

/// 把 trailer（已密封的 bytes）接上去、補上檔尾，pack bytes 就完成了。
pub fn finish(mut pack: Vec<u8>, trailer_sealed: &[u8]) -> Vec<u8> {
    pack.extend_from_slice(trailer_sealed);
    pack.extend_from_slice(&(trailer_sealed.len() as u64).to_be_bytes());
    pack.extend_from_slice(&magic());
    pack
}

/// 從 pack 檔的最後 16 bytes 讀出 trailer 的長度（與檔尾 magic 驗證）。
pub fn parse_footer(footer: &[u8]) -> Result<u64> {
    if footer.len() < FOOTER_LEN {
        return Err(FormatError::Truncated {
            what: "pack footer",
            needed: FOOTER_LEN,
            actual: footer.len(),
        });
    }
    let footer = &footer[footer.len() - FOOTER_LEN..];
    let m = magic();
    if footer[8..] != m {
        return Err(FormatError::BadMagic {
            what: "pack footer",
        });
    }
    let mut len = [0u8; 8];
    len.copy_from_slice(&footer[..8]);
    Ok(u64::from_be_bytes(len))
}

/// 給定整個 pack 的 bytes，切出 trailer（已密封）的 bytes。
pub fn trailer_bytes(pack: &[u8]) -> Result<&[u8]> {
    if pack.len() < HEADER_LEN + FOOTER_LEN {
        return Err(FormatError::Truncated {
            what: "pack",
            needed: HEADER_LEN + FOOTER_LEN,
            actual: pack.len(),
        });
    }
    let m = magic();
    if pack[..HEADER_LEN] != m {
        return Err(FormatError::BadMagic {
            what: "pack header",
        });
    }
    let trailer_len = usize::try_from(parse_footer(pack)?).unwrap_or(usize::MAX);
    let end = pack.len() - FOOTER_LEN;
    let start = end
        .checked_sub(trailer_len)
        .filter(|s| *s >= HEADER_LEN)
        .ok_or(FormatError::Truncated {
            what: "pack trailer",
            needed: trailer_len,
            actual: end.saturating_sub(HEADER_LEN),
        })?;
    Ok(&pack[start..end])
}
