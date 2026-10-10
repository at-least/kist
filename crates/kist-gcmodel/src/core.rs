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
    /// 「自某個世代起已過一個 grace」的判斷（drain 結束、hold 結束、壞 snapshot）只看後端時間，
    /// 重建的沿用條件只看兩端 head 的後端時間（外部審查 F1–F3 之前的規則）。
    pub one_clock_windows: bool,
    /// P3、P5 拒絕之前不重新列出 `maint/gen/`（外部審查 F5 之前的規則）。
    pub no_superseded_check: bool,
    /// P1 遇到時間在未來的 head 就拒絕（收尾活性檢查之前的規則）。
    pub refuse_future_head: bool,
    /// 柵欄只看主體，不看 `.r1`（深度隨機測試之前的規則）。
    pub fence_ignores_r1: bool,
    /// 維護角色不補寫最新 head 缺少的 `.r1`（加上這條規則之前）。
    pub no_head_repair: bool,
    /// backup 在 B1 接受還沒有 `.r1` 的最新 head（加上這條規則之前）。
    pub backup_trusts_lone_head: bool,
    /// 重建的沿用條件也看 T 的最大減最小（第二輪外部審查 D1 之前）。
    pub keep_rule_t_clause: bool,
    /// 「已過一個 grace」的現在只用 base head 的值，不與 P0 的讀數取小（第二輪外部審查 C1–C3 之前）。
    pub now_from_base_head_only: bool,
    /// head 的 `.r1` 寫入失敗時照樣往下刪（第二輪外部審查 G1 之前）。
    pub p9_ignores_head_r1_failure: bool,
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
    /// 沒有 `.r1`、寫入者已當掉的 head 主體遺失的次數。
    pub lone_head_losses: u64,
    /// 維護角色補寫 head `.r1` 的次數。
    pub head_repairs: u64,
    /// head `.r1` 的 PUT 失敗（故障）的次數。
    pub head_r1_failures: u64,
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
    low_ids: u32,
    /// 誠實的 backup commit 過的 snapshot（主體 PUT 成功）。
    pub honest: BTreeSet<SnapKey>,
    /// 每台 client 驗證過的（item, 持有者）。
    pub verified: BTreeMap<ClientId, BTreeSet<(ItemId, PackId)>>,
    /// prune 主機的牆鐘偏差。
    pub skew: BTreeMap<u8, Time>,
    /// 最新 head 的分片被故障拿掉、還沒重建。
    pub shard_fault: bool,
    /// 被故障拿掉的分片。
    pub lost_shards: BTreeSet<ShardId>,
    pub fail_next_readback: bool,
    /// 故障：下一個 head `.r1` 的 PUT 失敗。
    pub fail_next_head_r1: bool,
    pub reader_failures: Vec<String>,
    /// P3 的「資料遺失」拒絕。模型沒有遺失 pack 的故障，所以出現就是誤報。
    pub alarms: Vec<String>,
    /// 出現過的最大時鐘偏差（主機或後端）。單一時鐘出錯會把刪除延後這麼久，收尾階段要等過它。
    pub max_skew_seen: Time,
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
            low_ids: 0,
            honest: BTreeSet::new(),
            verified: BTreeMap::new(),
            skew: BTreeMap::new(),
            shard_fault: false,
            lost_shards: BTreeSet::new(),
            fail_next_readback: false,
            fail_next_head_r1: false,
            reader_failures: Vec::new(),
            alarms: Vec::new(),
            max_skew_seen: 0,
            log: Vec::new(),
            stats: Stats::default(),
        }
    }

    pub fn fresh_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// pack 的名稱。名稱是內容的 BLAKE3，攻擊者可以反覆嘗試、挑一個比別人小的（排序的最後一層）：
    /// `low` 的名稱一律比一般的小。
    pub fn pack_id(&mut self, low: bool) -> PackId {
        if low {
            self.low_ids += 1;
            PackId(self.low_ids)
        } else {
            PackId(1_000_000 + self.fresh_id())
        }
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

    /// 同 [`Core::put`]，但請求不會在真實時間 `since` 之前開始（寫入者在那時才讀到它所依賴的
    /// head）：ε 不超過 `now − since`。時鐘偏差照樣套用，所以後端時鐘倒退仍然看得到。
    pub fn put_since(&mut self, key: Key, content: Content, since: Time) -> Result<(), Exists> {
        let eps = self.below(self.cfg.eps_max.max(0) as u64 + 1) as Time;
        let eps = eps.min((self.now - since).max(0));
        let mtime = self.now + self.backend.skew - eps;
        self.backend.put(key, content, mtime)
    }

    /// 柵欄（P8，改 settings 與重建也一樣）：這一代的 `.r1` 已經存在就是輸了——主體遺失、`.r1`
    /// 還在的世代不能被重新建立；否則以只在不存在時建立的方式寫主體。head 一律寫 `.r1`，由贏的人
    /// 在主體之後寫。
    pub fn fence_put(&mut self, g: Gen, head: &HeadBody, since: Time) -> bool {
        if self.backend.exists(&Key::HeadR1(g)) && !self.cfg.mutations.fence_ignores_r1 {
            return false;
        }
        self.put_since(Key::Head(g), Content::Head(head.clone()), since)
            .is_ok()
    }

    /// 寫入者在主體與 `.r1` 之間當掉時，那一代只有一份。維護角色在它上面建下一代之前，以只在不存在
    /// 時建立的方式補寫 `.r1`（內容是讀到的主體）；之後主體遺失才不會在世代之間留下空洞。
    /// 補寫失敗就回 false：不在只有一份的 head 上建下一代。
    pub fn repair_head_r1(&mut self, head: &HeadBody, since: Time) -> bool {
        if self.cfg.mutations.no_head_repair || self.backend.exists(&Key::HeadR1(head.g)) {
            return true;
        }
        if self.head_r1_put_fails(head.g) {
            return false;
        }
        if self
            .put_since(Key::HeadR1(head.g), Content::Head(head.clone()), since)
            .is_ok()
        {
            self.stats.head_repairs += 1;
            self.note(format!("repaired the .r1 of head {}", head.g));
        }
        true
    }

    /// 故障：這一次 head `.r1` 的 PUT 失敗（物件沒有建立，寫入者收到錯誤）。
    pub fn head_r1_put_fails(&mut self, g: Gen) -> bool {
        if !self.fail_next_head_r1 {
            return false;
        }
        self.fail_next_head_r1 = false;
        self.stats.head_r1_failures += 1;
        self.note(format!("fault: the .r1 PUT of head {g} failed"));
        true
    }

    /// B1 只從兩份都在的 head 決定沿用：只有一份的 head 遺失之後，那一代可以被別的計畫重新寫出，照它
    /// 沿用的持有者可能已被刪掉。實作稍候重新列出一次，模型直接停止（較保守的那一邊）。
    pub fn lone_head(&self, g: Gen) -> bool {
        !self.cfg.mutations.backup_trusts_lone_head && !self.backend.exists(&Key::HeadR1(g))
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

/// 「自 `start` 起已過 `span`」：後端時間與寫入者時間都要成立（§11.1）。`start` 與 `now` 都是
/// （T, w）。
/// 「自某個世代起已過一個 grace」的現在：每個時鐘取 base head 的值與這一輪 P0 的讀數中較小的。一個
/// 時鐘只在 base head 寫成時跑快、到這一輪的 P0 已恢復，就騙不到這個比較；原本只用 base head 的值，
/// 兩個時鐘在不同時間各錯一次，就能讓兩邊都提早成立（第二輪外部審查 C1–C3）。P5 的等待只用 P0 的
/// 讀數：等待期間不寫 head，`T(g)` 不前進。
pub fn now_pair(m: Mutations, base: (Time, Time), probe: (Time, Time)) -> (Time, Time) {
    if m.now_from_base_head_only {
        base
    } else {
        (base.0.min(probe.0), base.1.min(probe.1))
    }
}

pub fn window_passed(m: Mutations, start: (Time, Time), now: (Time, Time), span: Time) -> bool {
    let t = start.0 + span <= now.0;
    let w = start.1 + span <= now.1;
    if m.one_clock_windows {
        t
    } else {
        t && w
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
