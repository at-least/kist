//! repo 裡物件的 key（路徑）命名規則（v2）。
//!
//! ```text
//! config                        明文 CBOR RepoConfig
//! keys/<slot id>                明文 CBOR KeySlot
//! packs/<ObjectId hex>          pack 檔
//! indexes/<ObjectId hex>        sealed(IndexBlob)
//! trees/<TreeId hex>            sealed(Tree)，名稱 = keyed hash(明文 CBOR)
//! snapshots/<client hex>/<ts>   sealed(Snapshot)
//! gc/<object hex>               待刪標記：pack / tree / index 共用（內容固定 GC_MARK_MAGIC）
//! parity/<pack hex>             RS 同位 sidecar（選配；讀取端忽略）
//! ```

use crate::{FormatError, ObjectId, Result, TreeId};

pub const CONFIG: &str = "config";
pub const KEYS_PREFIX: &str = "keys";
pub const PACKS_PREFIX: &str = "packs";
pub const INDEXES_PREFIX: &str = "indexes";
pub const TREES_PREFIX: &str = "trees";
pub const SNAPSHOTS_PREFIX: &str = "snapshots";
pub const GC_PREFIX: &str = "gc";
pub const PARITY_PREFIX: &str = "parity";

/// GC 標記的內容（固定 8 bytes）。標記本身不帶資訊：「何時標記」看後端記的
/// 修改時間，「標記什麼」看名稱。內容固定只是讓人與 check 能認出它是 kist 寫的。
pub const GC_MARK_MAGIC: &[u8; 8] = b"KISTGC2\n";

pub fn pack(id: &ObjectId) -> String {
    format!("{PACKS_PREFIX}/{id}")
}

pub fn index(id: &ObjectId) -> String {
    format!("{INDEXES_PREFIX}/{id}")
}

pub fn tree(id: &TreeId) -> String {
    format!("{TREES_PREFIX}/{id}")
}

pub fn parity(pack_id: &ObjectId) -> String {
    format!("{PARITY_PREFIX}/{pack_id}")
}

pub fn key_slot(slot_id: &str) -> String {
    format!("{KEYS_PREFIX}/{slot_id}")
}

/// 待刪標記；`id` 是 pack / tree / index 的名稱。
pub fn gc(id: &ObjectId) -> String {
    format!("{GC_PREFIX}/{id}")
}

/// `snapshots/<client id hex>/<timestamp>`。
pub fn snapshot(client_id: &[u8], key_timestamp: &str) -> String {
    format!(
        "{SNAPSHOTS_PREFIX}/{}/{key_timestamp}",
        hex::encode(client_id)
    )
}

/// 某台 client 所有 snapshot 的 prefix。
pub fn snapshot_prefix(client_id: &[u8]) -> String {
    format!("{SNAPSHOTS_PREFIX}/{}", hex::encode(client_id))
}

/// 從 `packs/<hex>` 這類 key 取出最後一段的 hex 名稱。
pub fn name_from_key(key: &str) -> Result<String> {
    key.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| FormatError::BadName(key.to_owned()))
}

/// 從 `packs/<hex>` 這類 key 取出 ObjectId。
pub fn object_id_from_key(key: &str) -> Result<ObjectId> {
    ObjectId::from_hex(&name_from_key(key)?)
}
