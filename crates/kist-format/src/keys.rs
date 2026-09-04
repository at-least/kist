//! repo 裡物件的 key（路徑）命名規則。
//!
//! ```text
//! config                       明文 CBOR RepoConfig
//! keys/<slot id>               明文 CBOR KeySlot
//! packs/<ObjectId hex>         pack 檔
//! indexes/<ObjectId hex>       envelope(Index)
//! trees/<ObjectId hex>         envelope(Tree)
//! snapshots/<client hex>/<ts>  envelope(Snapshot)
//! gc/<pack ObjectId hex>       待刪標記（M3）
//! ```

use crate::{FormatError, ObjectId, Result};

pub const CONFIG: &str = "config";
pub const KEYS_PREFIX: &str = "keys";
pub const PACKS_PREFIX: &str = "packs";
pub const INDEXES_PREFIX: &str = "indexes";
pub const TREES_PREFIX: &str = "trees";
pub const SNAPSHOTS_PREFIX: &str = "snapshots";
pub const GC_PREFIX: &str = "gc";

pub fn pack(id: &ObjectId) -> String {
    format!("{PACKS_PREFIX}/{id}")
}

pub fn index(id: &ObjectId) -> String {
    format!("{INDEXES_PREFIX}/{id}")
}

pub fn tree(id: &ObjectId) -> String {
    format!("{TREES_PREFIX}/{id}")
}

pub fn key_slot(slot_id: &str) -> String {
    format!("{KEYS_PREFIX}/{slot_id}")
}

pub fn gc(pack_id: &ObjectId) -> String {
    format!("{GC_PREFIX}/{pack_id}")
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

/// 從 `packs/<hex>` 這類 key 取出 ObjectId。
pub fn object_id_from_key(key: &str) -> Result<ObjectId> {
    let name = key
        .rsplit('/')
        .next()
        .ok_or_else(|| FormatError::BadName(key.to_owned()))?;
    ObjectId::from_hex(name)
}
