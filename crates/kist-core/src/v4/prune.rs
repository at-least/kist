//! prune（§11.3 的 P0–P9），照 GC 模型（`crates/kist-gcmodel/src/prune.rs`）逐段實作。
//!
//! - **P0、P1**：[`Session`]（檔案鎖、探針、最大世代、補寫 `.r1`）；載入 `Map(g)`，算出這一輪的 age（drain
//!   還沒結束就是舊的 grace）與 hold 是否還在。
//! - **P2**：依序列出 `snapshots/`、`deltas/`、`packs/` 與 `mpacks/`（含 `.r1`）、`parity/`、`maint/map/`。
//! - **P3**：live 或 condemned 的記錄主體與 `.r1` 都不在 → 先重新列出世代（已有更新的 head 就只是輸了），沒有
//!   才拒絕；held 的、unreadable 與 unknown 的直接移除。
//! - **P4**：新進者讀 trailer：讀得了 → live，內容讀不了 → unreadable，版本不認得 → unknown（不使用、不刪除）。
//!   讀 trailer 時後端的錯誤（不是「不存在」）停止這一輪：一次逾時不能讓好的 pack 變成 unreadable。
//! - **P5**：走訪每一個 snapshot（主體，與只有 `.r1` 的）引用的每一個 metadata item 與 chunk。metadata item 照
//!   持有者的順序取回驗證：不實的項目從記錄移除（同 GC 模型）、換下一個；沒有可讀、非 tombstone、種類相符的
//!   持有者 → 先重新列出世代再拒絕。兩份都解不開的 snapshot 照 `bad` 清單處理；版本不認得的拒絕。
//! - **P6、P7**：正本與 `Map(g+1)`；P7d 的刪前驗證與兩個時鐘；P7c 的 repack 與修法 D（parity 在記憶體裡修復）。
//!   `--read-data`：其他 live 的資料 pack 也整份讀、逐項驗證（mpack 的項目 P5 走訪時已逐一取回驗證），壞而修得回
//!   的 repack、修不回的項目從記錄移除——平常的 prune 讀不到稠密、live 的資料 pack，位元損壞的 chunk 只有這樣
//!   才修得回（修法 C 的替代，ADR 026 第七段）。
//! - **P8**：寫出有變動的分片、時限內以柵欄寫 head g+1、再寫 `.r1`。
//! - **P9**：時限內刪除 tombstone 而仍在列表的 pack（`.r1`、parity、主體）、收錄完的 delta、過了 grace 的
//!   解不開的 delta、兩代都沒引用的舊分片、pack 已不在的 parity、`bad` 裡已不再當成起點的 snapshot。刪不掉的
//!   回報、記錄留在 tombstone，下一輪再試。
//!
//! 一律讀後端（[`Session`] 的 repo 不用本地快取）：刪什麼取決於讀到的內容。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use kist_backend::BackendError;
use kist_format::v4::ids::{ClientId, ItemId, MetaId, ObjectId};
use kist_format::v4::index::{status, Bad, Delta, Head, Record, Shard, Status, SHARDS};
use kist_format::v4::keys::{Gen, Key, DELTAS, MAP, MPACKS, PACKS, PARITY, SNAPSHOTS};
use kist_format::v4::meta::{content_type, decode_item, node_type, Entry, Item};
use kist_format::v4::pack::{Kind, PackEntry};
use kist_format::v4::snapshot::Snapshot;

use super::backup::{Clock, M};
use super::maint::{secs, unix_ns, window_passed, Newest, Pair, Session};
use super::pack::{open_chunk, open_meta, PackWriter};
use super::repo::Repo;
use super::state::{pack_key, Holder, Source};
use crate::{CoreError, Result};

/// prune 的選項。
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// P7c：live、是正本、比 grace 老的 pack，當正本的項目（sealed bytes）不到這個百分比就 repack；0 不依比例
    /// repack。以 parity 修復過的 pack 一律 repack（規格錯誤第 17 項的修法 D）。
    pub repack_below: u8,
    /// `--read-data`：每個 live、當正本的資料 pack 整份讀、逐項驗證（下載整個 repo 的資料）。壞而 parity 修得回的
    /// repack，修不回的項目從記錄移除並回報。時間用掉一半就停、剩下的留給下一次（整輪仍寫 head）。
    pub read_data: bool,
    /// 牆鐘與含休眠的單調時鐘（P0 的本機讀數、head 的 `w`、整輪時限）。測試注入假的。
    #[doc(hidden)]
    pub clock: Clock,
}

/// [`PruneOptions::repack_below`] 的預設（同 v3 的 prune）。
pub const DEFAULT_REPACK_BELOW: u8 = 50;

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            repack_below: DEFAULT_REPACK_BELOW,
            read_data: false,
            clock: Clock::system(),
        }
    }
}

/// 一輪 prune 的結果。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PruneReport {
    /// 這一輪的基礎世代與寫出的世代。
    pub base: Gen,
    pub generation: Gen,
    pub snapshots: u64,
    /// 被引用的 metadata item 與 chunk。
    pub items: u64,
    pub chunks: u64,
    /// 收錄的新進者（含 unreadable 與 unknown）。
    pub newcomers: u64,
    pub revived: u64,
    pub condemned: u64,
    pub tombstoned: u64,
    /// 移除的記錄（消失的 held、unreadable、unknown，與過了 grace 的 tombstone）。
    pub removed_records: u64,
    /// 從記錄移除的不實項目。
    pub false_entries: u64,
    /// repack 寫出、讀回驗證過的新 pack（P7c）。
    pub repacked: u64,
    /// 損壞、以 parity 在記憶體裡修復的 pack（它當正本的項目由 repack 寫進新的 pack）。
    pub repaired: u64,
    /// `--read-data` 整份讀過並逐項驗證的資料 pack 數（不含因比例被 repack 的）。
    pub read_packs: u64,
    /// 這一輪結束時 live 的記錄數（含這一輪收錄與 repack 寫出的）；kist-app 的 metrics 用。
    pub live: u64,
    pub deleted_packs: u64,
    pub deleted_bytes: u64,
    pub deleted_deltas: u64,
    pub deleted_shards: u64,
    pub deleted_parity: u64,
    pub deleted_snapshots: u64,
    /// 刪不掉的物件（Object Lock、權限）：pack 的記錄留在 tombstone，下一輪再試。
    pub undeleted: Vec<String>,
    /// 沒做完的原因（P9 超過整輪時限）：已寫的世代有效，剩下的刪除在下一輪。
    pub unfinished: Option<String>,
    /// 回報：不實的項目、版本不認得的 pack、只缺主體的 pack、只有 `.r1` 的 snapshot、被忽略的壞 snapshot、時間異常。
    pub warnings: Vec<String>,
}

/// P2 列出來的一個 pack 或 mpack：主體與 `.r1` 各自的 (大小, 修改時間)。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Listed {
    client: ClientId,
    kind: Kind,
    body: Option<(u64, i64)>,
    r1: Option<(u64, i64)>,
}

impl Listed {
    /// 主體的大小與修改時間；只有 `.r1` 時用它的。
    pub(crate) fn first(&self) -> (u64, i64) {
        self.body.or(self.r1).unwrap_or_default()
    }
}

/// P2 列出來的其他物件。
#[derive(Debug, Clone)]
struct Object {
    key: Key,
    mtime: i64,
}

/// 一個 item 在某個持有者那裡的副本（GC 模型的 `Copy`）。
enum Copy {
    /// 解得開、ID 相符；metadata item 帶著規範 CBOR。
    Genuine(Vec<u8>),
    /// 存著的副本壞了，但 pack 以 parity 修得回（重算 BLAKE3 等於名稱），修好的 bytes 裡這一項解得開。
    Repaired(Vec<u8>),
    /// pack 在，但這一項解不開、ID 不符，或位移超出範圍。
    False(String),
    /// pack 不在了。
    Missing,
}

/// P2 的 `packs/` 與 `mpacks/`（含 `.r1`）：名稱 → 列表裡的主體與 `.r1`。不是 v4 的 key、同一個名稱又出現在
/// 別的前綴下的略過並回報（第二個回傳值）。
pub(crate) async fn list_packs(
    backend: &kist_backend::Backend,
) -> Result<(BTreeMap<ObjectId, Listed>, Vec<String>)> {
    let mut packs: BTreeMap<ObjectId, Listed> = BTreeMap::new();
    let mut warnings = Vec::new();
    for prefix in [PACKS, MPACKS] {
        for o in backend.list(prefix.trim_end_matches('/')).await? {
            let (client, id, kind, replica) = match Key::parse(&o.key) {
                Ok(Key::Pack { client, id }) => (client, id, Kind::Data, false),
                Ok(Key::Mpack {
                    client,
                    id,
                    replica,
                }) => (client, id, Kind::Meta, replica),
                _ => {
                    warnings.push(format!("{}: not a v4 pack key; ignored", o.key));
                    continue;
                }
            };
            let e = packs.entry(id).or_insert(Listed {
                client,
                kind,
                body: None,
                r1: None,
            });
            if e.client != client || e.kind != kind {
                warnings.push(format!(
                    "{}: the same pack name is also listed under another prefix; ignored",
                    o.key
                ));
                continue;
            }
            let at = Some((o.size, unix_ns(o.modified)));
            if replica {
                e.r1 = at;
            } else {
                e.body = at;
            }
        }
    }
    Ok((packs, warnings))
}

