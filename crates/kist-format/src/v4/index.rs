//! §10：世代 head、map 分片與記錄、delta。
//!
//! ```text
//! Head   { v: 4, g, w, parent（g=0 省略）, shards: [256 個分片名稱或 null],
//!          settings: { grace, replicas, pack_target, mpack_target }, drain?, bad?, hold? }
//! Shard  { v, index, records }   分片 i 放名稱第一個 byte 等於 i 的記錄，依名稱嚴格遞增
//! Record { id, client, kind, size, entries, by_prune, since, status: { s, at, age }, held }
//! Delta  { v, client, packs: [{ id, kind, size, entries }] }
//! ```
//!
//! 密封在 kist-crypto（§5）：head 的 AAD 是 key 路徑；分片的 AAD 是 `"kist/v4/shard"`，分片號碼在明文裡；
//! delta 的 AAD 是 `"kist/v4/delta"` ‖ client。
//!
//! 讀取端拒絕的值只有兩類：§4 第 8 條的在場規則（head 的 `parent`、狀態的 `at` 與 `age`），以及與 §10、
//! §11 的定義矛盾、照用會讓等待變短、信錯持有者或找不到記錄的值。指向不存在的世代、超出 pack 的位移這類值
//! 在使用時本來就會出錯，這裡不檢查。時間的判斷（`T`、`w`、兩個時鐘）在 prune 的實作。

use serde::{Deserialize, Serialize};

use super::ids::{ClientId, ObjectId};
use super::keys::{Gen, Key};
use super::pack::{Kind, PackEntry};
use super::{check_version, corrupt, invalid, Result, VERSION};
use crate::cbor;

/// head 指向的分片數。
pub const SHARDS: usize = 256;
/// grace 的下限（秒，§11.1）。
pub const MIN_GRACE: u64 = 2 * 3600;
/// init 的 grace（秒）。
pub const DEFAULT_GRACE: u64 = 72 * 3600;
/// pack_target 與 mpack_target 的範圍，與 v3 的 pack_target 相同。
pub const MIN_PACK_TARGET: u64 = 64 * 1024;
pub const MAX_PACK_TARGET: u64 = 4 * 1024 * 1024 * 1024;

/// 一個 pack 或 mpack 的記錄所在的分片：名稱的第一個 byte。
pub fn shard_of(id: &ObjectId) -> u8 {
    id.as_bytes()[0]
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// head 的 settings：§4 的欄位順序 grace, replicas, pack_target, mpack_target。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// 秒，至少 [`MIN_GRACE`]。
    pub grace: u64,
    /// 0 或 1：mpack、snapshot、map 分片是否也寫 `.r1`（head 一律寫，§13）。
    pub replicas: u8,
    pub pack_target: u64,
    pub mpack_target: u64,
}

impl Settings {
    /// init 的預設：grace 72 小時、pack 64 MiB、mpack 8 MiB；`replicas` 由後端決定（本機 1、S3 與 SFTP 0）。
    pub fn defaults(replicas: u8) -> Self {
        Self {
            grace: DEFAULT_GRACE,
            replicas,
            pack_target: 64 * 1024 * 1024,
            mpack_target: 8 * 1024 * 1024,
        }
    }

    fn check(&self) -> Result<()> {
        let bad = |msg: String| Err(corrupt("head settings", msg));
        if self.grace < MIN_GRACE {
            return bad(format!("grace {} s is under {MIN_GRACE} s", self.grace));
        }
        if self.replicas > 1 {
            return bad(format!("replicas {} is not 0 or 1", self.replicas));
        }
        for (what, v) in [
            ("pack_target", self.pack_target),
            ("mpack_target", self.mpack_target),
        ] {
            if !(MIN_PACK_TARGET..=MAX_PACK_TARGET).contains(&v) {
                return bad(format!(
                    "{what} {v} is outside {MIN_PACK_TARGET}..={MAX_PACK_TARGET}"
                ));
            }
        }
        Ok(())
    }
}

/// grace 調低之後的 drain：§4 的欄位順序 from, old_grace。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drain {
    /// 調低的世代。
    pub from: Gen,
    /// 調低之前的 grace（秒），一定大於現在的 grace：drain 期間新的 condemned 記錄用它當 `age`。
    pub old_grace: u64,
}

