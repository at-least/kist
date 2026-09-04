//! index 物件：chunk ID → (pack, offset, length) 的查表。
//!
//! 只是 pack trailer 的快取：每次 backup 結束把這次新寫的 pack 的 trailer 內容
//! 集成一個 index blob 上傳；打開 repo 時把所有 index 讀進來就知道每個 chunk 在哪。
//! 若 index 遺失或損壞，可以把每個 pack 的 trailer 讀一遍重建（`rebuild-index`）。

use serde::{Deserialize, Serialize};

use crate::pack::PackEntry;
use crate::{ObjectId, FORMAT_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexBlob {
    pub version: u32,
    pub packs: Vec<IndexPack>,
    /// 這個 blob 取代了哪些舊的 index blob（M3 repack 用：新 blob 寫好後，舊的走兩階段刪除；
    /// 讀取時若新舊都在，以新的為準）。M1/M2 一律為空。
    #[serde(default)]
    pub supersedes: Vec<ObjectId>,
}

impl IndexBlob {
    pub fn new(packs: Vec<IndexPack>) -> Self {
        Self {
            version: FORMAT_VERSION,
            packs,
            supersedes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPack {
    pub pack: ObjectId,
    /// pack 檔的總長度。`check` 用 HEAD 比對就能抓到被截斷或換掉的 pack。
    pub size: u64,
    pub entries: Vec<PackEntry>,
}
