//! repo 裡的物件與 map 記錄（規格 §1、§10）。欄位只留 GC 用得到的。

use std::collections::{BTreeMap, BTreeSet};

/// 秒。真實時間、後端時鐘、各主機的牆鐘都用這個單位。
pub type Time = i64;
/// 世代號碼。
pub type Gen = u32;
/// client id；0 是維護角色（repack 的輸出）。
pub type ClientId = u8;

pub const HOUR: Time = 3600;
/// 規格的安全邊際 `M`。
pub const M: Time = HOUR;
pub const DEFAULT_GRACE: Time = 72 * HOUR;
/// grace 至少 2M（§11.1）。
pub const MIN_GRACE: Time = 2 * M;
/// P0 的時鐘容差。
pub const PROBE_TOLERANCE: Time = 300;
/// map 分片數（規格是 256；模型用 4 個，分片的增刪規則一樣）。
pub const SHARDS: usize = 4;
pub const MAINTENANCE: ClientId = 0;
/// 被入侵的備份主機：持有主金鑰，只能在自己的前綴下建立物件（S3 的前綴憑證，負責人決定 1）。
pub const MALICIOUS: ClientId = 9;
pub const VERSION: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeltaId(pub u32);

/// `snapshots/<client>/<ts>`。`ts` 在模型裡一律唯一，數字大的較新。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapKey {
    pub client: ClientId,
    pub ts: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// 資料 pack 裡的 chunk。
    Data,
    /// mpack 裡的 metadata item。
    Meta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub grace: Time,
    pub replicas: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drain {
    pub from: Gen,
    pub old_grace: Time,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bad {
    pub key: SnapKey,
    pub first: Gen,
}

/// 重建後的 hold（§11.4）：遺失的世代是 `from..=lost`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hold {
    pub from: Gen,
    pub lost: Gen,
    pub grace: Time,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Live,
    Condemned {
        at: Gen,
        age: Time,
    },
    Tombstone {
        at: Gen,
    },
    Unreadable,
    /// trailer 解得開但版本不認得：不使用、不刪除、每一輪回報（P4）。
    Unknown,
}

/// map 的一筆記錄（§10）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub client: ClientId,
    pub kind: Kind,
    pub entries: BTreeSet<ItemId>,
    pub by_prune: bool,
    pub since: Gen,
    pub status: Status,
    pub held: bool,
}

pub type Map = BTreeMap<PackId, Record>;

/// 排序（§10）：live 先於 condemned，其次 `since` 小的先，其次名稱小的先。tombstone 與
/// unreadable 排在最後（讀取端的最後手段；unreadable 沒人用）。
pub fn rank(rec: &Record, id: PackId, newcomer_first: bool) -> (u8, u32, PackId) {
    let status = match rec.status {
        Status::Live => 0,
        Status::Condemned { .. } => 1,
        Status::Tombstone { .. } => 2,
        Status::Unreadable | Status::Unknown => 3,
    };
    let since = if newcomer_first {
        u32::MAX - rec.since
    } else {
        rec.since
    };
    (status, since, id)
}

pub fn shard_of(id: PackId) -> usize {
    id.0 as usize % SHARDS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trailer {
    Readable {
        version: u8,
        kind: Kind,
        entries: BTreeSet<ItemId>,
    },
    Unreadable,
}

/// pack 或 mpack。`kind` 是命名空間（`packs/` 或 `mpacks/`），`copies` 是實際內容：每個 item 的
/// 副本是真的（解密、重算 ID 會成功）還是假的。trailer 宣稱什麼由 `trailer` 決定，可以與實際不符。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackBody {
    pub client: ClientId,
    pub kind: Kind,
    pub trailer: Trailer,
    pub copies: BTreeMap<ItemId, bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaPack {
    /// pack 所在的前綴。
    pub client: ClientId,
    pub pack: PackId,
    pub kind: Kind,
    pub entries: BTreeSet<ItemId>,
}

/// delta。`client` 在明文裡，AAD 綁住它（§5）；與所在前綴不符就解不開。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaBody {
    pub readable: bool,
    pub client: ClientId,
    pub packs: Vec<DeltaPack>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapBody {
    pub readable: bool,
    pub version: u8,
    /// 哪一組 roots（parent 依 client 與 roots 選）。
    pub job: u8,
    pub root: ItemId,
    pub parent: Option<SnapKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadBody {
    pub version: u8,
    pub g: Gen,
    /// 寫入者的牆鐘時間。
    pub w: Time,
    pub shards: [Option<ShardId>; SHARDS],
    pub settings: Settings,
    pub drain: Option<Drain>,
    pub bad: Vec<Bad>,
    pub hold: Option<Hold>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardBody {
    pub index: usize,
    pub records: Map,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Pack(PackBody),
    Delta(DeltaBody),
    Snap(SnapBody),
    Head(HeadBody),
    Shard(ShardBody),
    Probe,
}

/// repo 的 key（§1）。pack 與 mpack 共用 `Pack`，命名空間在內容的 `kind`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    Pack { client: ClientId, id: PackId },
    PackR1 { client: ClientId, id: PackId },
    Delta { client: ClientId, id: DeltaId },
    Snap(SnapKey),
    SnapR1(SnapKey),
    Head(Gen),
    HeadR1(Gen),
    Shard(ShardId),
    Probe(u32),
}
