//! repo 裡物件的 key（路徑）命名規則（v3）。
//!
//! ```text
//! config                        明文 CBOR RepoConfig
//! keys/<slot id>                明文 CBOR KeySlot
//! packs/<ObjectId hex>          pack 檔
//! indexes/<ObjectId hex>        sealed(IndexBlob)
//! trees/<TreeId hex>            sealed(Tree)，名稱 = keyed hash(明文 CBOR)
//! trees/<TreeId hex>.r1         tree 的副本（選配；內容與主體逐 byte 相同）
//! snapshots/<client hex>/<ts>   sealed(Snapshot)
//! snapshots/<client hex>/<ts>.r1 snapshot 的副本（選配）
//! gc/<object hex>               待刪標記：pack / tree / index 共用（內容固定 GC_MARK_MAGIC）
//! touch/<TreeId hex>            復活訊號（內容固定 TOUCH_MAGIC；覆寫式 Put 刷新 mtime）
//! parity/<pack hex>             RS 同位 sidecar（選配；讀取端忽略）
//! ```
//!
//! 副本與 GC 成組語意：標記鍵＝主體名；刪除主體時一併刪 `.r1` 與 `touch/`。

use crate::{FormatError, ObjectId, Result, TreeId};

pub const CONFIG: &str = "config";
pub const KEYS_PREFIX: &str = "keys";
pub const PACKS_PREFIX: &str = "packs";
pub const INDEXES_PREFIX: &str = "indexes";
pub const TREES_PREFIX: &str = "trees";
pub const SNAPSHOTS_PREFIX: &str = "snapshots";
pub const GC_PREFIX: &str = "gc";
pub const TOUCH_PREFIX: &str = "touch";
pub const PARITY_PREFIX: &str = "parity";

/// 副本後綴（k=1 的複製；主體 `<name>` 的副本是 `<name>.r1`）。
pub const REPLICA_SUFFIX: &str = ".r1";

/// GC 標記的內容（固定 8 bytes）。標記本身不帶資訊：「何時標記」看後端記的
/// 修改時間，「標記什麼」看名稱。內容固定只是讓人與 check 能認出它是 kist 寫的。
pub const GC_MARK_MAGIC: &[u8; 8] = b"KISTGC3\n";

/// touch 物件的內容（固定 8 bytes）。覆寫式 Put 寫入——**mtime 刷新就是
/// 復活訊號本體**（PutIfAbsent 在第二次重用時不會刷新，見 format.md §13.1）。
pub const TOUCH_MAGIC: &[u8; 8] = b"KISTTC3\n";

pub fn pack(id: &ObjectId) -> String {
    format!("{PACKS_PREFIX}/{id}")
}

pub fn index(id: &ObjectId) -> String {
    format!("{INDEXES_PREFIX}/{id}")
}

pub fn tree(id: &TreeId) -> String {
    format!("{TREES_PREFIX}/{id}")
}

/// tree 的 `.r1` 副本。
pub fn tree_replica(id: &TreeId) -> String {
    format!("{TREES_PREFIX}/{id}{REPLICA_SUFFIX}")
}

/// 樹的復活訊號。
pub fn touch(id: &TreeId) -> String {
    format!("{TOUCH_PREFIX}/{id}")
}

pub fn parity(pack_id: &ObjectId) -> String {
    format!("{PARITY_PREFIX}/{pack_id}")
}

pub fn key_slot(slot_id: &str) -> String {
    format!("{KEYS_PREFIX}/{slot_id}")
}

/// 待刪標記；`id` 是 pack / index 的名稱。
pub fn gc(id: &ObjectId) -> String {
    format!("{GC_PREFIX}/{id}")
}

/// 樹的待刪標記（`gc/` 命名空間以 hex 共用）。
pub fn gc_tree(id: &TreeId) -> String {
    format!("{GC_PREFIX}/{id}")
}

/// `snapshots/<client id hex>/<timestamp>`。
pub fn snapshot(client_id: &[u8], key_timestamp: &str) -> String {
    format!(
        "{SNAPSHOTS_PREFIX}/{}/{key_timestamp}",
        hex::encode(client_id)
    )
}

/// snapshot 的 `.r1` 副本（寫在主體**之前**：主體出現＝commit）。
pub fn snapshot_replica(client_id: &[u8], key_timestamp: &str) -> String {
    format!(
        "{SNAPSHOTS_PREFIX}/{}/{key_timestamp}{REPLICA_SUFFIX}",
        hex::encode(client_id)
    )
}

/// 某台 client 所有 snapshot 的 prefix。
pub fn snapshot_prefix(client_id: &[u8]) -> String {
    format!("{SNAPSHOTS_PREFIX}/{}", hex::encode(client_id))
}

/// 從 `packs/<hex>` 這類 key 取出最後一段的名稱。
pub fn name_from_key(key: &str) -> Result<String> {
    let name = key
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FormatError::BadName(key.to_owned()))?;
    Ok(name.to_owned())
}

/// 若名稱是副本（`<hex>.r1`）則剝掉後綴回傳主體名。
pub fn strip_replica(name: &str) -> Option<&str> {
    name.strip_suffix(REPLICA_SUFFIX)
}

/// 從 `packs/<hex>` 這類 key 取出 ObjectId。
pub fn object_id_from_key(key: &str) -> Result<ObjectId> {
    ObjectId::from_hex(&name_from_key(key)?)
}