/// 兩份都解不開的 snapshot（§11.3 P5）：§4 的欄位順序 key, first。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bad {
    /// snapshot 主體的 key 路徑。P9 會刪掉它的主體與 `.r1`，所以只能是 snapshot 的 key。
    pub key: String,
    /// 它第一次被記進 head 的世代。
    pub first: Gen,
}

/// 重建後的 hold（§11.4）：§4 的欄位順序 from, lost, grace。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    /// 遺失的世代是 `from..=lost`。
    pub from: Gen,
    pub lost: Gen,
    /// 這段世代的 head 裡最大的 grace（秒）。
    pub grace: u64,
}

/// 世代 head `maint/gen/<g>`：§4 的欄位順序 v, g, w, parent, shards, settings, drain, bad, hold。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub v: u32,
    pub g: Gen,
    /// 寫入者的牆鐘時間（Unix 奈秒）。
    pub w: i64,
    /// BLAKE3(head g−1 寫進 repo 的 bytes，主體與 `.r1` 相同)；g=0 時省略，其他世代必填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ObjectId>,
    /// 分片 i 的名稱，空分片是 null。
    pub shards: Vec<Option<ObjectId>>,
    pub settings: Settings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<Drain>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bad: Vec<Bad>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<Hold>,
}

impl Head {
    /// init 寫的 head 0：空的 map。
    pub fn initial(w: i64, settings: Settings) -> Self {
        Self {
            v: VERSION,
            g: 0,
            w,
            parent: None,
            shards: vec![None; SHARDS],
            settings,
            drain: None,
            bad: Vec::new(),
            hold: None,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check().map_err(|e| invalid("head", e.to_string()))?;
        Ok(cbor::encode(self)?)
    }

    /// 讀取端：`g` 是 key 的世代，必須等於明文裡的 `g`（AAD 也綁著 key 路徑）。
    pub fn decode(bytes: &[u8], g: Gen) -> Result<Self> {
        let h: Head = cbor::decode(bytes)?;
        h.check()?;
        if h.g != g {
            return Err(corrupt(
                "head",
                format!("head {} is stored as generation {g}", h.g),
            ));
        }
        Ok(h)
    }

    fn check(&self) -> Result<()> {
        check_version("head", self.v)?;
        let bad = |msg: String| Err(corrupt("head", msg));
        if self.shards.len() != SHARDS {
            return bad(format!("{} shards, not {SHARDS}", self.shards.len()));
        }
        if self.parent.is_some() != (self.g > 0) {
            return bad(format!(
                "head {} must have a parent exactly when g > 0",
                self.g
            ));
        }
        self.settings.check()?;
        if let Some(d) = self.drain {
            if d.old_grace <= self.settings.grace {
                return bad(format!(
                    "drain old grace {} s is not above the grace {} s",
                    d.old_grace, self.settings.grace
                ));
            }
        }
        if let Some(h) = self.hold {
            if h.from > h.lost || h.grace < MIN_GRACE {
                return bad(format!(
                    "hold {}..={} with grace {} s",
                    h.from, h.lost, h.grace
                ));
            }
        }
        for b in &self.bad {
            if !matches!(Key::parse(&b.key), Ok(Key::Snapshot { replica: false, .. })) {
                return bad(format!("bad entry {:?} is not a snapshot key", b.key));
            }
        }
        Ok(())
    }
}

/// 記錄狀態的 `s`。
pub mod status {
    pub const LIVE: u8 = 0;
    pub const CONDEMNED: u8 = 1;
    pub const TOMBSTONE: u8 = 2;
    pub const UNREADABLE: u8 = 3;
    pub const UNKNOWN: u8 = 4;
}

/// 記錄的狀態：§4 的欄位順序 s, at, age（零值省略）。condemned 必須有 `at`（標記的世代）與 `age`（當時的
/// age，秒，是一個 grace 值）；tombstone 必須有 `at`、不得有 `age`；live、unreadable、unknown 兩個都不得有。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub s: u8,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub at: Gen,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub age: u64,
}

impl Status {
    pub const LIVE: Status = Status {
        s: status::LIVE,
        at: 0,
        age: 0,
    };

