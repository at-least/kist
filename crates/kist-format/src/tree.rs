//! tree 物件：一個目錄的內容（v2）。
//!
//! - 每個目錄一個（或一串）tree，內容是**依名稱 bytes 排序**的節點清單。
//! - tree 是 content-addressed，而且 v2 的名稱 = keyed BLAKE3(**明文 CBOR**)：
//!   目錄沒變，bytes 就沒變、名稱就沒變，與加密或壓縮的偶然無關。
//! - 超大目錄用 `prev` 串接：每滿 [`MAX_NODES_PER_TREE`] 個節點就先寫出一個
//!   tree，下一個 tree 的 `prev` 指向它。父目錄記錄的是**最後**一段的名稱；
//!   讀取時沿 `prev` 往回收集所有段，再從最舊的一段開始依序讀。
//! - 大檔案的 chunk 清單超過 [`MAX_INLINE_CHUNKS`] 個時改用間接（`ct` = 1）：
//!   清單本身編成 CBOR 的 [`ChunkList`]，當作一般資料切 chunk 存進 pack；
//!   tree 裡的 `chunks` 指向這些資料 chunk。
//!
//! 檔名以 bytes 存放：Unix 上是原始 OS bytes；Windows 上是 UTF-8。
//! Entry 是扁平結構：所有可選欄位「零值省略」，兩個實作必須省略完全相同
//! 的集合（Go 端以指標/自訂型別達成），bytes 才會一致。

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

use crate::{ChunkId, TreeId, FORMAT_VERSION};

/// 單一 tree 物件最多放幾個節點，超過就切段。
pub const MAX_NODES_PER_TREE: usize = 10_000;
/// 檔案的 chunk 清單超過這個數量就改用間接。
pub const MAX_INLINE_CHUNKS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    #[serde(rename = "v")]
    pub version: u32,
    /// 依 `n` 的 bytes 升冪排序。
    #[serde(rename = "entries")]
    pub entries: Vec<Entry>,
    /// 前一段（見模組說明）。
    #[serde(rename = "prev", default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<TreeId>,
}

impl Tree {
    pub fn new(entries: Vec<Entry>, prev: Option<TreeId>) -> Self {
        Self {
            version: FORMAT_VERSION,
            entries,
            prev,
        }
    }
}

/// 節點類型（`t` 欄位的值）。
pub mod node_type {
    pub const FILE: u8 = 0;
    pub const DIR: u8 = 1;
    pub const SYMLINK: u8 = 2;
}

/// 間接內容標記（`ct` 欄位的值）。
pub mod content_type {
    /// `chunks` 直接就是檔案內容的 chunk 清單（欄位省略時也是這個意思）。
    pub const DIRECT: u8 = 0;
    /// `chunks` 指向 `ChunkList` 資料塊：串起來解開才是真正的清單。
    pub const INDIRECT: u8 = 1;
}

/// 目錄裡的一個名字。所有可選欄位零值省略（編碼規則見模組說明）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 檔名（原始 OS bytes）。
    #[serde(rename = "n", with = "serde_bytes")]
    pub name: Vec<u8>,
    /// [`node_type`]。
    #[serde(rename = "t")]
    pub kind: u8,
    /// POSIX mode bits（含檔案類型位元）。Windows = 0。
    #[serde(rename = "mode")]
    pub mode: u32,
    #[serde(rename = "uid", default, skip_serializing_if = "is_zero_u32")]
    pub uid: u32,
    #[serde(rename = "gid", default, skip_serializing_if = "is_zero_u32")]
    pub gid: u32,
    /// 修改時間（奈秒；0 = 未知）。
    #[serde(rename = "mtime")]
    pub mtime_ns: i64,
    /// inode 變更時間（奈秒）。kernel 在任何寫入時更新、無法偽造，
    /// 快速路徑用；0 = 這個平台沒有，不拿來比對。
    #[serde(rename = "ctime", default, skip_serializing_if = "is_zero_i64")]
    pub ctime_ns: i64,
    /// 檔案內容長度（僅檔案）。
    #[serde(rename = "size", default, skip_serializing_if = "is_zero_u64")]
    pub size: u64,
    /// 符號連結的目標（僅符號連結）。
    #[serde(rename = "target", default, with = "serde_bytes", skip_serializing_if = "Vec::is_empty")]
    pub target: Vec<u8>,
    /// 檔案內容（或間接清單，見 `ct`）的 chunk。
    #[serde(rename = "chunks", default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChunkId>,
    /// [`content_type`]；省略 = 直接。
    #[serde(rename = "ct", default, skip_serializing_if = "is_zero_u8")]
    pub content: u8,
    /// 子目錄的 tree（目錄分段時 = 最後一段）。省略 = 全零。
    #[serde(rename = "tree", default, skip_serializing_if = "TreeId::is_zero")]
    pub subtree: TreeId,
    /// 硬連結識別（`nlink` > 1 才記）。
    #[serde(rename = "dev", default, skip_serializing_if = "is_zero_u64")]
    pub dev: u64,
    #[serde(rename = "ino", default, skip_serializing_if = "is_zero_u64")]
    pub inode: u64,
    #[serde(rename = "nlink", default, skip_serializing_if = "is_zero_u64")]
    pub nlink: u64,
    /// 擴充屬性（鍵與值都是 bytes；規範編碼會排序）。
    #[serde(rename = "xattrs", default, skip_serializing_if = "Option::is_none")]
    pub xattrs: Option<std::collections::BTreeMap<ByteBuf, ByteBuf>>,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}
fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

impl TreeId {
    pub fn is_zero(&self) -> bool {
        self.as_bytes() == &[0u8; 32]
    }
}

/// 間接內容的明文：chunk 清單本身。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkList {
    #[serde(rename = "v")]
    pub version: u32,
    #[serde(rename = "chunks")]
    pub chunks: Vec<ChunkId>,
}

impl ChunkList {
    pub fn new(chunks: Vec<ChunkId>) -> Self {
        Self {
            version: FORMAT_VERSION,
            chunks,
        }
    }
}
