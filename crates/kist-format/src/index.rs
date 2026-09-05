//! index blob（v2）：chunk ID → (pack, offset, length) 的查表快取。
//!
//! - 明文 = `algorithm byte（0 原文 / 1 zstd）‖ CBOR`，以 index key 密封
//!   （AAD = [`crate::AAD_INDEX`]）。實測 zstd 後比 v1 的陣列形狀更小。
//! - 讀取端先讀所有 blob、收集全部 `supersedes`，被任何有效 blob 列到的
//!   整個忽略——重疊的 prune / 途中 rebuild-index 都安全。
//! - 只是 pack trailer 的快取，可由所有 pack 重建（`rebuild-index`）。

use serde::{Deserialize, Serialize};

use crate::pack::PackEntry;
use crate::{ObjectId, FORMAT_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexBlob {
    #[serde(rename = "v")]
    pub version: u32,
    #[serde(rename = "packs")]
    pub packs: Vec<IndexPack>,
    /// 這個 blob 取代的舊 index blob。讀取時被列到的 blob 整個忽略。
    #[serde(rename = "supersedes", default, skip_serializing_if = "Vec::is_empty")]
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
    #[serde(rename = "id")]
    pub pack: ObjectId,
    /// pack 檔的總長度。`check` 用 HEAD 比對就能抓到被截斷或換掉的 pack。
    #[serde(rename = "size")]
    pub size: u64,
    #[serde(rename = "entries")]
    pub entries: Vec<PackEntry>,
}