    fn check(&self) -> Result<()> {
        let ok = match self.s {
            status::LIVE | status::UNREADABLE | status::UNKNOWN => self.at == 0 && self.age == 0,
            status::CONDEMNED => self.at > 0 && self.age >= MIN_GRACE,
            status::TOMBSTONE => self.at > 0 && self.age == 0,
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(corrupt("record", format!("status {self:?}")))
        }
    }
}

/// 一個 pack 或 mpack 的記錄：§4 的欄位順序 id, client, kind, size, entries, by_prune, since, status, held。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// pack 的名稱。
    pub id: ObjectId,
    /// pack 所在前綴的 client。
    pub client: ClientId,
    pub kind: Kind,
    /// pack 的 bytes。
    pub size: u64,
    /// 取自 prune 自己讀到的 trailer；之後唯一的改變是移除被證明不實的項目（P7c、P7d），所以不一定連續。
    pub entries: Vec<PackEntry>,
    /// prune 自己寫出、讀回驗證過的 pack（repack 的輸出，一定在全零 client 的前綴下）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub by_prune: bool,
    /// 這筆記錄進入 map 的世代。
    pub since: Gen,
    pub status: Status,
    /// 只在重建後的 hold 期間有意義（§11.4）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub held: bool,
}

impl Record {
    fn check(&self) -> Result<()> {
        self.status.check()?;
        // B2 不驗證就信 by_prune 的持有者：記在備份主機的前綴下，就是信了 prune 沒有驗證過的內容。
        if self.by_prune && self.client != ClientId::MAINT {
            return Err(corrupt(
                "record",
                format!(
                    "pack {} is by_prune outside the maintenance prefix",
                    self.id
                ),
            ));
        }
        Ok(())
    }

    /// §10 的排序（選正本、讀取順序）：live 先於 condemned，tombstone 最後（只在其他都失敗時用）；同一級裡
    /// `since` 小的先，其次名稱小的先，所以新進的 pack 不會取代既有的持有者。unreadable 與 unknown 不是持有者，
    /// 回 `None`。P6 的正本是這個順序裡第一個非 tombstone、可讀、種類相符的持有者。
    pub fn order_key(&self) -> Option<(u8, Gen, ObjectId)> {
        let rank = match self.status.s {
            status::LIVE => 0,
            status::CONDEMNED => 1,
            status::TOMBSTONE => 2,
            _ => return None,
        };
        Some((rank, self.since, self.id))
    }
}

/// map 分片 `maint/map/<hex>`：§4 的欄位順序 v, index, records。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    pub v: u32,
    pub index: u8,
    /// 依名稱嚴格遞增：一個 pack 只有一筆記錄，同樣的內容也只有一種寫法。
    pub records: Vec<Record>,
}

impl Shard {
    pub fn new(index: u8, mut records: Vec<Record>) -> Self {
        records.sort_by_key(|r| r.id);
        Self {
            v: VERSION,
            index,
            records,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check()
            .map_err(|e| invalid("map shard", e.to_string()))?;
        Ok(cbor::encode(self)?)
    }

    /// 讀取端：`index` 是 head 指向它的位置，必須等於明文裡的分片號碼（§5）。
    pub fn decode(bytes: &[u8], index: u8) -> Result<Self> {
        let s: Shard = cbor::decode(bytes)?;
        s.check()?;
        if s.index != index {
            return Err(corrupt(
                "map shard",
                format!("shard {} is at position {index}", s.index),
            ));
        }
        Ok(s)
    }

    fn check(&self) -> Result<()> {
        check_version("map shard", self.v)?;
        let mut prev: Option<ObjectId> = None;
        for r in &self.records {
            r.check()?;
            if shard_of(&r.id) != self.index {
                return Err(corrupt(
                    "map shard",
                    format!("pack {} does not belong in shard {}", r.id, self.index),
                ));
            }
            if prev.is_some_and(|p| p >= r.id) {
                return Err(corrupt(
                    "map shard",
                    format!("pack {} is out of order or repeats", r.id),
                ));
            }
            prev = Some(r.id);
        }
        Ok(())
    }
}

/// delta 裡的一個 pack：§4 的欄位順序 id, kind, size, entries。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaPack {
    pub id: ObjectId,
    pub kind: Kind,
    pub size: u64,
    pub entries: Vec<PackEntry>,
}