/// P4（重建也用）：讀新進者的 trailer（主體，mpack 不行再讀 `.r1`）。讀得了 → live；內容讀不了 → unreadable；
/// 版本不認得 → unknown（不使用、不刪除，兩份之中有一份是就算）。讀的時候後端的錯誤（不是「不存在」）回錯誤：
/// 一次逾時不能讓好的 pack 變成 unreadable。列表之後就不見了回 `None`。第二個值是要回報的話。
pub(crate) async fn newcomer(
    repo: &Repo,
    id: ObjectId,
    l: &Listed,
    since: Gen,
) -> Result<Option<(Record, Option<String>)>> {
    let mut copies = Vec::new();
    if let Some((size, _)) = l.body {
        copies.push((pack_key(l.kind, l.client, id), size));
    }
    if let (Some((size, _)), Kind::Meta) = (l.r1, l.kind) {
        copies.push((
            Key::Mpack {
                client: l.client,
                id,
                replica: true,
            },
            size,
        ));
    }
    let mut status_ = None;
    let mut entries = Vec::new();
    let mut reasons = Vec::new();
    for (key, size) in copies {
        match repo.trailer_sized(&key, size).await {
            Ok(t) => {
                entries = t.entries;
                status_ = Some(Status::LIVE);
                break;
            }
            Err(CoreError::FormatV4(e @ kist_format::v4::Error::Version { .. })) => {
                reasons.push(e.to_string());
                status_ = Some(Status {
                    s: status::UNKNOWN,
                    at: 0,
                    age: 0,
                });
            }
            Err(e) if not_found(&e) => {}
            Err(e @ CoreError::Backend(_)) => return Err(e),
            Err(e) => {
                reasons.push(e.to_string());
                if status_.is_none() {
                    status_ = Some(Status {
                        s: status::UNREADABLE,
                        at: 0,
                        age: 0,
                    });
                }
            }
        }
    }
    let Some(st) = status_ else {
        return Ok(None);
    };
    let key = pack_key(l.kind, l.client, id).path();
    let warning = match st.s {
        status::UNKNOWN => Some(format!(
            "{key}: unknown trailer version ({}); kept, not used, not deleted",
            reasons.join("; ")
        )),
        status::UNREADABLE => Some(format!(
            "{key}: unreadable ({}); nobody uses it, a later prune deletes it",
            reasons.join("; ")
        )),
        _ => None,
    };
    Ok(Some((
        Record {
            id,
            client: l.client,
            kind: l.kind,
            size: l.first().0,
            entries,
            by_prune: false,
            since,
            status: st,
            held: false,
        },
        warning,
    )))
}

/// 分片 i 的記錄：名稱第一個 byte 等於 i 的那些——map 依名稱排序，是連續的一段。
fn shard_range(i: u8) -> std::ops::RangeInclusive<ObjectId> {
    let mut lo = [0u8; 32];
    lo[0] = i;
    let mut hi = [0xffu8; 32];
    hi[0] = i;
    ObjectId::from_bytes(lo)..=ObjectId::from_bytes(hi)
}

/// 每一片明文的 BLAKE3（空的是 `None`）：P8 以它判斷分片有沒有改變，不必留一份基礎世代的記錄集合。
pub(crate) fn shard_hashes(map: &BTreeMap<ObjectId, Record>) -> Result<Vec<Option<ObjectId>>> {
    let mut out = Vec::with_capacity(SHARDS);
    for i in 0..SHARDS {
        let recs: Vec<&Record> = map.range(shard_range(i as u8)).map(|(_, r)| r).collect();
        out.push(if recs.is_empty() {
            None
        } else {
            Some(ObjectId::of(&Shard::encode_borrowed(i as u8, &recs)?))
        });
    }
    Ok(out)
}

