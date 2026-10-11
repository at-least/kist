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
//! - **P6、P7**：正本與 `Map(g+1)`；P7d 的刪前驗證與兩個時鐘。P7c（repack）在下一步。
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
use kist_format::v4::index::{shard_of, status, Bad, Delta, Head, Record, Shard, Status, SHARDS};
use kist_format::v4::keys::{Gen, Key, DELTAS, MAP, MPACKS, PACKS, PARITY, SNAPSHOTS};
use kist_format::v4::meta::{content_type, decode_item, node_type, Entry, Item};
use kist_format::v4::pack::{Kind, PackEntry};
use kist_format::v4::snapshot::Snapshot;

use super::backup::Clock;
use super::maint::{secs, unix_ns, window_passed, Newest, Pair, Session};
use super::pack::{open_chunk, open_meta, PackWriter};
use super::repo::Repo;
use super::state::{pack_key, Holder, MapView};
use crate::{CoreError, Result};

/// prune 的選項。
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// P7c：live、是正本、比 grace 老的 pack，當正本的項目（sealed bytes）不到這個百分比就 repack；0 不依比例
    /// repack。以 parity 修復過的 pack 一律 repack（規格錯誤第 17 項的修法 D）。
    pub repack_below: u8,
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
    base: BTreeMap<ObjectId, Record>,
    map: BTreeMap<ObjectId, Record>,
    snaps: BTreeMap<String, (bool, bool)>,
    deltas: Vec<(Object, Option<Delta>)>,
    packs: BTreeMap<ObjectId, Listed>,
    parity: Vec<Object>,
    shards: Vec<Object>,
    /// 被引用的 ID 與它的種類。
    referenced: HashMap<ItemId, Kind>,
    /// P5 發現的不實項目（從記錄移除了的 (ID, pack)）。
    false_claims: HashSet<(ItemId, ObjectId)>,
    bad_ignored: Vec<String>,
    mpacks: MpackCache,
    repack_below: u8,
    /// 以 parity 修復過的 pack 的完整 bytes（P7c 從它 repack）與修不回的 pack。
    repaired: HashMap<ObjectId, Arc<Vec<u8>>>,
    unrepairable: HashSet<ObjectId>,
    /// P6 的正本（P7c 用）。
    canon: HashMap<ItemId, ObjectId>,
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
        let map = session.repo.load_map(&newest.copy.gen).await?;
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
        let records: BTreeMap<ObjectId, Record> =
            map.records().iter().map(|r| (r.id, r.clone())).collect();
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
            base: records.clone(),
            map: records,
            snaps: BTreeMap::new(),
            deltas: Vec::new(),
            packs: BTreeMap::new(),
            parity: Vec::new(),
            shards: Vec::new(),
            referenced: HashMap::new(),
            false_claims: HashSet::new(),
            bad_ignored: Vec::new(),
            mpacks: MpackCache::new(super::read::MPACK_MEMORY),
            repack_below: opts.repack_below,
            repaired: HashMap::new(),
            unrepairable: HashSet::new(),
            canon: HashMap::new(),
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

    /// P5：每一個 snapshot 當成起點。持有者的表在走訪之前建一次（P3、P4 之後的記錄），之後發現的不實項目
    /// 以 `false_claims` 排除。
    async fn walk_all(&mut self) -> Result<()> {
        let view = MapView::new(self.map.values().cloned().collect());
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
                self.walk(&view, &path, s.roots).await?;
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

    /// 一個 ID 的持有者：記錄裡種類相符、live 或 condemned、還在列表裡、不是已知的不實項目，依 §10 的順序。
    fn holders(&self, view: &MapView, id: &ItemId, kind: Kind) -> Vec<Holder> {
        view.holders(id, kind)
            .into_iter()
            .filter(|h| matches!(h.status, status::LIVE | status::CONDEMNED))
            .filter(|h| self.packs.contains_key(&h.pack))
            .filter(|h| !self.false_claims.contains(&(*id, h.pack)))
            .collect()
    }

    /// P5：走訪一個 snapshot 引用的每一個 item（每個 ID 一次）。
    async fn walk(&mut self, view: &MapView, snap: &str, roots: MetaId) -> Result<()> {
        let mut stack: Vec<MetaId> = Vec::new();
        if self.referenced.insert(roots.into(), Kind::Meta).is_none() {
            stack.push(roots);
        }
        while let Some(id) = stack.pop() {
            let item: ItemId = id.into();
            let holders = self.holders(view, &item, Kind::Meta);
            if holders.is_empty() {
                let msg = format!(
                    "metadata item {id} (snapshot {snap}) has no readable holder that is not a \
                     tombstone: data loss; run check"
                );
                return Err(self.refuse(msg).await);
            }
            let mut cbor = None;
            for h in &holders {
                match self.fetch(h).await? {
                    Copy::Genuine(c) | Copy::Repaired(c) => {
                        cbor = Some(c);
                        break;
                    }
                    Copy::False(why) => self.remove_false(item, h, &why),
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
                if self.referenced.contains_key(&child) {
                    continue;
                }
                self.referenced.insert(child, kind);
                match kind {
                    Kind::Meta => stack.push(child.to_meta()),
                    Kind::Data => {
                        if self.holders(view, &child, Kind::Data).is_empty() {
                            let msg = format!(
                                "chunk {child} (snapshot {snap}) has no readable holder that is \
                                 not a tombstone: data loss; run check"
                            );
                            return Err(self.refuse(msg).await);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 不實的項目從記錄移除並回報（P5、P7d）。
    fn remove_false(&mut self, id: ItemId, h: &Holder, why: &str) {
        if let Some(r) = self.map.get_mut(&h.pack) {
            let before = r.entries.len();
            r.entries.retain(|e| e.i != id);
            if r.entries.len() < before {
                self.report.false_entries += 1;
            }
        }
        self.false_claims.insert((id, h.pack));
        self.warn(format!(
            "{} claims {id} but its copy is false ({why}); removed from the record",
            h.key().path()
        ));
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

    /// P6：每個被引用的 ID 的正本。
    fn canonical(&self, map: &BTreeMap<ObjectId, Record>) -> HashMap<ItemId, ObjectId> {
        let view = MapView::new(map.values().cloned().collect());
        let mut out = HashMap::new();
        for (id, kind) in &self.referenced {
            let first = view
                .holders(id, *kind)
                .into_iter()
                .filter(|h| matches!(h.status, status::LIVE | status::CONDEMNED))
                .find(|h| self.packs.contains_key(&h.pack));
            if let Some(h) = first {
                out.insert(*id, h.pack);
            }
        }
        out
    }

    /// P6、P7：建 `Map(g+1)`，回傳 head g+1（分片在 P8 填）。
    async fn plan(&mut self) -> Result<Head> {
        let next = self.g + 1;
        let mut newmap = self.map.clone();

        // 已過 age 的 condemned（P7d 的兩個時鐘）
        let mut aged: BTreeSet<ObjectId> = BTreeSet::new();
        let condemned_: Vec<(ObjectId, Gen, u64)> = newmap
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

        // P6 與刪前驗證：驗證時發現不實的項目就從正本的記錄移除，再重算正本，直到沒有變動
        let mut verdicts: HashMap<(ItemId, ObjectId), bool> = HashMap::new();
        let mut tainted: HashSet<ObjectId> = HashSet::new();
        let canon = loop {
            let canon = self.canonical(&newmap);
            if self.hold_active {
                break canon;
            }
            let canonical_packs: HashSet<ObjectId> = canon.values().copied().collect();
            let mut removals: Vec<(ItemId, ObjectId, String)> = Vec::new();
            for pid in &aged {
                if canonical_packs.contains(pid) {
                    continue;
                }
                let Some(rec) = newmap.get(pid) else { continue };
                let ids: Vec<ItemId> = rec
                    .entries
                    .iter()
                    .filter(|e| self.referenced.get(&e.i) == Some(&rec.kind))
                    .map(|e| e.i)
                    .collect();
                for id in ids {
                    let Some(c) = canon.get(&id).copied() else {
                        continue;
                    };
                    let Some(crec) = newmap.get(&c) else { continue };
                    if crec.by_prune {
                        continue;
                    }
                    let v = match verdicts.get(&(id, c)) {
                        Some(v) => *v,
                        None => {
                            let Some(entry) = crec.entries.iter().find(|e| e.i == id).copied()
                            else {
                                continue;
                            };
                            let h = Holder {
                                pack: c,
                                client: crec.client,
                                kind: crec.kind,
                                entry,
                                source: super::state::Source::Map,
                                by_prune: false,
                                status: crec.status.s,
                            };
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
                break canon;
            }
            for (id, c, why) in removals {
                if let Some(r) = newmap.get_mut(&c) {
                    let path = pack_key(r.kind, r.client, c).path();
                    let before = r.entries.len();
                    r.entries.retain(|e| e.i != id);
                    if r.entries.len() < before {
                        self.report.false_entries += 1;
                        let msg = format!(
                            "{path} claims {id} but its copy is false ({why}); removed from the \
                             record (P7d)"
                        );
                        tracing::warn!("{msg}");
                        self.report.warnings.push(msg);
                    }
                }
                verdicts.remove(&(id, c));
            }
        };
        let canonical_packs: HashSet<ObjectId> = canon.values().copied().collect();

        let named: HashSet<ObjectId> = self
            .deltas
            .iter()
            .filter_map(|(_, d)| d.as_ref())
            .flat_map(|d| d.packs.iter().map(|p| p.id))
            .collect();
        let mut removed = Vec::new();
        let ids: Vec<ObjectId> = newmap.keys().copied().collect();
        for pid in ids {
            let Some(rec) = newmap.get(&pid).cloned() else {
                continue;
            };
            let listed = self.packs.get(&pid).copied();
            let new_status = match rec.status.s {
                status::CONDEMNED => {
                    if canonical_packs.contains(&pid) {
                        if self.hold_active && rec.held {
                            None
                        } else {
                            self.report.revived += 1;
                            Some(Status::LIVE)
                        }
                    } else if !self.hold_active && aged.contains(&pid) {
                        let ok = !tainted.contains(&pid)
                            && rec
                                .entries
                                .iter()
                                .filter(|e| self.referenced.get(&e.i) == Some(&rec.kind))
                                .all(|e| match canon.get(&e.i) {
                                    None => false,
                                    Some(c) => {
                                        newmap.get(c).is_some_and(|cr| cr.by_prune)
                                            || verdicts.get(&(e.i, *c)) == Some(&true)
                                    }
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
                    if canonical_packs.contains(&pid) {
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
                    if rec.since <= self.g && self.t_passed(rec.since, self.grace).await? {
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
                        if self.t_passed(rec.status.at, self.grace).await? && !named.contains(&pid)
                        {
                            removed.push(pid);
                        }
                        None
                    }
                }
                // 版本不認得的：不使用、不刪除
                _ => None,
            };
            if let Some(s) = new_status {
                if let Some(r) = newmap.get_mut(&pid) {
                    if s == Status::LIVE {
                        r.held = false;
                    }
                    r.status = s;
                }
            }
        }
        for pid in removed {
            newmap.remove(&pid);
            self.report.removed_records += 1;
        }
        self.report.items = self
            .referenced
            .values()
            .filter(|k| **k == Kind::Meta)
            .count() as u64;
        self.report.chunks = self.referenced.len() as u64 - self.report.items;

        let mut head = self.head.clone();
        if head.drain.is_some() && self.drain_over {
            head.drain = None;
        }
        // hold 的時間窗過了就拿掉 hold；記錄的 held 留著，只在被復活時清掉
        if head.hold.is_some() && !self.hold_active {
            head.hold = None;
        }
        self.map = newmap;
        self.canon = canon;
        Ok(head)
    }

    /// P7c：repack（hold 期間不做）。候選是 live、是正本的 pack：以 parity 修復過的一律做；其他的要比 grace 老、
    /// 當正本的項目（sealed bytes）不到 `repack_below` 百分比。把它當正本的項目逐一解密驗證（不實的從記錄移除並
    /// 回報；整份讀到的 bytes 名稱不符時先以 parity 修復），寫進全零 client 前綴下的新 pack（一個舊的對一個新的，
    /// 同 GC 模型），讀回驗證後記為 by_prune 的 live、舊的 → condemned（保留全部項目）。讀回驗證失敗：新的記為
    /// unreadable，舊的維持 live，回報。
    async fn repack(&mut self) -> Result<()> {
        if self.hold_active {
            return Ok(());
        }
        let mut canon_of: HashMap<ObjectId, HashSet<ItemId>> = HashMap::new();
        for (id, p) in &self.canon {
            canon_of.entry(*p).or_default().insert(*id);
        }
        let mut jobs = Vec::new();
        for r in self.map.values() {
            if r.status.s != status::LIVE {
                continue;
            }
            let Some(items) = canon_of.get(&r.id) else {
                continue;
            };
            if !self.repaired.contains_key(&r.id) {
                let old = self
                    .packs
                    .get(&r.id)
                    .is_some_and(|l| l.first().1.saturating_add(secs(self.grace)) <= self.n_t);
                let total: u64 = r.entries.iter().map(|e| e.l).sum();
                let live: u64 = r
                    .entries
                    .iter()
                    .filter(|e| items.contains(&e.i))
                    .map(|e| e.l)
                    .sum();
                let sparse =
                    live.saturating_mul(100) < total.saturating_mul(u64::from(self.repack_below));
                if !(old && sparse) {
                    continue;
                }
            }
            jobs.push((r.id, items.clone()));
        }
        for (id, items) in jobs {
            self.repack_one(id, &items).await?;
        }
        Ok(())
    }

    async fn repack_one(&mut self, id: ObjectId, items: &HashSet<ItemId>) -> Result<()> {
        let next = self.g + 1;
        let Some(rec) = self.map.get(&id).cloned() else {
            return Ok(());
        };
        let entries: Vec<PackEntry> = rec
            .entries
            .iter()
            .filter(|e| items.contains(&e.i))
            .copied()
            .collect();
        let mut bytes = match self.repaired.get(&id) {
            Some(b) => Arc::clone(b),
            None => match self.whole(id, rec.client, rec.kind).await? {
                Some(b) => Arc::new(b),
                // 列表之後就不見了：P3 下一輪處理
                None => return Ok(()),
            },
        };
        let mut opened: Vec<std::result::Result<Vec<u8>, String>> = entries
            .iter()
            .map(|e| self.open_in(rec.kind, e, &bytes))
            .collect();
        if opened.iter().any(|o| o.is_err()) && !self.repaired.contains_key(&id) {
            if let Some(b) = self.repair(id, rec.client, rec.kind).await? {
                bytes = b;
                opened = entries
                    .iter()
                    .map(|e| self.open_in(rec.kind, e, &bytes))
                    .collect();
            }
        }
        let path = pack_key(rec.kind, rec.client, id).path();
        let mut good = Vec::new();
        for (e, o) in entries.iter().zip(opened) {
            match o {
                Ok(plain) => good.push((e.i, plain)),
                Err(why) => {
                    if let Some(r) = self.map.get_mut(&id) {
                        r.entries.retain(|x| x.i != e.i);
                    }
                    self.report.false_entries += 1;
                    self.warn(format!(
                        "{path} claims {} but its copy is false ({why}); removed from the record (P7c)",
                        e.i
                    ));
                }
            }
        }
        if good.is_empty() {
            return Ok(());
        }
        let repo = self.session.repo.clone();
        let chunk_max = repo.config().chunker.max;
        let mut w = PackWriter::new(Arc::clone(repo.keys()), rec.kind, u64::MAX, chunk_max);
        for (i, plain) in &good {
            w.add(*i, plain)?;
        }
        let Some(p) = w.finish()? else {
            return Ok(());
        };
        let key = pack_key(rec.kind, ClientId::MAINT, p.id);
        let backend = repo.backend().clone();
        let mut copies = vec![key.clone()];
        if rec.kind == Kind::Meta && self.head.settings.replicas == 1 {
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
        let old_parity = Key::Parity {
            client: rec.client,
            id,
        };
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
                self.open_in(rec.kind, e, &back)
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
                        kind: rec.kind,
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
                        kind: rec.kind,
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
        let mut new: BTreeMap<u8, Vec<Record>> = BTreeMap::new();
        for r in self.map.values() {
            new.entry(shard_of(&r.id)).or_default().push(r.clone());
        }
        let mut old: BTreeMap<u8, Vec<Record>> = BTreeMap::new();
        for r in self.base.values() {
            old.entry(shard_of(&r.id)).or_default().push(r.clone());
        }
        let backend = self.session.repo.backend().clone();
        for i in 0..SHARDS {
            let i = i as u8;
            let recs = new.remove(&i).unwrap_or_default();
            if old.get(&i).map_or(recs.is_empty(), |o| *o == recs) {
                continue;
            }
            if recs.is_empty() {
                head.shards[i as usize] = None;
                continue;
            }
            let plain = Shard::new(i, recs).encode()?;
            let sealed = self.session.repo.keys().seal_shard(&plain)?;
            let id = ObjectId::of(&sealed);
            for replica in [false, true] {
                if replica && self.head.settings.replicas == 0 {
                    continue;
                }
                match backend
                    .put_if_absent(&Key::Shard { id, replica }.path(), sealed.clone())
                    .await
                {
                    Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            head.shards[i as usize] = Some(id);
        }
        Ok(())
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