/// delta `deltas/<c>/<hex>`：§4 的欄位順序 v, client, packs。只是提示，prune 從不依 delta 做決定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    pub v: u32,
    pub client: ClientId,
    pub packs: Vec<DeltaPack>,
}

impl Delta {
    pub fn new(client: ClientId, packs: Vec<DeltaPack>) -> Self {
        Self {
            v: VERSION,
            client,
            packs,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        check_version("delta", self.v).map_err(|e| invalid("delta", e.to_string()))?;
        Ok(cbor::encode(self)?)
    }

    /// 讀取端：`client` 是 delta 所在前綴的 client，必須等於明文裡的 client（AAD 也綁著它）。
    pub fn decode(bytes: &[u8], client: &ClientId) -> Result<Self> {
        let d: Delta = cbor::decode(bytes)?;
        check_version("delta", d.v)?;
        if d.client != *client {
            return Err(corrupt("delta", "client differs from its prefix"));
        }
        Ok(d)
    }

    /// delta 裡的 pack 的 key：只能在 delta 所在 client 的前綴下（§10），所以 key 由 delta 的 client 決定。
    pub fn pack_key(&self, p: &DeltaPack) -> Key {
        match p.kind {
            Kind::Data => Key::Pack {
                client: self.client,
                id: p.id,
            },
            Kind::Meta => Key::Mpack {
                client: self.client,
                id: p.id,
                replica: false,
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::v4::ids::ItemId;
    use crate::v4::keys::SnapTs;
    use crate::v4::Error;

    fn entry(b: u8) -> PackEntry {
        PackEntry {
            i: ItemId::from_bytes([b; 32]),
            o: 8,
            l: 41,
            r: 10,
        }
    }

    /// 名稱的第一個 byte 是 `first`（決定分片），其餘是 `rest`。
    fn name(first: u8, rest: u8) -> ObjectId {
        let mut b = [rest; 32];
        b[0] = first;
        ObjectId::from_bytes(b)
    }

    fn record(id: ObjectId, since: Gen, status: Status) -> Record {
        Record {
            id,
            client: ClientId::from_bytes([3; 16]),
            kind: Kind::Data,
            size: 100,
            entries: vec![entry(1)],
            by_prune: false,
            since,
            status,
            held: false,
        }
    }

    fn st(s: u8, at: Gen, age: u64) -> Status {
        Status { s, at, age }
    }

    fn snapshot_key(replica: bool) -> String {
        Key::Snapshot {
            client: ClientId::from_bytes([0x1f; 16]),
            ts: SnapTs::from_nanos(1_788_523_200_000_000_000).unwrap(),
            replica,
        }
        .path()
    }

    fn head(g: Gen) -> Head {
        let mut h = Head::initial(1000, Settings::defaults(1));
        h.g = g;
        if g > 0 {
            h.parent = Some(ObjectId::from_bytes([0x11; 32]));
        }
        h
    }

    /// 繞過寫入端的檢查，做出讀取端要拒絕的 bytes。
    fn raw<T: Serialize>(v: &T) -> Vec<u8> {
        cbor::encode(v).unwrap()
    }

    const SETTINGS_HEX: &str = concat!(
        "a4",
        "656772616365",               // grace
        "1a0003f480",                 // 259200
        "687265706c69636173",         // replicas
        "01",                         // 1
        "6b7061636b5f746172676574",   // pack_target
        "1a04000000",                 // 64 MiB
        "6c6d7061636b5f746172676574", // mpack_target
        "1a00800000",                 // 8 MiB
    );

    #[test]
    fn head_zero_encodes_as_specified() {
        // 手寫的預期值：{v:4, g:0, w:1000, shards:[null × 256], settings}，沒有 parent、drain、bad、hold。
        let expected = [
            "a5",
            "617604",             // v: 4
            "616700",             // g: 0
            "61771903e8",         // w: 1000
            "66736861726473",     // shards
            "990100",             // array(256)
            &"f6".repeat(256),    // null × 256
            "6873657474696e6773", // settings
            SETTINGS_HEX,
        ]
        .concat();
        assert_eq!(hex::encode(head(0).encode().unwrap()), expected);
    }

    #[test]
    fn a_full_head_encodes_as_specified() {
        let mut h = head(2);
        h.shards[0] = Some(ObjectId::from_bytes([0x22; 32]));
        h.settings.grace = MIN_GRACE;
        h.drain = Some(Drain {
            from: 2,
            old_grace: DEFAULT_GRACE,
        });
        h.bad = vec![Bad {
            key: snapshot_key(false),
            first: 1,
        }];
        h.hold = Some(Hold {
            from: 1,
            lost: 1,
            grace: DEFAULT_GRACE,
        });
        let key = snapshot_key(false);
        assert_eq!(key.len(), 0x44);
        let expected = [
            "a9",
            "617604",         // v: 4
            "616702",         // g: 2
            "61771903e8",     // w: 1000
            "66706172656e74", // parent
            "5820",
            &"11".repeat(32),
            "66736861726473", // shards
            "990100",
            "5820",
            &"22".repeat(32),
            &"f6".repeat(255),
            "6873657474696e6773", // settings, grace 7200
            "a4",
            "656772616365",
            "191c20",
            "687265706c69636173",
            "01",
            "6b7061636b5f746172676574",
            "1a04000000",
            "6c6d7061636b5f746172676574",
            "1a00800000",
            "65647261696e", // drain
            "a2",
            "6466726f6d02",         // from: 2
            "696f6c645f6772616365", // old_grace
            "1a0003f480",
            "63626164", // bad
            "81a2",
            "636b6579", // key
            "7844",
            &hex::encode(key.as_bytes()),
            "65666972737401", // first: 1
            "64686f6c64",     // hold
            "a3",
            "6466726f6d01", // from: 1
            "646c6f737401", // lost: 1
            "656772616365", // grace
            "1a0003f480",
        ]
        .concat();
        let bytes = h.encode().unwrap();
        assert_eq!(hex::encode(&bytes), expected);
        assert_eq!(Head::decode(&bytes, 2).unwrap(), h);
    }

    #[test]
    fn the_defaults_are_the_specified_ones() {
        let s = Settings::defaults(0);
        assert_eq!(
            (s.grace, s.replicas, s.pack_target, s.mpack_target),
            (72 * 3600, 0, 64 << 20, 8 << 20)
        );
        let h = Head::initial(5, Settings::defaults(1));
        assert_eq!((h.g, h.w, h.parent, h.shards.len()), (0, 5, None, 256));
        assert!(h.shards.iter().all(Option::is_none));
    }

    #[test]
    fn the_head_must_match_its_generation() {
        let bytes = head(3).encode().unwrap();
        assert_eq!(Head::decode(&bytes, 3).unwrap(), head(3));
        assert!(matches!(
            Head::decode(&bytes, 4),
            Err(Error::Corrupt { .. })
        ));
    }

    #[test]
    fn bad_heads_are_rejected() {
        let cases: Vec<(&str, Head)> = vec![
            ("g > 0 without a parent", {
                let mut h = head(1);
                h.parent = None;
                h
            }),
            ("g = 0 with a parent", {
                let mut h = head(0);
                h.parent = Some(ObjectId::from_bytes([1; 32]));
                h
            }),
            ("255 shards", {
                let mut h = head(0);
                h.shards.pop();
                h
            }),
            ("257 shards", {
                let mut h = head(0);
                h.shards.push(None);
                h
            }),
            ("grace under 2 hours", {
                let mut h = head(0);
                h.settings.grace = MIN_GRACE - 1;
                h
            }),
            ("replicas 2", {
                let mut h = head(0);
                h.settings.replicas = 2;
                h
            }),
            ("pack_target under the minimum", {
                let mut h = head(0);
                h.settings.pack_target = MIN_PACK_TARGET - 1;
                h
            }),
            ("mpack_target over the maximum", {
                let mut h = head(0);
                h.settings.mpack_target = MAX_PACK_TARGET + 1;
                h
            }),
            ("drain whose old grace is the grace", {
                let mut h = head(1);
                h.drain = Some(Drain {
                    from: 1,
                    old_grace: h.settings.grace,
                });
                h
            }),
            ("hold with from > lost", {
                let mut h = head(5);
                h.hold = Some(Hold {
                    from: 3,
                    lost: 2,
                    grace: DEFAULT_GRACE,
                });
                h
            }),
            ("hold grace under 2 hours", {
                let mut h = head(5);
                h.hold = Some(Hold {
                    from: 2,
                    lost: 3,
                    grace: MIN_GRACE - 1,
                });
                h
            }),
            ("bad entry is a pack key", {
                let mut h = head(1);
                h.bad = vec![Bad {
                    key: Key::Pack {
                        client: ClientId::from_bytes([1; 16]),
                        id: ObjectId::from_bytes([2; 32]),
                    }
                    .path(),
                    first: 1,
                }];
                h
            }),
            ("bad entry is a snapshot replica", {
                let mut h = head(1);
                h.bad = vec![Bad {
                    key: snapshot_key(true),
                    first: 1,
                }];
                h
            }),
            ("bad entry is not a key", {
                let mut h = head(1);
                h.bad = vec![Bad {
                    key: "snapshots/x".into(),
                    first: 1,
                }];
                h
            }),
        ];
        for (what, h) in cases {
            assert!(matches!(h.encode(), Err(Error::Invalid { .. })), "{what}");
            assert!(
                matches!(Head::decode(&raw(&h), h.g), Err(Error::Corrupt { .. })),
                "{what}"
            );
        }
        let mut h = head(0);
        h.v = 5;
        assert!(matches!(
            Head::decode(&raw(&h), 0),
            Err(Error::Version { version: 5, .. })
        ));
    }

    #[test]
    fn the_boundaries_are_accepted() {
        let mut h = head(4);
        h.settings.grace = MIN_GRACE;
        h.settings.replicas = 0;
        h.settings.pack_target = MIN_PACK_TARGET;
        h.settings.mpack_target = MAX_PACK_TARGET;
        h.drain = Some(Drain {
            from: 4,
            old_grace: MIN_GRACE + 1,
        });
        h.hold = Some(Hold {
            from: 2,
            lost: 2,
            grace: MIN_GRACE,
        });
        h.bad = vec![Bad {
            key: snapshot_key(false),
            first: 3,
        }];
        assert_eq!(Head::decode(&h.encode().unwrap(), 4).unwrap(), h);
    }

    #[test]
    fn a_record_encodes_as_specified() {
        // 手寫的預期值：九個欄位都在場（by_prune 與 held 是 true、狀態是 condemned）。
        let r = Record {
            id: ObjectId::from_bytes([0xaa; 32]),
            client: ClientId::MAINT,
            kind: Kind::Meta,
            size: 100,
            entries: vec![entry(0xcc)],
            by_prune: true,
            since: 3,
            status: st(status::CONDEMNED, 5, MIN_GRACE),
            held: true,
        };
        let full = [
            "a9",
            "626964", // id
            "5820",
            &"aa".repeat(32),
            "66636c69656e74", // client
            "50",
            &"00".repeat(16),
            "646b696e6401",     // kind: 1
            "6473697a651864",   // size: 100
            "67656e7472696573", // entries
            "81a4",
            "61695820",
            &"cc".repeat(32),
            "616f08",
            "616c1829",
            "61720a",
            "6862795f7072756e65f5", // by_prune: true
            "6573696e636503",       // since: 3
            "66737461747573",       // status {s:1, at:5, age:7200}
            "a3617301626174056361676519",
            "1c20",
            "6468656c64f5", // held: true
        ]
        .concat();
        assert_eq!(hex::encode(raw(&r)), full);
        // false 與零值省略：{id, client, kind, size, entries, since, status:{s:0}}。
        let r = Record {
            by_prune: false,
            held: false,
            status: Status::LIVE,
            client: ClientId::from_bytes([0xbb; 16]),
            ..r
        };
        let short = [
            "a7",
            "626964",
            "5820",
            &"aa".repeat(32),
            "66636c69656e74",
            "50",
            &"bb".repeat(16),
            "646b696e6401",
            "6473697a651864",
            "67656e7472696573",
            "81a4",
            "61695820",
            &"cc".repeat(32),
            "616f08",
            "616c1829",
            "61720a",
            "6573696e636503",
            "66737461747573",
            "a1617300", // status {s:0}
        ]
        .concat();
        assert_eq!(hex::encode(raw(&r)), short);
    }

    #[test]
    fn statuses_carry_exactly_their_fields() {
        let ok = [
            st(status::LIVE, 0, 0),
            st(status::CONDEMNED, 1, MIN_GRACE),
            st(status::TOMBSTONE, 1, 0),
            st(status::UNREADABLE, 0, 0),
            st(status::UNKNOWN, 0, 0),
        ];
        let bad = [
            ("live with at", st(status::LIVE, 1, 0)),
            ("live with age", st(status::LIVE, 0, MIN_GRACE)),
            ("condemned without at", st(status::CONDEMNED, 0, MIN_GRACE)),
            ("condemned without age", st(status::CONDEMNED, 1, 0)),
            (
                "condemned with an age under 2 hours",
                st(status::CONDEMNED, 1, MIN_GRACE - 1),
            ),
            ("tombstone without at", st(status::TOMBSTONE, 0, 0)),
            ("tombstone with age", st(status::TOMBSTONE, 1, MIN_GRACE)),
            ("unreadable with at", st(status::UNREADABLE, 1, 0)),
            ("unknown with age", st(status::UNKNOWN, 0, MIN_GRACE)),
            ("an unknown status", st(5, 0, 0)),
        ];
        for s in ok {
            let shard = Shard::new(7, vec![record(name(7, 1), 1, s)]);
            assert_eq!(Shard::decode(&shard.encode().unwrap(), 7).unwrap(), shard);
        }
        for (what, s) in bad {
            let shard = Shard::new(7, vec![record(name(7, 1), 1, s)]);
            assert!(
                matches!(shard.encode(), Err(Error::Invalid { .. })),
                "{what}"
            );
            assert!(
                matches!(Shard::decode(&raw(&shard), 7), Err(Error::Corrupt { .. })),
                "{what}"
            );
        }
    }

    #[test]
    fn by_prune_belongs_to_the_maintenance_prefix() {
        let mut r = record(name(7, 1), 1, Status::LIVE);
        r.by_prune = true;
        let shard = Shard::new(7, vec![r.clone()]);
        assert!(matches!(
            Shard::decode(&raw(&shard), 7),
            Err(Error::Corrupt { .. })
        ));
        r.client = ClientId::MAINT;
        let shard = Shard::new(7, vec![r]);
        assert_eq!(Shard::decode(&shard.encode().unwrap(), 7).unwrap(), shard);
    }

    #[test]
    fn a_shard_encodes_as_specified() {
        // 手寫的預期值：{v:4, index:170, records:[{...}]}。
        let r = record(name(0xaa, 0xaa), 1, Status::LIVE);
        let shard = Shard::new(0xaa, vec![r.clone()]);
        let expected = [
            "a3",
            "617604",           // v: 4
            "65696e64657818aa", // index: 170
            "677265636f726473", // records
            "81",
            &hex::encode(raw(&r)),
        ]
        .concat();
        assert_eq!(hex::encode(shard.encode().unwrap()), expected);
    }

    #[test]
    fn shards_hold_their_own_records_in_order() {
        let a = record(name(7, 1), 1, Status::LIVE);
        let b = record(name(7, 2), 1, Status::LIVE);
        // new 依名稱排序。
        let shard = Shard::new(7, vec![b.clone(), a.clone()]);
        assert_eq!(shard.records, vec![a.clone(), b.clone()]);
        let bytes = shard.encode().unwrap();
        assert_eq!(Shard::decode(&bytes, 7).unwrap(), shard);
        // 分片號碼必須等於 head 指向它的位置。
        assert!(matches!(
            Shard::decode(&bytes, 8),
            Err(Error::Corrupt { .. })
        ));
        let cases = [
            (
                "a record of another shard",
                Shard {
                    v: VERSION,
                    index: 7,
                    records: vec![a.clone(), record(name(8, 0), 1, Status::LIVE)],
                },
            ),
            (
                "out of order",
                Shard {
                    v: VERSION,
                    index: 7,
                    records: vec![b.clone(), a.clone()],
                },
            ),
            (
                "the same pack twice",
                Shard {
                    v: VERSION,
                    index: 7,
                    records: vec![a.clone(), a.clone()],
                },
            ),
        ];
        for (what, s) in cases {
            assert!(matches!(s.encode(), Err(Error::Invalid { .. })), "{what}");
            assert!(
                matches!(Shard::decode(&raw(&s), 7), Err(Error::Corrupt { .. })),
                "{what}"
            );
        }
        let mut s = shard;
        s.v = 5;
        assert!(matches!(
            Shard::decode(&raw(&s), 7),
            Err(Error::Version { version: 5, .. })
        ));
        assert_eq!(shard_of(&name(0xfe, 0)), 0xfe);
    }

    #[test]
    fn holders_are_ordered_by_status_then_since_then_name() {
        let live_old_big = record(name(9, 0xff), 1, Status::LIVE);
        let live_old_small = record(name(9, 0x01), 1, Status::LIVE);
        let live_new_small = record(name(9, 0x00), 2, Status::LIVE);
        let condemned_oldest = record(name(9, 0x00), 0, st(status::CONDEMNED, 1, MIN_GRACE));
        let tombstone = record(name(9, 0x00), 0, st(status::TOMBSTONE, 1, 0));
        let unreadable = record(name(9, 0x00), 0, st(status::UNREADABLE, 0, 0));
        let unknown = record(name(9, 0x00), 0, st(status::UNKNOWN, 0, 0));
        assert_eq!(unreadable.order_key(), None);
        assert_eq!(unknown.order_key(), None);
        let mut holders = vec![
            &tombstone,
            &condemned_oldest,
            &live_new_small,
            &live_old_big,
            &live_old_small,
        ];
        holders.sort_by_key(|r| r.order_key().unwrap());
        // 新進的 pack（since 2）排在既有的持有者之後，即使名稱較小；condemned 即使最早也排在 live 之後。
        assert_eq!(
            holders,
            vec![
                &live_old_small,
                &live_old_big,
                &live_new_small,
                &condemned_oldest,
                &tombstone
            ]
        );
    }

    #[test]
    fn a_delta_encodes_as_specified() {
        // 手寫的預期值：{v:4, client, packs:[{id, kind:0, size:100, entries:[...]}]}。
        let d = Delta::new(
            ClientId::from_bytes([0xbb; 16]),
            vec![DeltaPack {
                id: ObjectId::from_bytes([0xaa; 32]),
                kind: Kind::Data,
                size: 100,
                entries: vec![entry(0xcc)],
            }],
        );
        let expected = [
            "a3",
            "617604",         // v: 4
            "66636c69656e74", // client
            "50",
            &"bb".repeat(16),
            "657061636b73", // packs
            "81a4",
            "626964", // id
            "5820",
            &"aa".repeat(32),
            "646b696e6400",     // kind: 0
            "6473697a651864",   // size: 100
            "67656e7472696573", // entries
            "81a4",
            "61695820",
            &"cc".repeat(32),
            "616f08",
            "616c1829",
            "61720a",
        ]
        .concat();
        let bytes = d.encode().unwrap();
        assert_eq!(hex::encode(&bytes), expected);
        assert_eq!(Delta::decode(&bytes, &d.client).unwrap(), d);
    }

    #[test]
    fn a_delta_only_describes_its_own_prefix() {
        let c = ClientId::from_bytes([0xbb; 16]);
        let pack = |kind| DeltaPack {
            id: ObjectId::from_bytes([0xaa; 32]),
            kind,
            size: 100,
            entries: vec![entry(1)],
        };
        let d = Delta::new(c, vec![pack(Kind::Data), pack(Kind::Meta)]);
        let bytes = d.encode().unwrap();
        assert!(matches!(
            Delta::decode(&bytes, &ClientId::from_bytes([0xbc; 16])),
            Err(Error::Corrupt { .. })
        ));
        assert_eq!(
            d.pack_key(&d.packs[0]),
            Key::Pack {
                client: c,
                id: ObjectId::from_bytes([0xaa; 32])
            }
        );
        assert_eq!(
            d.pack_key(&d.packs[1]),
            Key::Mpack {
                client: c,
                id: ObjectId::from_bytes([0xaa; 32]),
                replica: false
            }
        );
        let mut d5 = d;
        d5.v = 5;
        assert!(matches!(d5.encode(), Err(Error::Invalid { .. })));
        assert!(matches!(
            Delta::decode(&raw(&d5), &c),
            Err(Error::Version { version: 5, .. })
        ));
    }
}
