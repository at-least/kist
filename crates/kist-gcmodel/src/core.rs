//! 共用的世界狀態（[`Core`]）與讀取端的共用步驟：讀 head 與分片、讀 delta、讀 pack 的副本。

use std::collections::{BTreeMap, BTreeSet};

use crate::backend::{Backend, Exists};
use crate::types::*;
use crate::universe::Universe;

/// 讓協定故意出錯的開關（反向對照）：每一個都應該能讓某個情境違反不變式，證明檢查不是空轉。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mutations {
    /// P7d 不做刪前驗證。
    pub no_verify_before_drop: bool,
    /// P7d 只比寫入者的時鐘 `w`，不比後端時間 `T`。
    pub only_writer_clock: bool,
    /// P7d 只比後端時間 `T`，不比 `w`。
    pub only_backend_clock: bool,
    /// B3、B4、B5 不檢查經過時間 E。
    pub no_elapsed_check: bool,
    /// P8 輸掉柵欄之後照樣執行 P9。
    pub no_fence: bool,
    /// B2 也沿用 condemned 的持有者。
    pub reuse_condemned: bool,
    /// 排序改成新進的先（`since` 大的先）。
    pub newcomer_first: bool,
    /// B4 不 HEAD parent。
    pub no_parent_head: bool,
    /// 讀取端接受 delta 描述別的前綴的 pack。
    pub no_delta_prefix_rule: bool,
    /// B2 不驗證別台的副本，任何持有者都信。
    pub trust_all: bool,
    /// B4 不確認沿用的 ID 還在 `Map(g_c)` 的記錄裡。
    pub no_b4_entries_check: bool,
    /// 重建不設 hold。
    pub no_hold: bool,
    /// prune 不管整輪時限。
    pub no_run_bound: bool,
    /// grace 調低時不 drain：新的 condemned 記錄直接用新的 grace 當 age。
    pub no_drain: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepackPolicy {
    Never,
    Always,
    /// 每個候選 pack 擲一次 1/3 的硬幣（世界的亂數）。
    Random,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub grace: Time,
    pub replicas: u8,
    pub eps_max: Time,
    pub under_max: Time,
    pub create_only: bool,
    pub max_items_per_pack: usize,
    pub repack: RepackPolicy,
    /// 強制 forget 每組 roots 最新的那一份時，遵守「那個 client 沒有進行中的 backup」的程序規則。
    pub forced_forget_waits_for_idle: bool,
    pub mutations: Mutations,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            grace: DEFAULT_GRACE,
            replicas: 1,
            eps_max: 20 * 60,
            under_max: 30 * 60,
            create_only: true,
            max_items_per_pack: 3,
            repack: RepackPolicy::Random,
            forced_forget_waits_for_idle: true,
            mutations: Mutations::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub commits: u64,
    pub backup_aborts: u64,
    pub generations: u64,
    pub fence_losses: u64,
    pub prune_refusals: u64,
    pub condemned: u64,
    pub revived: u64,
    pub tombstoned: u64,
    pub repacked: u64,
    pub packs_deleted: u64,
    pub false_claims_removed: u64,
    pub rebuilds: u64,
    pub reads_ok: u64,
}

#[derive(Clone, Debug)]
pub struct Core {
    pub cfg: Config,
    pub now: Time,
    pub backend: Backend,
    pub universe: Universe,
    rng: u64,
    next_id: u32,
    /// 誠實的 backup commit 過的 snapshot（主體 PUT 成功）。
    pub honest: BTreeSet<SnapKey>,
    /// 每台 client 驗證過的（item, 持有者）。
    pub verified: BTreeMap<ClientId, BTreeSet<(ItemId, PackId)>>,
    /// prune 主機的牆鐘偏差。
    pub skew: BTreeMap<u8, Time>,
    /// 最新 head 的分片被故障拿掉、還沒重建。
    pub shard_fault: bool,
    pub fail_next_readback: bool,
    pub reader_failures: Vec<String>,
    pub log: Vec<String>,
    pub stats: Stats,
}

pub const LOG_LIMIT: usize = 4000;

impl Core {
    pub fn new(cfg: Config, seed: u64) -> Self {
        Core {
            backend: Backend::new(cfg.create_only),
            cfg,
            now: 1_000_000_000,
            universe: Universe::default(),
            rng: seed | 1,
            next_id: 1,
            honest: BTreeSet::new(),
            verified: BTreeMap::new(),
            skew: BTreeMap::new(),
            shard_fault: false,
            fail_next_readback: false,
            reader_failures: Vec::new(),
            log: Vec::new(),
            stats: Stats::default(),
        }
    }

    pub fn fresh_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.rand() % n
        }
    }

    pub fn note(&mut self, msg: String) {
        if self.log.len() < LOG_LIMIT {
            let line = format!("[t={}] {msg}", self.now);
            self.log.push(line);
        }
    }

    /// 後端時鐘減去 ε：修改時間是請求開始的時間。
    fn mtime(&mut self) -> Time {
        let eps = self.below(self.cfg.eps_max.max(0) as u64 + 1) as Time;
        self.now + self.backend.skew - eps
    }

    pub fn put(&mut self, key: Key, content: Content) -> Result<(), Exists> {
        let mtime = self.mtime();
        self.backend.put(key, content, mtime)
    }

    /// 小物件、請求立刻完成：修改時間就是後端時鐘（P0 的探針）。
    pub fn put_now(&mut self, key: Key, content: Content) -> Result<(), Exists> {
        let mtime = self.backend_now();
        self.backend.put(key, content, mtime)
    }

    pub fn host_wall(&self, host: u8) -> Time {
        self.now + self.skew.get(&host).copied().unwrap_or(0)
    }

    pub fn backend_now(&self) -> Time {
        self.now + self.backend.skew
    }
}

