//! snapshot 物件：一次備份的 commit point（v2）。
//!
//! 寫入 `snapshots/<client hex>/<timestamp>`，用 conditional put（不覆蓋）。
//! 所有 pack、index、tree 都上傳完成後才寫 snapshot；沒有 snapshot 指到的
//! 東西就是垃圾，GC 可以收。
//!
//! key 裡的時間戳用「無分隔符」的 ISO 8601 基本格式 `YYYYMMDDTHHMMSSnnnnnnnnnZ`：
//! 字典序 = 時間序，而且不含冒號（冒號在 Windows 檔名裡不合法）。
//! 內容裡的 `time` 是 i64 奈秒（與 key 同一瞬間；讀取端核對一致）。
//! AAD = 完整 key 路徑。

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

use crate::{FormatError, Result, TreeId, FORMAT_VERSION};

const KEY_TS_FORMAT: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second][subsecond digits:9]Z");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(rename = "v")]
    pub version: u32,
    /// 根 tree（根目錄分段時 = 最後一段）。
    #[serde(rename = "root")]
    pub root: TreeId,
    /// backup 開始時刻（奈秒；與 key 的時間戳同一瞬間，讀取端核對）。
    #[serde(rename = "time")]
    pub time_ns: i64,
    #[serde(rename = "host")]
    pub host: String,
    #[serde(rename = "user", default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// 備份來源路徑（原始 OS bytes）。
    #[serde(rename = "paths")]
    pub paths: Vec<ByteBuf>,
    /// 產生這個 snapshot 的機器（16 bytes，隨機產生、存在本機）。
    /// 與 key 的 client hex 必須一致。
    #[serde(rename = "client", with = "serde_bytes")]
    pub client_id: Vec<u8>,
    /// 同一台機器、同一組路徑的上一個 snapshot key。只用於加速，可為 null。
    #[serde(rename = "parent", default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(rename = "stats", default)]
    pub stats: SnapshotStats,
}

impl Snapshot {
    /// 目前格式版本的 `version` 欄位值。
    pub const VERSION: u32 = FORMAT_VERSION;
}

/// 備份統計。僅供回報，結構性決策一律不讀它。全部欄位零值省略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SnapshotStats {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dirs: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub symlinks: u64,
    /// 所有檔案內容的總 bytes。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
    /// 這次新寫進 repo 的 chunk 個數。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chunks_new: u64,
    /// 這次讀了資料的 chunk 個數（含沿用的確認）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chunks_read: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub packs_new: u64,
    /// 引用到被 GC 標記的 pack 而重寫資料的次數回報。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub packs_revived: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes_stored: u64,
    /// backup 時讀不到而被略過的項目數。snapshot 仍會寫出，CLI 以非 0 結束。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub errors: u64,
    /// 走快速路徑沿用 chunk 清單的檔案數。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files_reused: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
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
