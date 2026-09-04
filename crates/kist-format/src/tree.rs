//! tree 物件：一個目錄的內容。
//!
//! - 每個目錄一個 tree，內容是**依名稱 bytes 排序**的節點清單。
//! - tree 是 content-addressed：目錄沒變，編出來的 bytes 就沒變，物件名稱也沒變，
//!   整棵子樹直接重用。所以這裡的所有欄位都必須是決定性的。
//! - 超大目錄用 `prev` 串接：每滿 [`MAX_NODES_PER_TREE`] 個節點就先寫出一個 tree，
//!   下一個 tree 的 `prev` 指向它。父目錄記錄的是**最後**一段的名稱；讀取時沿 `prev`
//!   往回收集所有段，再從最舊的一段開始依序讀。
//! - 大檔案的 chunk 清單超過 [`MAX_INLINE_CHUNKS`] 時改存 [`Content::Indirect`]：
//!   清單本身編成 CBOR 的 [`ChunkList`]，當作一般資料切成 chunk 存進 pack。
//!
//! 檔名以 bytes 存放：Unix 上是原始的 OS bytes；Windows 上是 UTF-8。

use serde::{Deserialize, Serialize};

use crate::{ChunkId, ObjectId, FORMAT_VERSION};

/// 單一 tree 物件最多放幾個節點，超過就切段。
pub const MAX_NODES_PER_TREE: usize = 10_000;
/// 檔案的 chunk 清單超過這個數量就改用 indirect。
pub const MAX_INLINE_CHUNKS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub version: u32,
    /// 依 `name` 的 bytes 升冪排序。
    pub nodes: Vec<Node>,
    /// 前一段（見模組說明）。
    #[serde(default)]
    pub prev: Option<ObjectId>,
}

impl Tree {
    pub fn new(nodes: Vec<Node>, prev: Option<ObjectId>) -> Self {
        Self {
            version: FORMAT_VERSION,
            nodes,
            prev,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    #[serde(with = "serde_bytes")]
    pub name: Vec<u8>,
    pub meta: NodeMeta,
    pub kind: NodeKind,
}

/// 各平台共通的 metadata。Windows 上 mode / uid / gid 存 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodeMeta {
    /// POSIX mode bits（含檔案類型位元）。
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// 修改時間：Unix 秒 + 奈秒。
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    /// inode 變更時間（Unix 的 ctime）：kernel 在任何寫入時更新、使用者無法設定，
    /// 所以 `cp -p` 這類保留 mtime 的複製也會被抓到。0 = 這個平台沒有，不拿來比對。
    #[serde(default)]
    pub ctime_secs: i64,
    #[serde(default)]
    pub ctime_nanos: u32,
    /// inode 編號。0 = 這個平台沒有，不拿來比對。
    #[serde(default)]
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    File {
        size: u64,
        content: Content,
    },
    Dir {
        subtree: ObjectId,
    },
    Symlink {
        #[serde(with = "serde_bytes")]
        target: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    /// chunk 清單直接放在 tree 裡。
    Direct { chunks: Vec<ChunkId> },
    /// `chunks` 串起來的明文是 CBOR 的 [`ChunkList`]。
    Indirect { chunks: Vec<ChunkId> },
}

/// indirect content 的明文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkList {
    pub version: u32,
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
