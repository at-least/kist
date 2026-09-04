//! snapshot 物件：一次備份的 commit point。
//!
//! 寫入 `snapshots/<client id hex>/<timestamp>`，用 conditional put（不覆蓋）。
//! 所有 pack、index、tree 都上傳完成後才寫 snapshot；沒有 snapshot 指到的東西
//! 就是垃圾，GC 可以收。
//!
//! key 裡的時間戳用「無分隔符」的 ISO 8601 基本格式 `YYYYMMDDTHHMMSSnnnnnnnnnZ`：
//! 字典序 = 時間序，而且不含冒號（冒號在 Windows 檔名裡不合法）。
//! snapshot 內容裡的 `time` 才是給人看的 RFC 3339。

use serde::{Deserialize, Serialize};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

use crate::{FormatError, ObjectId, Result, FORMAT_VERSION};

const KEY_TS_FORMAT: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second][subsecond digits:9]Z");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// 產生這個 snapshot 的機器（16 bytes，隨機產生、存在本機）。
    #[serde(with = "serde_bytes")]
    pub client_id: Vec<u8>,
    pub hostname: String,
    pub username: String,
    /// RFC 3339 UTC，**backup 開始**的時間（不是寫出 snapshot 的時間）：
    /// 下一次 backup 用它判斷「檔案的 mtime/ctime 早於上次開始 → 中間沒動過」。
    /// 與 key 裡的時間戳來自同一個瞬間。
    pub time: String,
    /// 備份的來源路徑（原始 OS bytes，見 tree 模組對檔名的說明）。
    pub paths: Vec<serde_bytes::ByteBuf>,
    /// 根 tree 的名稱。
    pub root: ObjectId,
    /// 同一台機器、同一組路徑的上一個 snapshot key（用來加速：mtime/size 沒變就沿用 chunk 清單）。
    #[serde(default)]
    pub parent: Option<String>,
    pub stats: SnapshotStats,
}

impl Snapshot {
    /// 目前格式版本的 `version` 欄位值。
    pub const VERSION: u32 = FORMAT_VERSION;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SnapshotStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// 所有檔案內容的總 bytes。
    pub bytes_total: u64,
    /// 這次新寫進 repo 的 chunk 明文 bytes。
    pub bytes_new: u64,
    pub chunks_total: u64,
    pub chunks_new: u64,
    pub packs_new: u64,
    /// backup 時讀不到而被略過的項目數（檔案或目錄）。snapshot 仍會寫出，CLI 以非 0 結束。
    #[serde(default)]
    pub errors: u64,
    /// 走快速路徑（metadata 沒變、直接沿用上一個 snapshot 的 chunk 清單）的檔案數。
    #[serde(default)]
    pub files_reused: u64,
}

/// 時間 → snapshot key 用的時間戳。
pub fn format_key_timestamp(t: OffsetDateTime) -> Result<String> {
    t.to_offset(time::UtcOffset::UTC)
        .format(KEY_TS_FORMAT)
        .map_err(|e| FormatError::BadTimestamp(e.to_string()))
}

/// snapshot key 用的時間戳 → 時間。
pub fn parse_key_timestamp(s: &str) -> Result<OffsetDateTime> {
    time::PrimitiveDateTime::parse(s, KEY_TS_FORMAT)
        .map(|p| p.assume_utc())
        .map_err(|e| FormatError::BadTimestamp(format!("{s}: {e}")))
}

/// 時間 → snapshot 內容裡給人看的 RFC 3339。
pub fn format_rfc3339(t: OffsetDateTime) -> Result<String> {
    t.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| FormatError::BadTimestamp(e.to_string()))
}
