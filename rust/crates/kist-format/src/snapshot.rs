//! snapshot 物件：一次備份的 commit point（v3）。
//!
//! 寫入 `snapshots/<client hex>/<timestamp>`，用 conditional put（不覆蓋）。
//! 所有 pack、index、tree 都上傳完成後才寫 snapshot；沒有 snapshot 指到的
//! 東西就是垃圾，GC 可以收。
//!
//! v3 用 `roots: [{path, tree}]` 取代 v2 的 `root`＋`paths`＋合成根：
//! - `path` 是**不透明來源定位**（本機絕對路徑、`sftp://host[:port]/path`、
//!   `s3://bucket/prefix`），不再塞進 tree 節點名——樹節點名一律是單一路徑
//!   元件（見 tree 模組）。v2 的合成根是 stats 口徑分裂點，且塞不下遠端
//!   來源定位，v3 淘汰。
//! - `stats` 只留**資料事實**（files/dirs/symlinks/bytes，依 metadata 種類
//!   定義口徑）。過程計數（新 chunk、新 pack…）依 GC 狀態與去重順序而變，
//!   兩個實作可以「合法地」數出不同數字——不可變結構不收，移到 backup
//!   的執行報告（format 之外）。
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
    /// 備份來源（≥1；依 `path` bytes 升冪排序、不重複；讀取端驗證）。
    #[serde(rename = "roots")]
    pub roots: Vec<Root>,
    /// backup 開始時刻（奈秒；與 key 的時間戳同一瞬間，讀取端核對）。
    #[serde(rename = "time")]
    pub time_ns: i64,
    #[serde(rename = "host")]
    pub host: String,
    #[serde(rename = "user", default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// 產生這個 snapshot 的機器（16 bytes，隨機產生、存在本機）。
    /// 與 key 的 client hex 必須一致。
    #[serde(rename = "client", with = "serde_bytes")]
    pub client_id: Vec<u8>,
    /// 同一台機器、同一組 roots 的上一個 snapshot key。只用於加速，可為 null。
    #[serde(rename = "parent", default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(rename = "stats", default)]
    pub stats: SnapshotStats,
}

impl Snapshot {
    /// 目前格式版本的 `version` 欄位值。
    pub const VERSION: u32 = FORMAT_VERSION;

    /// 結構驗證（讀取端強制）：roots 非空、排序、唯一、tree 非零。
    pub fn validate(&self) -> Result<()> {
        if self.version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: "snapshot",
                version: self.version,
            });
        }
        if self.roots.is_empty() {
            return Err(FormatError::InvalidSnapshot(
                "snapshot must carry at least one root".to_owned(),
            ));
        }
        let mut prev: Option<&[u8]> = None;
        for r in &self.roots {
            if r.path.is_empty() {
                return Err(FormatError::InvalidSnapshot("empty root path".to_owned()));
            }
            if r.tree.is_zero() {
                return Err(FormatError::InvalidSnapshot(
                    "root tree id must not be zero".to_owned(),
                ));
            }
            if let Some(p) = prev {
                if p >= r.path.as_slice() {
                    return Err(FormatError::InvalidSnapshot(
                        "roots must be sorted by path with no duplicates".to_owned(),
                    ));
                }
            }
            prev = Some(r.path.as_slice());
        }
        Ok(())
    }
}

/// 一個備份來源：不透明定位字串 ＋ 根目錄內容的 tree。
/// 序列化：repo 的 CBOR（binary）裡 path 是 byte string；人類可讀格式
/// （`--json`）裡是 lossy UTF-8 字串（與 [`crate::ids`] 的 hex 慣例同一套）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// 來源定位（bytes）：本機絕對路徑（`/srv/data`）、`sftp://host[:port]/path`、
    /// `s3://bucket/prefix`。restore 映射：去掉 scheme 後以 `/` 切段，
    /// 映射到 `<target>/` 之下（`s3://b/p` → `target/b/p`）。
    pub path: ByteBuf,
    /// 根目錄**內容**的 tree（entries = 根目錄的子女；分段時 = 最後一段）。
    pub tree: TreeId,
}

impl Serialize for Root {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        if s.is_human_readable() {
            let mut st = s.serialize_struct("Root", 2)?;
            st.serialize_field(
                "path",
                &String::from_utf8_lossy(self.path.as_slice()).into_owned(),
            )?;
            st.serialize_field("tree", &self.tree)?;
            st.end()
        } else {
            #[derive(Serialize)]
            struct Wire<'a> {
                path: &'a ByteBuf,
                tree: &'a TreeId,
            }
            Wire {
                path: &self.path,
                tree: &self.tree,
            }
            .serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for Root {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        if d.is_human_readable() {
            #[derive(Deserialize)]
            struct Human {
                path: String,
                tree: TreeId,
            }
            let h = Human::deserialize(d)?;
            Ok(Self {
                path: ByteBuf::from(h.path.into_bytes()),
                tree: h.tree,
            })
        } else {
            #[derive(Deserialize)]
            struct Wire {
                path: ByteBuf,
                tree: TreeId,
            }
            let w = Wire::deserialize(d)?;
            Ok(Self {
                path: w.path,
                tree: w.tree,
            })
        }
    }
}

/// 備份統計——只有**資料決定的事實**（兩個實作對同一棵樹必須數出同一組
/// 數字）。口徑（format.md §9.1）：
/// - `files`/`symlinks` 按**名稱**計（hard link 的每個名字各算一個）。
/// - `dirs` 是樹裡的目錄 entry 數；roots 本身不算（root 是 path 不是 entry）。
/// - `bytes` 是檔案內容總和，同一 `(dev,ino)` 群組只算一次（跨 roots）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SnapshotStats {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dirs: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub symlinks: u64,
    /// 所有檔案內容的總 bytes（hard link 內容只算一次）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
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