#[derive(Clone, Debug)]
pub struct Loaded {
    pub g: Gen,
    pub head: HeadBody,
    /// `T(g)`。
    pub t: Time,
    pub map: Map,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    NoHead,
    Version(u8),
    MissingShard(Gen),
}

/// 列出 `maint/gen/`（主體與 `.r1`）。
pub fn list_heads(core: &Core) -> Vec<Gen> {
    let mut gens: Vec<Gen> = core
        .backend
        .keys()
        .filter_map(|k| match k {
            Key::Head(g) | Key::HeadR1(g) => Some(*g),
            _ => None,
        })
        .collect();
    gens.sort_unstable();
    gens.dedup();
    gens
}

/// 讀 head g：主體，不行就 `.r1`。回傳內容與 `T(g)`（主體的修改時間，沒有主體就用 `.r1` 的）。
pub fn read_head(core: &Core, g: Gen) -> Option<(HeadBody, Time)> {
    for key in [Key::Head(g), Key::HeadR1(g)] {
        if let Some(obj) = core.backend.get(&key) {
            if let Content::Head(h) = &obj.content {
                return Some((h.clone(), obj.mtime));
            }
        }
    }
    None
}

pub fn load_map(core: &Core, head: &HeadBody) -> Result<Map, LoadError> {
    let mut map = Map::new();
    for sid in head.shards.iter().flatten() {
        match core.backend.get(&Key::Shard(*sid)) {
            Some(obj) => match &obj.content {
                Content::Shard(s) => map.extend(s.records.iter().map(|(k, v)| (*k, v.clone()))),
                _ => return Err(LoadError::MissingShard(head.g)),
            },
            None => return Err(LoadError::MissingShard(head.g)),
        }
    }
    Ok(map)
}

pub fn load_gen(core: &Core, g: Gen) -> Result<Loaded, LoadError> {
    let (head, t) = read_head(core, g).ok_or(LoadError::NoHead)?;
    if head.version != VERSION {
        return Err(LoadError::Version(head.version));
    }
    let map = load_map(core, &head)?;
    Ok(Loaded { g, head, t, map })
}

/// 列出來的最大世代（§10 讀取端）。
pub fn load_highest(core: &Core) -> Result<Loaded, LoadError> {
    let g = *list_heads(core).last().ok_or(LoadError::NoHead)?;
    load_gen(core, g)
}

/// restore 與 mount：最大世代讀不了就退回較舊的世代。
pub fn load_with_fallback(core: &Core) -> Option<Loaded> {
    list_heads(core)
        .into_iter()
        .rev()
        .find_map(|g| load_gen(core, g).ok())
}

/// 讀所有解得開的 delta，回傳 map 沒提到的 pack 與它們被宣稱的內容（§10）。
pub fn read_deltas(core: &Core, map: &Map) -> BTreeMap<PackId, DeltaPack> {
    let mut out: BTreeMap<PackId, DeltaPack> = BTreeMap::new();
    for (key, obj) in core.backend.iter() {
        let Key::Delta { client, .. } = key else {
            continue;
        };
        let Content::Delta(d) = &obj.content else {
            continue;
        };
        if !d.readable || d.client != *client {
            continue;
        }
        for dp in &d.packs {
            if dp.client != *client && !core.cfg.mutations.no_delta_prefix_rule {
                continue;
            }
            if map.contains_key(&dp.pack) {
                continue;
            }
            out.entry(dp.pack)
                .and_modify(|e| e.entries.extend(dp.entries.iter().copied()))
                .or_insert_with(|| dp.clone());
        }
    }
    out
}

/// 讀 pack 的內容：主體，不行就 `.r1`。
pub fn read_pack(core: &Core, client: ClientId, id: PackId) -> Option<PackBody> {
    for key in [Key::Pack { client, id }, Key::PackR1 { client, id }] {
        if let Some(obj) = core.backend.get(&key) {
            if let Content::Pack(p) = &obj.content {
                return Some(p.clone());
            }
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Copy {
    Genuine,
    /// pack 在，但這個 item 的副本不對（解不開或 ID 不符），或根本沒有這一項。
    False,
    Missing,
}

/// 取回某個持有者裡的 item 並驗證（解密、重算 ID）。
pub fn fetch(core: &Core, client: ClientId, id: PackId, item: ItemId) -> Copy {
    match read_pack(core, client, id) {
        None => Copy::Missing,
        Some(p) => match p.copies.get(&item) {
            Some(true) => Copy::Genuine,
            _ => Copy::False,
        },
    }
}

pub fn read_snap(core: &Core, key: &Key) -> Option<SnapBody> {
    match core.backend.get(key).map(|o| &o.content) {
        Some(Content::Snap(s)) => Some(s.clone()),
        _ => None,
    }
}

/// 某個 snapshot 讀得了的內容：主體，不行就 `.r1`。
pub fn readable_snap(core: &Core, key: SnapKey) -> Option<SnapBody> {
    [Key::Snap(key), Key::SnapR1(key)]
        .iter()
        .filter_map(|k| read_snap(core, k))
        .find(|s| s.readable)
}