/// P8：明文與基礎世代不同的分片才寫（`replicas=1` 時先主體、再 `.r1`），名稱填進 `head`；變空的分片是 null。
pub(crate) async fn put_changed_shards(
    repo: &Repo,
    base: &[Option<ObjectId>],
    new: &BTreeMap<ObjectId, Record>,
    head: &mut Head,
) -> Result<()> {
    for (i, base_hash) in base.iter().enumerate() {
        let recs: Vec<&Record> = new.range(shard_range(i as u8)).map(|(_, r)| r).collect();
        if recs.is_empty() {
            head.shards[i] = None;
            continue;
        }
        let plain = Shard::encode_borrowed(i as u8, &recs)?;
        if *base_hash == Some(ObjectId::of(&plain)) {
            continue;
        }
        let sealed = repo.keys().seal_shard(&plain)?;
        let id = ObjectId::of(&sealed);
        for replica in [false, true] {
            if replica && head.settings.replicas == 0 {
                continue;
            }
            match repo
                .backend()
                .put_if_absent(&Key::Shard { id, replica }.path(), sealed.clone())
                .await
            {
                Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        head.shards[i] = Some(id);
    }
    Ok(())
}

fn not_found(e: &CoreError) -> bool {
    matches!(e, CoreError::Backend(BackendError::NotFound(_)))
}

/// 一個 entry 引用的 item：目錄的 tree、檔案的 chunk（直接）或 chunk 清單（間接）。
fn entry_refs(e: &Entry, out: &mut Vec<(ItemId, Kind)>) {
    match e.t {
        node_type::DIR => out.extend(e.tree.map(|t| (t.into(), Kind::Meta))),
        node_type::FILE if e.ct == content_type::INDIRECT => {
            out.extend(e.chunks.iter().map(|c| (*c, Kind::Meta)))
        }
        node_type::FILE => out.extend(e.chunks.iter().map(|c| (*c, Kind::Data))),
        _ => {}
    }
}

fn condemned(at: Gen, age: u64) -> Status {
    Status {
        s: status::CONDEMNED,
        at,
        age,
    }
}

fn tombstone(at: Gen) -> Status {
    Status {
        s: status::TOMBSTONE,
        at,
        age: 0,
    }
}

/// 記憶體裡的 mpack 副本（主體或 `.r1` 的整份 bytes），超過上限時丟掉最久沒用的。
struct MpackCache {
    cap: usize,
    used: usize,
    order: VecDeque<(ObjectId, bool)>,
    map: HashMap<(ObjectId, bool), Arc<Vec<u8>>>,
}

impl MpackCache {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            used: 0,
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    fn get(&self, k: &(ObjectId, bool)) -> Option<Arc<Vec<u8>>> {
        self.map.get(k).cloned()
    }

    fn insert(&mut self, k: (ObjectId, bool), bytes: Arc<Vec<u8>>) {
        self.used += bytes.len();
        self.map.insert(k, bytes);
        self.order.push_back(k);
        while self.used > self.cap && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front() {
                if let Some(b) = self.map.remove(&old) {
                    self.used -= b.len();
                }
            }
        }
    }
}

/// P5–P7 的索引（第 7 段第十二步）：P4 之後的記錄裡每一個項目一列，依 (ID, §10 的持有者順序) 排好；記錄本身
/// 只有 [`Prune::map`] 一份，列以 pack 的名稱（`packs[ri]`）與項目的位置（`ei`）指回去。三張依 (記錄, 項目)
/// 索引的表：`dead`（證明不實的項目；P8 之前才真的從記錄移除，索引才一直有效）、`referenced`（P5 走到、種類
/// 相符）、`canon`（P6 的正本）。100 萬個項目：列 40 MiB、三張表各 1 MiB。原本以 32 bytes 的 ID 當鍵的
/// `referenced` 與 `canon` 兩張 HashMap 在 dhat 裡是 263 MiB，加上每個階段各 clone 一份記錄集合與 ID 的表
/// （第八步的歸因）。
struct Index {
    packs: Vec<ObjectId>,
    rows: Vec<Row>,
    dead: Vec<Vec<bool>>,
    referenced: Vec<Vec<bool>>,
    canon: Vec<Vec<bool>>,
    /// 有項目被當成正本的記錄。
    canonical_pack: Vec<bool>,
    /// 被引用的 metadata item 與 chunk 的數目（報告用）。
    items: u64,
    chunks: u64,
}

#[derive(Clone, Copy)]
struct Row {
    item: ItemId,
    ri: u32,
    ei: u32,
}

impl Index {
    fn new(map: &BTreeMap<ObjectId, Record>) -> Self {
        let packs: Vec<ObjectId> = map.keys().copied().collect();
        let keys: Vec<Option<(u8, Gen, ObjectId)>> = map.values().map(Record::order_key).collect();
        let mut rows = Vec::with_capacity(map.values().map(|r| r.entries.len()).sum());
        let mut flags = Vec::with_capacity(packs.len());
        for (ri, r) in map.values().enumerate() {
            flags.push(vec![false; r.entries.len()]);
            if keys[ri].is_none() {
                continue;
            }
            rows.extend(r.entries.iter().enumerate().map(|(ei, e)| Row {
                item: e.i,
                ri: ri as u32,
                ei: ei as u32,
            }));
        }
        rows.sort_unstable_by(|a, b| {
            a.item
                .cmp(&b.item)
                .then_with(|| keys[a.ri as usize].cmp(&keys[b.ri as usize]))
        });
        Self {
            canonical_pack: vec![false; packs.len()],
            packs,
            rows,
            dead: flags.clone(),
            referenced: flags.clone(),
            canon: flags,
            items: 0,
            chunks: 0,
        }
    }

    /// 一個 ID 的列（連續的一段）。
    fn find(&self, item: &ItemId) -> std::ops::Range<usize> {
        let start = self.rows.partition_point(|r| r.item < *item);
        let end = start + self.rows[start..].partition_point(|r| r.item == *item);
        start..end
    }

    fn ri(&self, pack: &ObjectId) -> Option<usize> {
        self.packs.binary_search(pack).ok()
    }

    fn is_canonical_pack(&self, pack: &ObjectId) -> bool {
        self.ri(pack).is_some_and(|ri| self.canonical_pack[ri])
    }

    /// `pack` 的每一項是不是被引用（種類相符）：與記錄的 `entries` 同樣的位置。索引建好之後才進 map 的記錄
    /// （repack 寫出的）沒有。
    fn referenced_in(&self, pack: &ObjectId) -> Option<&[bool]> {
        self.ri(pack).map(|ri| self.referenced[ri].as_slice())
    }

    /// `pack` 的每一項是不是正本。
    fn canon_in(&self, pack: &ObjectId) -> Option<&[bool]> {
        self.ri(pack).map(|ri| self.canon[ri].as_slice())
    }

    /// 一個 ID 的持有者（P5、P7d）：記錄裡種類相符、live 或 condemned、還在列表裡、不是不實的項目，依 §10
    /// 的順序。
    fn holders(
        &self,
        map: &BTreeMap<ObjectId, Record>,
        listed: &BTreeMap<ObjectId, Listed>,
        item: &ItemId,
        kind: Kind,
    ) -> Vec<Holder> {
        let mut out = Vec::new();
        for row in &self.rows[self.find(item)] {
            let (ri, ei) = (row.ri as usize, row.ei as usize);
            if self.dead[ri][ei] {
                continue;
            }
            let Some(r) = map.get(&self.packs[ri]) else {
                continue;
            };
            if r.kind != kind
                || !matches!(r.status.s, status::LIVE | status::CONDEMNED)
                || !listed.contains_key(&r.id)
            {
                continue;
            }
            out.push(Holder {
                pack: r.id,
                client: r.client,
                kind: r.kind,
                entry: r.entries[ei],
                source: Source::Map,
                by_prune: r.by_prune,
                status: r.status.s,
            });
        }
        out
    }

    /// P5：記下 (ID, 種類) 被引用。`None`：沒有任何記錄有它（呼叫端拒絕）；`Some(false)`：已經記過；
    /// `Some(true)`：這次記的。
    fn mark_referenced(
        &mut self,
        map: &BTreeMap<ObjectId, Record>,
        item: &ItemId,
        kind: Kind,
    ) -> Option<bool> {
        let mut any = false;
        for i in self.find(item) {
            let row = self.rows[i];
            let (ri, ei) = (row.ri as usize, row.ei as usize);
            if !matches!(map.get(&self.packs[ri]), Some(r) if r.kind == kind) {
                continue;
            }
            if self.referenced[ri][ei] {
                return Some(false);
            }
            self.referenced[ri][ei] = true;
            any = true;
        }
        if !any {
            return None;
        }
        match kind {
            Kind::Meta => self.items += 1,
            Kind::Data => self.chunks += 1,
        }
        Some(true)
    }

    /// P6：每個被引用的 (ID, 種類) 的正本——依 §10 的順序第一個 live 或 condemned、還在列表裡、不是不實的持有者。
    fn compute_canon(
        &mut self,
        map: &BTreeMap<ObjectId, Record>,
        listed: &BTreeMap<ObjectId, Listed>,
    ) {
        for v in &mut self.canon {
            v.iter_mut().for_each(|b| *b = false);
        }
        self.canonical_pack.iter_mut().for_each(|b| *b = false);
        let mut i = 0;
        while i < self.rows.len() {
            let item = self.rows[i].item;
            let end = i + self.rows[i..].partition_point(|r| r.item == item);
            let mut done = [false; 2];
            for row in &self.rows[i..end] {
                let (ri, ei) = (row.ri as usize, row.ei as usize);
                if self.dead[ri][ei] || !self.referenced[ri][ei] {
                    continue;
                }
                let Some(r) = map.get(&self.packs[ri]) else {
                    continue;
                };
                let k = usize::from(r.kind == Kind::Data);
                if done[k]
                    || !matches!(r.status.s, status::LIVE | status::CONDEMNED)
                    || !listed.contains_key(&r.id)
                {
                    continue;
                }
                self.canon[ri][ei] = true;
                self.canonical_pack[ri] = true;
                done[k] = true;
            }
            i = end;
        }
    }

    /// (ID, 種類) 的正本：記錄的名稱與那一項的位置。
    fn canon_of(
        &self,
        map: &BTreeMap<ObjectId, Record>,
        item: &ItemId,
        kind: Kind,
    ) -> Option<(ObjectId, usize)> {
        self.rows[self.find(item)].iter().find_map(|row| {
            let (ri, ei) = (row.ri as usize, row.ei as usize);
            if !self.canon[ri][ei] {
                return None;
            }
            let r = map.get(&self.packs[ri])?;
            (r.kind == kind).then_some((r.id, ei))
        })
    }

    /// 項目證明不實：標成 dead。回傳這次標的數目（已經標過的不算）。
    fn kill(&mut self, pack: &ObjectId, item: &ItemId) -> usize {
        let Some(ri) = self.ri(pack) else {
            return 0;
        };
        let mut n = 0;
        for i in self.find(item) {
            let row = self.rows[i];
            if row.ri as usize == ri && !self.dead[ri][row.ei as usize] {
                self.dead[ri][row.ei as usize] = true;
                n += 1;
            }
        }
        n
    }
}

/// P5 的拒絕：一個被引用的 item 沒有可用的持有者。
fn no_holder(kind: Kind, item: &ItemId, snap: &str) -> String {
    match kind {
        Kind::Meta => format!(
            "metadata item {} (snapshot {snap}) has no readable holder that is not a tombstone: \
             data loss; run check",
            item.to_meta()
        ),
        Kind::Data => format!(
            "chunk {item} (snapshot {snap}) has no readable holder that is not a tombstone: data \
             loss; run check"
        ),
    }
}

struct Prune {
    session: Session,
    newest: Newest,
    g: Gen,
    head: Head,
    grace: u64,
    /// 新的 condemned 記錄的 age：drain 還沒結束時是舊的 grace。
    age: u64,
    drain_over: bool,
    hold_active: bool,
    /// 「已過一個 grace」的現在（兩個時鐘，§11.1）。
    now: Pair,
    /// 只看 `T` 的規則用的現在：`T(g)`，比本機時鐘晚超過 `M` 時是本機時鐘 + M。
    n_t: i64,
    /// 基礎世代每一片明文的 BLAKE3（P8 以它判斷分片有沒有改變；不留第二份記錄集合）。
    base_hashes: Vec<Option<ObjectId>>,
    /// 唯一的一份記錄：載入的 `Map(g)`，P3、P4、P7、P7c 直接在上面改成 `Map(g+1)`。
    map: BTreeMap<ObjectId, Record>,
    /// P5 建的索引（P5–P7c 用）。
    index: Option<Index>,
    snaps: BTreeMap<String, (bool, bool)>,
    deltas: Vec<(Object, Option<Delta>)>,
    packs: BTreeMap<ObjectId, Listed>,
    parity: Vec<Object>,
    shards: Vec<Object>,
    /// 證明不實的 (ID, pack)：索引裡標成 dead，P8 之前從記錄移除。
    false_claims: HashSet<(ItemId, ObjectId)>,
    bad_ignored: Vec<String>,
    mpacks: MpackCache,
    repack_below: u8,
    read_data: bool,
    /// 以 parity 修復過的 pack 的完整 bytes（P7c 從它 repack）與修不回的 pack。
    repaired: HashMap<ObjectId, Arc<Vec<u8>>>,
    unrepairable: HashSet<ObjectId>,
    report: PruneReport,
}

impl Repo {
    /// prune（§11.3）。見模組說明。
    pub async fn prune(&self, opts: PruneOptions) -> Result<PruneReport> {
        let mut session = Session::begin(self, opts.clock).await?;
        // P1
        let newest = session.newest().await?;
        session.repair_r1(&newest).await?;
        let g = newest.g();
        let head = newest.head().clone();
        let records = session.repo.load_map_records(&newest.copy.gen).await?;
        let now = session.now(&newest);
        let grace = head.settings.grace;
        let mut age = grace;
        let mut drain_over = false;
        if let Some(d) = head.drain {
            let from = session.times(d.from).await?;
            drain_over = window_passed(from, now, d.old_grace);
            if !drain_over {
                age = d.old_grace;
            }
        }
        let mut hold_active = false;
        if let Some(h) = head.hold {
            // 遺失的世代裡最晚的 T 與最晚的 w，各自加一個 grace（§11.4）
            let mut latest = Pair {
                t: i64::MIN,
                w: i64::MIN,
            };
            for lost in h.from..=h.lost {
                let p = session.times(lost).await?;
                latest = Pair {
                    t: latest.t.max(p.t),
                    w: latest.w.max(p.w),
                };
            }
            hold_active = !window_passed(latest, now, h.grace);
        }
        let map: BTreeMap<ObjectId, Record> = records.into_iter().map(|r| (r.id, r)).collect();
        let base_hashes = shard_hashes(&map)?;
        let mut p = Prune {
            n_t: newest.now_t,
            session,
            newest,
            g,
            head,
            grace,
            age,
            drain_over,
            hold_active,
            now,
            base_hashes,
            map,
            index: None,
            snaps: BTreeMap::new(),
            deltas: Vec::new(),
            packs: BTreeMap::new(),
            parity: Vec::new(),
            shards: Vec::new(),
            false_claims: HashSet::new(),
            bad_ignored: Vec::new(),
            mpacks: MpackCache::new(super::read::MPACK_MEMORY),
            repack_below: opts.repack_below,
            read_data: opts.read_data,
            repaired: HashMap::new(),
            unrepairable: HashSet::new(),
            report: PruneReport {
                base: g,
                ..Default::default()
            },
        };
        if let Some(w) = p.newest.ahead.clone() {
            p.report.warnings.push(w);
        }
        p.list().await?;
        p.missing().await?;
        p.newcomers().await?;
        p.walk_all().await?;
        let mut new_head = p.plan().await?;
        p.repack().await?;
        p.report.live = p
            .map
            .values()
            .filter(|r| r.status.s == status::LIVE)
            .count() as u64;
        p.drop_false_entries();
        p.put_shards(&mut new_head).await?;
        if p.session.out_of_time(p.grace) {
            return Err(CoreError::Unsafe(format!(
                "this prune has run for {}s, past the GC grace ({}s) minus one hour; not writing \
                 the next generation",
                p.session.elapsed().as_secs(),
                p.grace
            )));
        }
        let written = p.session.fence(&p.newest, new_head.clone()).await?;
        p.report.generation = written;
        p.delete(&new_head).await?;
        Ok(p.report)
    }
}

impl Prune {
    fn warn(&mut self, msg: String) {
        tracing::warn!("{msg}");
        self.report.warnings.push(msg);
    }

    /// 拒絕之前重新列出 `maint/gen/`：已經有比基礎世代新的 head，就只是輸了（P3、P5）。
    async fn refuse(&self, msg: String) -> CoreError {
        match self.session.repo.list_heads(Some(self.g)).await {
            Ok(listing) => match listing.keys().next_back() {
                Some(&newest) if newest > self.g => CoreError::Superseded(format!(
                    "generation {newest} was written while this prune ran on generation {}",
                    self.g
                )),
                _ => CoreError::Unsafe(msg),
            },
            Err(e) => e,
        }
    }

    /// P2：依序列出（先列的物件寫在它所依賴的物件之後，那些物件一定在後列的清單裡）。
    async fn list(&mut self) -> Result<()> {
        let backend = self.session.repo.backend().clone();
        for o in backend.list(SNAPSHOTS.trim_end_matches('/')).await? {
            match Key::parse(&o.key) {
                Ok(k @ Key::Snapshot { replica, .. }) => {
                    let e = self.snaps.entry(k.primary().path()).or_default();
                    if replica {
                        e.1 = true;
                    } else {
                        e.0 = true;
                    }
                }
                _ => self.warn(format!("{}: not a v4 snapshot key; ignored", o.key)),
            }
        }
        for o in backend.list(DELTAS.trim_end_matches('/')).await? {
            let key = match Key::parse(&o.key) {
                Ok(k @ Key::Delta { .. }) => k,
                _ => {
                    self.warn(format!("{}: not a v4 delta key; ignored", o.key));
                    continue;
                }
            };
            let Key::Delta { client, id } = key else {
                continue;
            };
            let delta = async {
                let bytes = backend.get(&o.key).await?;
                if ObjectId::of(&bytes) != id {
                    return Err(CoreError::corrupt(
                        &o.key,
                        "content does not match its name",
                    ));
                }
                let plain = self
                    .session
                    .repo
                    .keys()
                    .open_delta(&client, &bytes)
                    .map_err(|e| CoreError::corrupt(&o.key, e.to_string()))?;
                Ok(Delta::decode(&plain, &client)?)
            }
            .await;
            let delta = match delta {
                Ok(d) => Some(d),
                Err(e) if not_found(&e) => continue,
                Err(e @ CoreError::Backend(_)) => return Err(e),
                Err(e) => {
                    self.warn(format!("{}: delta cannot be read: {e}", o.key));
                    None
                }
            };
            self.deltas.push((
                Object {
                    key,
                    mtime: unix_ns(o.modified),
                },
                delta,
            ));
        }
        let (packs, warnings) = list_packs(&backend).await?;
        self.packs = packs;
        for w in warnings {
            self.warn(w);
        }
        for o in backend.list(PARITY.trim_end_matches('/')).await? {
            match Key::parse(&o.key) {
                Ok(key @ Key::Parity { .. }) => self.parity.push(Object {
                    key,
                    mtime: unix_ns(o.modified),
                }),
                _ => self.warn(format!("{}: not a v4 parity key; ignored", o.key)),
            }
        }
        for o in backend.list(MAP.trim_end_matches('/')).await? {
            match Key::parse(&o.key) {
                Ok(key @ Key::Shard { .. }) => self.shards.push(Object {
                    key,
                    mtime: unix_ns(o.modified),
                }),
                _ => self.warn(format!("{}: not a v4 map shard key; ignored", o.key)),
            }
        }
        Ok(())
    }

    /// P3：記錄裡有、列表裡主體與 `.r1` 都沒有的 pack。
    async fn missing(&mut self) -> Result<()> {
        let mut dropped = Vec::new();
        for (id, rec) in &self.map {
            let listed = self.packs.get(id);
            if let Some(l) = listed {
                if l.body.is_none() {
                    let msg = format!(
                        "{}: the body is missing, its .r1 is there; run check --repair",
                        pack_key(rec.kind, rec.client, *id).path()
                    );
                    tracing::warn!("{msg}");
                    self.report.warnings.push(msg);
                }
                continue;
            }
            match rec.status.s {
                status::LIVE | status::CONDEMNED if rec.held => dropped.push(*id),
                status::LIVE | status::CONDEMNED => {
                    let msg = format!(
                        "pack {} ({}) is in the map but missing from the repository: data loss; \
                         run check, and repair or forget the snapshots that need it",
                        pack_key(rec.kind, rec.client, *id).path(),
                        if rec.status.s == status::LIVE {
                            "live"
                        } else {
                            "condemned"
                        }
                    );
                    return Err(self.refuse(msg).await);
                }
                status::UNREADABLE | status::UNKNOWN => dropped.push(*id),
                _ => {}
            }
        }
        for id in dropped {
            self.map.remove(&id);
            self.report.removed_records += 1;
        }
        Ok(())
    }

    /// P4：列表裡有、map 裡沒有的 pack。
    async fn newcomers(&mut self) -> Result<()> {
        let next = self.g + 1;
        let fresh: Vec<(ObjectId, Listed)> = self
            .packs
            .iter()
            .filter(|(id, _)| !self.map.contains_key(*id))
            .map(|(id, l)| (*id, *l))
            .collect();
        for (id, l) in fresh {
            let Some((rec, warning)) = newcomer(&self.session.repo, id, &l, next).await? else {
                continue;
            };
            if let Some(w) = warning {
                self.warn(w);
            }
            self.map.insert(id, rec);
            self.report.newcomers += 1;
        }
        Ok(())
    }

    /// P5：每一個 snapshot 當成起點。持有者的索引在走訪之前建一次（P3、P4 之後的記錄）；之後發現的不實項目在
    /// 索引裡標成 dead。
    async fn walk_all(&mut self) -> Result<()> {
        let mut index = Index::new(&self.map);
        let result = self.walk_snapshots(&mut index).await;
        self.report.items = index.items;
        self.report.chunks = index.chunks;
        self.index = Some(index);
        result
    }

    async fn walk_snapshots(&mut self, index: &mut Index) -> Result<()> {
        let snaps: Vec<(String, (bool, bool))> =
            self.snaps.iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (path, (body, r1)) in snaps {
            let key = Key::parse(&path)?;
            let Key::Snapshot { client, ts, .. } = &key else {
                continue;
            };
            if !body && r1 {
                self.warn(format!(
                    "{path}: only its .r1 is there (an interrupted commit, or a lost body); \
                     walked as a snapshot"
                ));
            }
            let mut readable: Option<Snapshot> = None;
            let mut version = None;
            let mut present = false;
            for replica in [false, true] {
                let copy = Key::Snapshot {
                    client: *client,
                    ts: ts.clone(),
                    replica,
                };
                match self.session.repo.read_snapshot_copy(&copy).await {
                    Ok(s) => {
                        readable = Some(s);
                        break;
                    }
                    Err(e) if not_found(&e) => {}
                    Err(e @ CoreError::Backend(_)) => return Err(e),
                    Err(CoreError::FormatV4(e @ kist_format::v4::Error::Version { .. })) => {
                        present = true;
                        version = Some(e.to_string());
                    }
                    Err(_) => present = true,
                }
            }
            if let Some(s) = readable {
                self.report.snapshots += 1;
                self.walk(index, &path, s.roots).await?;
                continue;
            }
            if let Some(v) = version {
                return Err(CoreError::Unsafe(format!(
                    "snapshot {path} has {v}: a newer kist wrote it; prune with that version"
                )));
            }
            if !present {
                // 列表之後兩份都消失：視為已 forget
                continue;
            }
            self.bad_snapshot(&path).await?;
        }
        Ok(())
    }

    /// P5：兩份都解不開的 snapshot。已在 `bad` 裡滿一個 grace（兩個時鐘，比 P0 的讀數）→ 不再當成起點、回報、
    /// P9 刪它；還不到 → 停止；還不在 `bad` 裡 → 以柵欄寫 head g+1 記進去（分片不變），然後停止。
    async fn bad_snapshot(&mut self, path: &str) -> Result<()> {
        if let Some(b) = self.head.bad.iter().find(|b| b.key == path).cloned() {
            let first = self.session.times(b.first).await?;
            // 比 P0 的讀數，不比 T(g)：等待期間每一輪都停在這裡、不寫 head，T(g) 不會前進
            if window_passed(first, self.session.probe, self.grace) {
                self.warn(format!(
                    "snapshot {path}: neither copy can be decrypted; recorded at generation {} \
                     more than a grace ago, so it is ignored and deleted",
                    b.first
                ));
                self.bad_ignored.push(path.to_owned());
                return Ok(());
            }
            return Err(CoreError::Unsafe(format!(
                "snapshot {path}: neither copy can be decrypted (recorded at generation {}); prune \
                 refuses until one grace has passed since then, or until `forget {path}`",
                b.first
            )));
        }
        let mut head = self.head.clone();
        head.bad.push(Bad {
            key: path.to_owned(),
            first: self.g + 1,
        });
        let g = self.session.fence(&self.newest, head).await?;
        Err(CoreError::Unsafe(format!(
            "snapshot {path}: neither copy can be decrypted; recorded at generation {g}; prune \
             refuses until one grace has passed, then ignores and deletes it (or run `forget {path}` \
             now)"
        )))
    }

    /// P5：走訪一個 snapshot 引用的每一個 item（每個 (ID, 種類) 一次）。
    async fn walk(&mut self, index: &mut Index, snap: &str, roots: MetaId) -> Result<()> {
        let mut stack: Vec<MetaId> = Vec::new();
        match index.mark_referenced(&self.map, &roots.into(), Kind::Meta) {
            Some(true) => stack.push(roots),
            Some(false) => {}
            None => {
                return Err(self
                    .refuse(no_holder(Kind::Meta, &roots.into(), snap))
                    .await)
            }
        }
        while let Some(id) = stack.pop() {
            let item: ItemId = id.into();
            let holders = index.holders(&self.map, &self.packs, &item, Kind::Meta);
            if holders.is_empty() {
                return Err(self.refuse(no_holder(Kind::Meta, &item, snap)).await);
            }
            let mut cbor = None;
            for h in &holders {
                match self.fetch(h).await? {
                    Copy::Genuine(c) | Copy::Repaired(c) => {
                        cbor = Some(c);
                        break;
                    }
                    Copy::False(why) => self.remove_false(
                        index,
                        item,
                        h.pack,
                        &h.key(),
                        &format!("false ({why})"),
                        "",
                    ),
                    Copy::Missing => {}
                }
            }
            let Some(cbor) = cbor else {
                let msg = format!(
                    "no holder of metadata item {id} (snapshot {snap}) has a readable copy: data \
                     loss; run check"
                );
                return Err(self.refuse(msg).await);
            };
            let parsed = decode_item(&cbor).map_err(|e| {
                CoreError::Unsafe(format!(
                    "metadata item {id} (snapshot {snap}) is genuine but invalid ({e}); prune \
                     cannot tell what it references; forget the snapshot"
                ))
            })?;
            let mut children: Vec<(ItemId, Kind)> = Vec::new();
            match &parsed {
                Item::Seg(s) => s.entries.iter().for_each(|e| entry_refs(e, &mut children)),
                Item::Dir(d) => children.extend(d.segs.iter().map(|s| (s.id.into(), Kind::Meta))),
                Item::ChunkList(l) => {
                    children.extend(l.chunks.iter().map(|c| ((*c).into(), Kind::Data)))
                }
                Item::ChunkIndex(x) => {
                    children.extend(x.parts.iter().map(|p| (p.id.into(), Kind::Meta)))
                }
                Item::RootSeg(r) => {
                    for root in &r.roots {
                        children.extend(root.tree.map(|t| (t.into(), Kind::Meta)));
                        if let Some(e) = &root.entry {
                            entry_refs(e, &mut children);
                        }
                    }
                }
            }
            for (child, kind) in children {
                match index.mark_referenced(&self.map, &child, kind) {
                    Some(false) => {}
                    Some(true) => match kind {
                        Kind::Meta => stack.push(child.to_meta()),
                        Kind::Data => {
                            if index
                                .holders(&self.map, &self.packs, &child, Kind::Data)
                                .is_empty()
                            {
                                return Err(self.refuse(no_holder(kind, &child, snap)).await);
                            }
                        }
                    },
                    None => return Err(self.refuse(no_holder(kind, &child, snap)).await),
                }
            }
        }
        Ok(())
    }

    /// 不實的項目：在索引裡標成 dead（P8 之前才真的從記錄移除）並回報（P5、P7c、P7d、`--read-data`）。
    fn remove_false(
        &mut self,
        index: &mut Index,
        id: ItemId,
        pack: ObjectId,
        key: &Key,
        because: &str,
        stage: &str,
    ) {
        if index.kill(&pack, &id) > 0 {
            self.report.false_entries += 1;
        }
        self.false_claims.insert((id, pack));
        self.warn(format!(
            "{} claims {id} but its copy is {because}; removed from the record{stage}",
            key.path()
        ));
    }

    /// P8 之前：證明不實的項目真的從記錄移除。走訪與 P7 期間記錄不動，索引（以項目的位置指回記錄）才一直有效。
    fn drop_false_entries(&mut self) {
        if self.false_claims.is_empty() {
            return;
        }
        for (id, r) in &mut self.map {
            r.entries
                .retain(|e| !self.false_claims.contains(&(e.i, *id)));
        }
    }

    /// 從一份 pack 的完整 bytes 打開一項（解密、解壓、長度、重算 ID）：chunk 回明文，metadata item 回規範 CBOR。
    fn open_in(
        &self,
        kind: Kind,
        e: &PackEntry,
        bytes: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        let sealed =
            e.o.checked_add(e.l)
                .and_then(|end| usize::try_from(e.o).ok().zip(usize::try_from(end).ok()))
                .and_then(|(s, t)| bytes.get(s..t))
                .ok_or_else(|| format!("entry {}+{} is outside the pack", e.o, e.l))?;
        self.open_sealed(kind, e, sealed)
    }

    fn open_sealed(
        &self,
        kind: Kind,
        e: &PackEntry,
        sealed: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        let repo = &self.session.repo;
        match kind {
            Kind::Data => {
                let max = u64::from(repo.config().chunker.max);
                open_chunk(repo.keys(), &e.i.to_chunk(), sealed, e.r, max)
            }
            Kind::Meta => open_meta(repo.keys(), &e.i.to_meta(), sealed, e.r),
        }
        .map_err(|err| err.to_string())
    }

    /// 取回一個持有者裡的 item 並驗證。資料 pack 以 range 讀那一項；mpack 整份讀（先主體、再 `.r1`）。每一份都
    /// 驗證失敗時，以 parity 在記憶體裡修復整個 pack 再試（[`Prune::repair`]）。
    async fn fetch(&mut self, h: &Holder) -> Result<Copy> {
        let e = h.entry;
        let Some(end) = e.o.checked_add(e.l) else {
            return Ok(Copy::False(format!("entry {}+{} overflows", e.o, e.l)));
        };
        let repo = self.session.repo.clone();
        let mut reasons = Vec::new();
        match h.kind {
            Kind::Data => {
                let key = h.key().path();
                match repo.backend().get_range(&key, e.o..end).await {
                    Ok(sealed) => match self.open_sealed(Kind::Data, &e, &sealed) {
                        Ok(plain) => return Ok(Copy::Genuine(plain)),
                        Err(why) => reasons.push(why),
                    },
                    Err(BackendError::NotFound(_)) => return Ok(Copy::Missing),
                    Err(err) => return Err(err.into()),
                }
            }
            Kind::Meta => {
                let listed = self.packs.get(&h.pack).copied();
                for replica in [false, true] {
                    let present = listed.is_some_and(|l| {
                        if replica {
                            l.r1.is_some()
                        } else {
                            l.body.is_some()
                        }
                    });
                    if !present {
                        continue;
                    }
                    let bytes = match self.mpacks.get(&(h.pack, replica)) {
                        Some(b) => b,
                        None => {
                            let key = Key::Mpack {
                                client: h.client,
                                id: h.pack,
                                replica,
                            };
                            match repo.backend().get(&key.path()).await {
                                Ok(b) => {
                                    let b = Arc::new(b);
                                    self.mpacks.insert((h.pack, replica), Arc::clone(&b));
                                    b
                                }
                                Err(BackendError::NotFound(_)) => continue,
                                Err(err) => return Err(err.into()),
                            }
                        }
                    };
                    match self.open_in(Kind::Meta, &e, &bytes) {
                        Ok(cbor) => return Ok(Copy::Genuine(cbor)),
                        Err(why) => reasons.push(why),
                    }
                }
                if reasons.is_empty() {
                    return Ok(Copy::Missing);
                }
            }
        }
        if let Some(bytes) = self.repair(h.pack, h.client, h.kind).await? {
            if let Ok(plain) = self.open_in(h.kind, &e, &bytes) {
                return Ok(Copy::Repaired(plain));
            }
        }
        Ok(Copy::False(reasons.join("; ")))
    }

    /// 整份讀一個 pack：主體，不在就 `.r1`（mpack）。不驗證名稱；都不在就是 `None`。
    async fn whole(&self, id: ObjectId, client: ClientId, kind: Kind) -> Result<Option<Vec<u8>>> {
        let listed = self.packs.get(&id).copied();
        for replica in [false, true] {
            let present = listed.is_some_and(|l| {
                if replica {
                    l.r1.is_some()
                } else {
                    l.body.is_some()
                }
            });
            if !present || (replica && kind == Kind::Data) {
                continue;
            }
            let key = match (kind, replica) {
                (Kind::Meta, _) => Key::Mpack {
                    client,
                    id,
                    replica,
                },
                (Kind::Data, _) => Key::Pack { client, id },
            };
            match self.session.repo.backend().get(&key.path()).await {
                Ok(b) => return Ok(Some(b)),
                Err(BackendError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// 規格錯誤第 17 項的修法 D：存著的 pack 名稱與內容不符（位元損壞）而有 parity → 在記憶體裡修復，重算
    /// BLAKE3 等於名稱才算數。修好的 bytes 留給 P7c，它當正本的項目寫進 by_prune 的新 pack，損壞的 pack 走
    /// 一般的兩階段刪除；不覆寫任何東西。名稱相符的 pack（某一項本來就是假的）parity 救不了。
    async fn repair(
        &mut self,
        id: ObjectId,
        client: ClientId,
        kind: Kind,
    ) -> Result<Option<Arc<Vec<u8>>>> {
        if let Some(b) = self.repaired.get(&id) {
            return Ok(Some(Arc::clone(b)));
        }
        if self.unrepairable.contains(&id) {
            return Ok(None);
        }
        let parity = Key::Parity { client, id };
        if !self.parity.iter().any(|o| o.key == parity) {
            self.unrepairable.insert(id);
            return Ok(None);
        }
        let Some(damaged) = self.whole(id, client, kind).await? else {
            self.unrepairable.insert(id);
            return Ok(None);
        };
        if ObjectId::of(&damaged) == id {
            self.unrepairable.insert(id);
            return Ok(None);
        }
        let raw = match self.session.repo.backend().get(&parity.path()).await {
            Ok(r) => r,
            Err(BackendError::NotFound(_)) => {
                self.unrepairable.insert(id);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let key = pack_key(kind, client, id).path();
        match crate::blocking(move || {
            let obj = kist_format::v4::parity::parse(&raw)?;
            Ok(obj.repair(&id, &damaged)?)
        })
        .await
        {
            Ok(bytes) => {
                let bytes = Arc::new(bytes);
                self.warn(format!(
                    "{key} is damaged; repaired from its parity in memory, its live items are \
                     rewritten into a new pack by this prune"
                ));
                self.report.repaired += 1;
                self.repaired.insert(id, Arc::clone(&bytes));
                Ok(Some(bytes))
            }
            Err(e) => {
                self.warn(format!(
                    "{key} is damaged and its parity cannot repair it: {e}"
                ));
                self.unrepairable.insert(id);
                Ok(None)
            }
        }
    }

    /// 一個世代的 `T` 是否已過 `span` 秒（只看 `T` 的規則：P7b 以外的 P7e、P7g）。
    async fn t_passed(&mut self, g: Gen, span: u64) -> Result<bool> {
        Ok(self.session.times(g).await?.t.saturating_add(secs(span)) <= self.n_t)
    }

    /// P6、P7：在 `map` 上建 `Map(g+1)`，回傳 head g+1（分片在 P8 填）。
    async fn plan(&mut self) -> Result<Head> {
        let mut index = self
            .index
            .take()
            .ok_or_else(|| CoreError::Unsafe("prune: P5 did not run".to_owned()))?;
        let result = self.plan_with(&mut index).await;
        self.index = Some(index);
        result
    }

    async fn plan_with(&mut self, index: &mut Index) -> Result<Head> {
        let next = self.g + 1;

        // 已過 age 的 condemned（P7d 的兩個時鐘）
        let mut aged: BTreeSet<ObjectId> = BTreeSet::new();
        let condemned_: Vec<(ObjectId, Gen, u64)> = self
            .map
            .values()
            .filter(|r| r.status.s == status::CONDEMNED)
            .map(|r| (r.id, r.status.at, r.status.age))
            .collect();
        for (id, at, age) in condemned_ {
            let start = self.session.times(at).await?;
            if window_passed(start, self.now, age) {
                aged.insert(id);
            }
        }

        // P6 與刪前驗證：驗證時發現不實的項目就標成 dead，再重算正本，直到沒有變動
        let mut verdicts: HashMap<(ItemId, ObjectId), bool> = HashMap::new();
        let mut tainted: HashSet<ObjectId> = HashSet::new();
        loop {
            index.compute_canon(&self.map, &self.packs);
            if self.hold_active {
                break;
            }
            let mut removals: Vec<(ItemId, ObjectId, String)> = Vec::new();
            for pid in &aged {
                if index.is_canonical_pack(pid) {
                    continue;
                }
                let Some(rec) = self.map.get(pid) else {
                    continue;
                };
                let Some(refd) = index.referenced_in(pid) else {
                    continue;
                };
                let kind = rec.kind;
                let ids: Vec<ItemId> = rec
                    .entries
                    .iter()
                    .zip(refd)
                    .filter(|(_, r)| **r)
                    .map(|(e, _)| e.i)
                    .collect();
                for id in ids {
                    let Some((c, cei)) = index.canon_of(&self.map, &id, kind) else {
                        continue;
                    };
                    let Some(crec) = self.map.get(&c) else {
                        continue;
                    };
                    if crec.by_prune {
                        continue;
                    }
                    let h = Holder {
                        pack: c,
                        client: crec.client,
                        kind: crec.kind,
                        entry: crec.entries[cei],
                        source: Source::Map,
                        by_prune: false,
                        status: crec.status.s,
                    };
                    let v = match verdicts.get(&(id, c)) {
                        Some(v) => *v,
                        None => {
                            // 修得回的副本：不移除，但這一輪不憑它標 tombstone——repack 把它寫進
                            // by_prune 的新 pack 之後，下一輪以那一份驗證（規格錯誤第 17 項的修法 D）
                            let v = match self.fetch(&h).await? {
                                Copy::Genuine(_) => true,
                                Copy::False(why) => {
                                    removals.push((id, c, why));
                                    false
                                }
                                Copy::Repaired(_) | Copy::Missing => false,
                            };
                            verdicts.insert((id, c), v);
                            v
                        }
                    };
                    if !v {
                        tainted.insert(*pid);
                    }
                }
            }
            if removals.is_empty() {
                break;
            }
            for (id, c, why) in removals {
                if let Some((kind, client)) = self.map.get(&c).map(|r| (r.kind, r.client)) {
                    let key = pack_key(kind, client, c);
                    self.remove_false(index, id, c, &key, &format!("false ({why})"), " (P7d)");
                }
                verdicts.remove(&(id, c));
            }
        }

        let named: HashSet<ObjectId> = self
            .deltas
            .iter()
            .filter_map(|(_, d)| d.as_ref())
            .flat_map(|d| d.packs.iter().map(|p| p.id))
            .collect();
        let mut removed = Vec::new();
        let ids: Vec<ObjectId> = self.map.keys().copied().collect();
        for pid in ids {
            let Some(rec) = self.map.get(&pid) else {
                continue;
            };
            let (status_s, held, since, at) = (rec.status.s, rec.held, rec.since, rec.status.at);
            let listed = self.packs.get(&pid).copied();
            let new_status = match status_s {
                status::CONDEMNED => {
                    if index.is_canonical_pack(&pid) {
                        if self.hold_active && held {
                            None
                        } else {
                            self.report.revived += 1;
                            Some(Status::LIVE)
                        }
                    } else if !self.hold_active && aged.contains(&pid) {
                        let ok =
                            !tainted.contains(&pid)
                                && index.referenced_in(&pid).is_some_and(|refd| {
                                    rec.entries.iter().zip(refd).filter(|(_, r)| **r).all(
                                        |(e, _)| match index.canon_of(&self.map, &e.i, rec.kind) {
                                            None => false,
                                            Some((c, _)) => {
                                                self.map.get(&c).is_some_and(|cr| cr.by_prune)
                                                    || verdicts.get(&(e.i, c)) == Some(&true)
                                            }
                                        },
                                    )
                                });
                        if ok {
                            self.report.tombstoned += 1;
                            Some(tombstone(next))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                status::LIVE => {
                    if index.is_canonical_pack(&pid) {
                        None
                    } else {
                        match listed {
                            Some(l) if l.first().1.saturating_add(secs(self.grace)) <= self.n_t => {
                                self.report.condemned += 1;
                                Some(condemned(next, self.age))
                            }
                            _ => None,
                        }
                    }
                }
                status::UNREADABLE => {
                    if since <= self.g && self.t_passed(since, self.grace).await? {
                        self.report.condemned += 1;
                        Some(condemned(next, self.age))
                    } else {
                        None
                    }
                }
                status::TOMBSTONE => {
                    if listed.is_some() {
                        Some(tombstone(next))
                    } else {
                        if self.t_passed(at, self.grace).await? && !named.contains(&pid) {
                            removed.push(pid);
                        }
                        None
                    }
                }
                // 版本不認得的：不使用、不刪除
                _ => None,
            };
            if let Some(s) = new_status {
                if let Some(r) = self.map.get_mut(&pid) {
                    if s == Status::LIVE {
                        r.held = false;
                    }
                    r.status = s;
                }
            }
        }
        for pid in removed {
            self.map.remove(&pid);
            self.report.removed_records += 1;
        }

        let mut head = self.head.clone();
        if head.drain.is_some() && self.drain_over {
            head.drain = None;
        }
        // hold 的時間窗過了就拿掉 hold；記錄的 held 留著，只在被復活時清掉
        if head.hold.is_some() && !self.hold_active {
            head.hold = None;
        }
        Ok(head)
    }

    /// P7c：repack（hold 期間不做）。候選是 live、是正本的 pack：以 parity 修復過的一律做；其他的要比 grace 老、
    /// 當正本的項目（sealed bytes）不到 `repack_below` 百分比。把它當正本的項目逐一解密驗證（不實的標成 dead 並
    /// 回報；整份讀到的 bytes 名稱不符時先以 parity 修復），寫進全零 client 前綴下的新 pack（一個舊的對一個新的，
    /// 同 GC 模型），讀回驗證後記為 by_prune 的 live、舊的 → condemned（保留全部項目）。讀回驗證失敗：新的記為
    /// unreadable，舊的維持 live，回報。
    ///
    /// `--read-data`（[`PruneOptions::read_data`]）：不是候選的 live 資料 pack 也整份讀、逐項驗證它當正本的
    /// chunk（mpack 不必：P5 走訪時已逐一取回驗證）。有項目驗證失敗 → 先以 parity 修復（[`Prune::repair`]），
    /// 修得回就進 repack；修不回的項目標成 dead 並回報（同 P7c 對不實項目的處理；它若是唯一的持有者，下一輪
    /// P5 拒絕——資料真的遺失了，大聲失敗）。整輪的時間用掉一半就停、回報，剩下的留給下一次：這一輪照樣寫 head。
    async fn repack(&mut self) -> Result<()> {
        if self.hold_active {
            return Ok(());
        }
        let mut index = self
            .index
            .take()
            .ok_or_else(|| CoreError::Unsafe("prune: P5 did not run".to_owned()))?;
        let result = self.repack_with(&mut index).await;
        self.index = Some(index);
        result
    }

    async fn repack_with(&mut self, index: &mut Index) -> Result<()> {
        let mut jobs = Vec::new();
        let mut verify = Vec::new();
        for r in self.map.values() {
            if r.status.s != status::LIVE {
                continue;
            }
            let Some(canon) = index.canon_in(&r.id) else {
                continue;
            };
            if !canon.iter().any(|c| *c) {
                continue;
            }
            if !self.repaired.contains_key(&r.id) {
                let old = self
                    .packs
                    .get(&r.id)
                    .is_some_and(|l| l.first().1.saturating_add(secs(self.grace)) <= self.n_t);
                let total: u64 = r.entries.iter().map(|e| e.l).sum();
                let live: u64 = r
                    .entries
                    .iter()
                    .zip(canon)
                    .filter(|(_, c)| **c)
                    .map(|(e, _)| e.l)
                    .sum();
                let sparse =
                    live.saturating_mul(100) < total.saturating_mul(u64::from(self.repack_below));
                if !(old && sparse) {
                    if self.read_data && r.kind == Kind::Data {
                        verify.push(r.id);
                    }
                    continue;
                }
            }
            jobs.push(r.id);
        }
        for id in verify {
            if self.session.elapsed().as_secs().saturating_mul(2) + M.as_secs() >= self.grace {
                self.warn(format!(
                    "--read-data stopped after {} pack(s): half of this prune's time limit is \
                     used; the rest are read by a later prune",
                    self.report.read_packs
                ));
                break;
            }
            let Some((client, kind)) = self.map.get(&id).map(|r| (r.client, r.kind)) else {
                continue;
            };
            // 列表之後就不見了：P3 下一輪處理
            let Some(bytes) = self.whole(id, client, kind).await? else {
                continue;
            };
            self.report.read_packs += 1;
            let bad: Vec<ItemId> = match (self.map.get(&id), index.canon_in(&id)) {
                (Some(rec), Some(canon)) => rec
                    .entries
                    .iter()
                    .zip(canon)
                    .filter(|(e, c)| **c && self.open_in(kind, e, &bytes).is_err())
                    .map(|(e, _)| e.i)
                    .collect(),
                _ => Vec::new(),
            };
            if bad.is_empty() {
                continue;
            }
            if self.repair(id, client, kind).await?.is_some() {
                jobs.push(id);
                continue;
            }
            let key = pack_key(kind, client, id);
            for i in bad {
                self.remove_false(
                    index,
                    i,
                    id,
                    &key,
                    "damaged and cannot be repaired",
                    " (--read-data)",
                );
            }
        }
        for id in jobs {
            self.repack_one(index, id).await?;
        }
        Ok(())
    }

    async fn repack_one(&mut self, index: &mut Index, id: ObjectId) -> Result<()> {
        let next = self.g + 1;
        let Some((client, kind, entries)) = self.map.get(&id).and_then(|rec| {
            let canon = index.canon_in(&id)?;
            let entries: Vec<PackEntry> = rec
                .entries
                .iter()
                .zip(canon)
                .filter(|(_, c)| **c)
                .map(|(e, _)| *e)
                .collect();
            Some((rec.client, rec.kind, entries))
        }) else {
            return Ok(());
        };
        let mut bytes = match self.repaired.get(&id) {
            Some(b) => Arc::clone(b),
            None => match self.whole(id, client, kind).await? {
                Some(b) => Arc::new(b),
                // 列表之後就不見了：P3 下一輪處理
                None => return Ok(()),
            },
        };
        // 有打不開的先試 parity 修復；之後一項一項打開、寫進新 pack（明文不整批留在記憶體裡）
        if !self.repaired.contains_key(&id)
            && entries
                .iter()
                .any(|e| self.open_in(kind, e, &bytes).is_err())
        {
            if let Some(b) = self.repair(id, client, kind).await? {
                bytes = b;
            }
        }
        let key_old = pack_key(kind, client, id);
        let path = key_old.path();
        let repo = self.session.repo.clone();
        let chunk_max = repo.config().chunker.max;
        let mut w = PackWriter::new(Arc::clone(repo.keys()), kind, u64::MAX, chunk_max);
        let mut good = 0usize;
        for e in &entries {
            match self.open_in(kind, e, &bytes) {
                Ok(plain) => {
                    w.add(e.i, &plain)?;
                    good += 1;
                }
                Err(why) => self.remove_false(
                    index,
                    e.i,
                    id,
                    &key_old,
                    &format!("false ({why})"),
                    " (P7c)",
                ),
            }
        }
        drop(bytes);
        if good == 0 {
            return Ok(());
        }
        let Some(p) = w.finish()? else {
            return Ok(());
        };
        let key = pack_key(kind, ClientId::MAINT, p.id);
        let backend = repo.backend().clone();
        let mut copies = vec![key.clone()];
        if kind == Kind::Meta && self.head.settings.replicas == 1 {
            copies.push(Key::Mpack {
                client: ClientId::MAINT,
                id: p.id,
                replica: true,
            });
        }
        for k in &copies {
            match backend.put_if_absent(&k.path(), p.bytes.clone()).await {
                Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                Err(e) => {
                    self.warn(format!(
                        "repacking {path}: writing {} failed: {e}; it stays live",
                        k.path()
                    ));
                    return Ok(());
                }
            }
        }
        // repack 為新 pack 寫 parity（§13）：片數同舊的 parity，沒有就不寫；寫失敗只回報
        let old_parity = Key::Parity { client, id };
        if self.parity.iter().any(|o| o.key == old_parity) {
            let written = async {
                let raw = backend.get(&old_parity.path()).await?;
                let m = kist_format::v4::parity::parse(&raw)?.parity_shards();
                let (pid, bytes) = (p.id, p.bytes.clone());
                let parity =
                    crate::blocking(move || Ok(kist_format::v4::parity::encode(&pid, &bytes, m)?))
                        .await?;
                match backend
                    .put_if_absent(
                        &Key::Parity {
                            client: ClientId::MAINT,
                            id: p.id,
                        }
                        .path(),
                        parity,
                    )
                    .await
                {
                    Ok(()) | Err(BackendError::AlreadyExists(_)) => Ok::<_, CoreError>(()),
                    Err(e) => Err(e.into()),
                }
            }
            .await;
            if let Err(e) = written {
                self.warn(format!("{}: its parity is not written: {e}", key.path()));
            }
        }
        // 讀回驗證：名稱、trailer、每一項
        let verified = async {
            let back = backend.get(&key.path()).await?;
            if ObjectId::of(&back) != p.id {
                return Err(CoreError::corrupt(
                    key.path(),
                    "read back with different bytes",
                ));
            }
            let t = super::pack::trailer_of(repo.keys(), &key, &back, chunk_max)?;
            if t.entries != p.entries {
                return Err(CoreError::corrupt(
                    key.path(),
                    "read back with a different trailer",
                ));
            }
            for e in &t.entries {
                self.open_in(kind, e, &back)
                    .map_err(|why| CoreError::corrupt(key.path(), why))?;
            }
            Ok(())
        }
        .await;
        match verified {
            Ok(()) => {
                self.map.insert(
                    p.id,
                    Record {
                        id: p.id,
                        client: ClientId::MAINT,
                        kind,
                        size: p.bytes.len() as u64,
                        entries: p.entries,
                        by_prune: true,
                        since: next,
                        status: Status::LIVE,
                        held: false,
                    },
                );
                if let Some(r) = self.map.get_mut(&id) {
                    r.status = condemned(next, self.age);
                }
                self.report.repacked += 1;
            }
            Err(e) => {
                self.map.insert(
                    p.id,
                    Record {
                        id: p.id,
                        client: ClientId::MAINT,
                        kind,
                        size: p.bytes.len() as u64,
                        entries: Vec::new(),
                        by_prune: false,
                        since: next,
                        status: Status {
                            s: status::UNREADABLE,
                            at: 0,
                            age: 0,
                        },
                        held: false,
                    },
                );
                self.warn(format!(
                    "repacking {path}: the new pack does not read back ({e}); recorded as \
                     unreadable, {path} stays live"
                ));
            }
        }
        Ok(())
    }

    /// P8：寫出有變動的分片（`replicas=1` 時先主體、再 `.r1`），把名稱填進 head。
    async fn put_shards(&mut self, head: &mut Head) -> Result<()> {
        put_changed_shards(&self.session.repo, &self.base_hashes, &self.map, head).await
    }

    /// 刪一個物件；Object Lock 或權限擋下時回報。整輪時限過了就停止、回 false。
    async fn delete_one(&mut self, key: &Key) -> Result<bool> {
        if self.session.out_of_time(self.grace) {
            if self.report.unfinished.is_none() {
                self.report.unfinished = Some(format!(
                    "this prune ran past the GC grace ({}s) minus one hour; generation {} is \
                     written, the remaining deletions happen in the next prune",
                    self.grace, self.report.generation
                ));
            }
            return Ok(false);
        }
        match self.session.repo.backend().delete(&key.path()).await {
            Ok(()) => Ok(true),
            Err(e) => {
                let msg = format!("{}: {e}", key.path());
                tracing::warn!("cannot delete {msg}");
                self.report.undeleted.push(msg);
                Ok(false)
            }
        }
    }

    /// P9（仍在整輪時限內）。
    async fn delete(&mut self, new_head: &Head) -> Result<()> {
        let grace = secs(self.grace);
        // tombstone 而仍在列表的 pack：.r1、parity、主體
        let tombstones: Vec<(ObjectId, Listed)> = self
            .map
            .values()
            .filter(|r| r.status.s == status::TOMBSTONE)
            .filter_map(|r| self.packs.get(&r.id).map(|l| (r.id, *l)))
            .collect();
        let parity_of: HashSet<String> = self.parity.iter().map(|o| o.key.path()).collect();
        for (id, l) in tombstones {
            let mut keys = Vec::new();
            if l.r1.is_some() {
                keys.push(Key::Mpack {
                    client: l.client,
                    id,
                    replica: true,
                });
            }
            let parity = Key::Parity {
                client: l.client,
                id,
            };
            if parity_of.contains(&parity.path()) {
                keys.push(parity);
            }
            if l.body.is_some() {
                keys.push(pack_key(l.kind, l.client, id));
            }
            let mut all = true;
            for k in &keys {
                if !self.delete_one(k).await? {
                    all = false;
                    break;
                }
                if matches!(k, Key::Parity { .. }) {
                    self.report.deleted_parity += 1;
                }
            }
            if all {
                self.report.deleted_packs += 1;
                self.report.deleted_bytes += l.first().0;
            }
            if self.report.unfinished.is_some() {
                return Ok(());
            }
        }
        // delta：提到的 pack 都已在 Map(g+1) 或已不在列表；解不開的過了 grace 才刪
        let deltas: Vec<Key> = self
            .deltas
            .iter()
            .filter(|(o, d)| match d {
                Some(d) => d
                    .packs
                    .iter()
                    .all(|p| self.map.contains_key(&p.id) || !self.packs.contains_key(&p.id)),
                None => o.mtime.saturating_add(grace) <= self.n_t,
            })
            .map(|(o, _)| o.key.clone())
            .collect();
        for k in deltas {
            if self.delete_one(&k).await? {
                self.report.deleted_deltas += 1;
            }
            if self.report.unfinished.is_some() {
                return Ok(());
            }
        }
        // 兩代都沒引用、過了 grace 的分片
        let kept: HashSet<ObjectId> = self
            .head
            .shards
            .iter()
            .chain(new_head.shards.iter())
            .flatten()
            .copied()
            .collect();
        let shards: Vec<Key> = self
            .shards
            .iter()
            .filter(|o| match &o.key {
                Key::Shard { id, .. } => !kept.contains(id),
                _ => false,
            })
            .filter(|o| o.mtime.saturating_add(grace) <= self.n_t)
            .map(|o| o.key.clone())
            .collect();
        for k in shards {
            if self.delete_one(&k).await? {
                self.report.deleted_shards += 1;
            }
            if self.report.unfinished.is_some() {
                return Ok(());
            }
        }
        // pack 已不在、過了 grace 的 parity
        let orphans: Vec<Key> = self
            .parity
            .iter()
            .filter(|o| match &o.key {
                Key::Parity { id, .. } => !self.packs.contains_key(id),
                _ => false,
            })
            .filter(|o| o.mtime.saturating_add(grace) <= self.n_t)
            .map(|o| o.key.clone())
            .collect();
        for k in orphans {
            if self.delete_one(&k).await? {
                self.report.deleted_parity += 1;
            }
            if self.report.unfinished.is_some() {
                return Ok(());
            }
        }
        // bad 裡已不再當成起點的 snapshot：.r1、主體
        for path in std::mem::take(&mut self.bad_ignored) {
            let key = Key::parse(&path)?;
            let Key::Snapshot { client, ts, .. } = key else {
                continue;
            };
            let mut all = true;
            for replica in [true, false] {
                let k = Key::Snapshot {
                    client,
                    ts: ts.clone(),
                    replica,
                };
                if !self.delete_one(&k).await? {
                    all = false;
                    break;
                }
            }
            if all {
                self.report.deleted_snapshots += 1;
            }
            if self.report.unfinished.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }
}
