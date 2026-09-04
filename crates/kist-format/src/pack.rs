//! pack 檔的位元組排版。
//!
//! ```text
//! +----------+----------------------+------------------+------------------+----------+
//! | magic 8B | chunk entry * N      | trailer envelope | trailer len u64  | magic 8B |
//! +----------+----------------------+------------------+------------------+----------+
//! ```
//!
//! - 每個 chunk entry = 24-byte nonce ‖ AEAD 密文（chunk key，AAD = chunk ID）。
//!   entry 的 offset / length 只記在 trailer 裡，entry 本身沒有分隔符。
//! - trailer 是一個 [`crate::envelope::Envelope`]（kind = PackTrailer），
//!   內容是 CBOR 的 [`PackTrailer`]。
//! - 從檔尾讀 16 bytes 就能知道 trailer 在哪：先驗 magic，再取 trailer 長度。
//!   這樣只要一次 range read 就能拿到 trailer，不用下載整個 pack。
//! - pack 的名稱 = BLAKE3(整個檔案的 bytes)，見 [`crate::ObjectId::of`]。

use serde::{Deserialize, Serialize};

use crate::{ChunkId, FormatError, Result, FORMAT_VERSION};

pub const MAGIC: &[u8; 8] = b"KISTPAK1";
pub const HEADER_LEN: usize = 8;
/// 檔尾固定部分：u64 trailer 長度 + magic。
pub const FOOTER_LEN: usize = 8 + 8;
/// 每個 chunk entry 開頭的 nonce 長度。
pub const CHUNK_NONCE_LEN: usize = 24;
/// AEAD tag 長度。
pub const TAG_LEN: usize = 16;

/// `PackEntry::flags` 的位元：bit0 = 明文在加密前先用 zstd 壓過。
pub const FLAG_ZSTD: u8 = 0b0000_0001;

/// trailer 內容：這個 pack 裡有哪些 chunk、各在哪裡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackTrailer {
    pub version: u32,
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

/// 一個 chunk 在 pack 裡的位置。index blob 也用同一個結構。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    /// chunk 的身分（keyed hash of 明文）。
    pub id: ChunkId,
    /// entry 在 pack 檔裡的起點（含 nonce）。
    pub offset: u64,
    /// entry 的總長度：nonce + 密文 + tag。
    pub length: u64,
    /// 明文長度（解壓縮後）。
    pub raw_len: u64,
    /// 見 [`FLAG_ZSTD`]。
    pub flags: u8,
}

/// 開新 pack：回傳已含 magic 的 buffer。
pub fn begin() -> Vec<u8> {
    MAGIC.to_vec()
}

/// 把 trailer envelope 接上去、補上檔尾，pack bytes 就完成了。
pub fn finish(mut pack: Vec<u8>, trailer_envelope: &[u8]) -> Vec<u8> {
    pack.extend_from_slice(trailer_envelope);
    pack.extend_from_slice(&(trailer_envelope.len() as u64).to_le_bytes());
    pack.extend_from_slice(MAGIC);
    pack
}

/// 從 pack 檔的最後 16 bytes 讀出 trailer envelope 的長度。
pub fn parse_footer(footer: &[u8]) -> Result<u64> {
    if footer.len() < FOOTER_LEN {
        return Err(FormatError::Truncated {
            what: "pack footer",
            needed: FOOTER_LEN,
            actual: footer.len(),
        });
    }
    let footer = &footer[footer.len() - FOOTER_LEN..];
    if &footer[8..] != MAGIC {
        return Err(FormatError::BadMagic { what: "pack" });
    }
    let mut len = [0u8; 8];
    len.copy_from_slice(&footer[..8]);
    Ok(u64::from_le_bytes(len))
}

/// 給定整個 pack 的 bytes，切出 trailer envelope 的 bytes。
pub fn trailer_bytes(pack: &[u8]) -> Result<&[u8]> {
    if pack.len() < HEADER_LEN + FOOTER_LEN {
        return Err(FormatError::Truncated {
            what: "pack",
            needed: HEADER_LEN + FOOTER_LEN,
            actual: pack.len(),
        });
    }
    if &pack[..HEADER_LEN] != MAGIC {
        return Err(FormatError::BadMagic { what: "pack" });
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
