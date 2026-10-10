//! 排程器：依 [`Action`] 清單推進世界。

use std::collections::{BTreeMap, BTreeSet};

use crate::backup::Backup;
use crate::check::check;
use crate::core::{load_highest, readable_snap, Config, Core, Stats};
use crate::maint::{FPhase, Forget, Rebuild, SettingsChange};
use crate::prune::Prune;
use crate::reader::Reader;
use crate::types::*;
use crate::universe::ItemDef;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepResult {
    Continue,
    Done,
    Abort(String),
}

pub type ActorId = u32;

#[derive(Clone, Debug)]
enum Actor {
    Backup(Box<Backup>),
    Prune(Box<Prune>),
    Forget(Forget),
    Reader(Box<Reader>),
    Settings(Box<SettingsChange>),
    Rebuild(Box<Rebuild>),
}

impl Actor {
    fn step(&mut self, core: &mut Core) -> StepResult {
        match self {
            Actor::Backup(a) => a.step(core),
            Actor::Prune(a) => a.step(core),
            Actor::Forget(a) => a.step(core),
            Actor::Reader(a) => a.step(core),
            Actor::Settings(a) => a.step(core),
            Actor::Rebuild(a) => a.step(core),
        }
    }

    fn describe(&self) -> String {
        match self {
            Actor::Backup(a) => format!("backup c{} {}", a.client, a.phase_name()),
            Actor::Prune(a) => format!("prune h{} {}", a.host, a.phase_name()),
            Actor::Forget(a) => format!("forget {:?} {:?}", a.key, a.phase),
            Actor::Reader(a) => format!("restore {:?} {:?}", a.key, a.phase),
            Actor::Settings(a) => format!("settings h{} {:?}", a.host, a.phase),
            Actor::Rebuild(a) => format!("rebuild h{} {:?}", a.host, a.phase),
        }
    }
}

/// 被入侵的備份主機（[`MALICIOUS`]）能做的寫入：只能在自己的前綴下建立物件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mal {
    /// 宣稱持有某些 item、副本卻是假的 pack，可以附上 delta。
    FalsePack {
        items: Vec<u16>,
        meta: bool,
        delta: bool,
    },
    /// 一個 delta，替別台的 pack 宣稱它沒有的內容。
    ForeignDelta { pack_pick: u16, items: Vec<u16> },
    /// 解不開的 snapshot。
    JunkSnap { r1: bool },
    /// trailer 解不開的 pack。
    JunkPack,
    /// 解得開、引用某個既有 item 的 snapshot。
    ForgedSnap { item: u16 },
    /// trailer 解得開但版本不認得的 pack。
    UnknownVersionPack,
    /// 挑過名稱（比所有一般的 pack 小）的 pack：宣稱持有 `items`，其中 `genuine` 的 bit 標示的
    /// 是真副本（只有後端上已經有真副本的 item，攻擊者才拿得到內容）。
    GroundPack {
        items: Vec<u16>,
        meta: bool,
        delta: bool,
        genuine: u8,
    },
    /// 解得開、版本是 5 的 snapshot。
    FutureSnap { item: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Advance(Time),
    StartBackup {
        client: u8,
        job: u8,
        variant: u8,
        under: u16,
    },
    StartPrune {
        host: u8,
        under: u16,
    },
    StartForget {
        pick: u16,
        force: bool,
    },
    StartReader {
        pick: u16,
    },
    StartSettings {
        host: u8,
        grace_hours: u8,
    },
    StartRebuild {
        host: u8,
    },
    Step {
        pick: u16,
    },
    /// 把某個 actor 一口氣跑完（中間不推進時間）。
    Finish {
        pick: u16,
    },
    Crash {
        pick: u16,
    },
    LoseShard {
        pick: u8,
    },
    SetSkew {
        host: u8,
        skew: i32,
    },
    BackendSkew(i32),
    FailReadback,
    /// head 的主體遺失（只在有 `.r1` 的 head 上，讀取端改讀 `.r1`）。
    LoseHeadBody {
        pick: u16,
    },
    Mal(Mal),
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub violations: Vec<String>,
    /// 第一個違反出現在第幾個 action 之後。
    pub at: Option<usize>,
    pub stats: Stats,
    pub log: Vec<String>,
}

impl Outcome {
    pub fn report(&self) -> String {
        let tail: Vec<&String> = self.log.iter().rev().take(60).collect();
        let mut s = format!(
            "violations after action {:?}:\n  {}\nstats: {:?}\nlast log lines (newest first):\n",
            self.at,
            self.violations.join("\n  "),
            self.stats
        );
        for line in tail {
            s.push_str("  ");
            s.push_str(line);
            s.push('\n');
        }
        s
    }
}

pub struct World {
    pub core: Core,
    actors: BTreeMap<ActorId, Actor>,
    next_actor: ActorId,
    client_busy: BTreeMap<ClientId, ActorId>,
    host_busy: BTreeMap<u8, ActorId>,
    /// 正在被強制 forget 最新一份的（client, roots）：退役中，不開始新的 backup（§9 的程序規則）。
    retiring: BTreeMap<(ClientId, u8), ActorId>,
}

impl World {
    /// init：寫 head 0（空的 map、預設 settings）。
    pub fn new(cfg: Config, seed: u64) -> Self {
        let mut core = Core::new(cfg, seed);
        let head = HeadBody {
            version: VERSION,
            g: 0,
            w: core.now,
            shards: [None; SHARDS],
            settings: Settings {
                grace: core.cfg.grace,
                replicas: core.cfg.replicas,
            },
            drain: None,
            bad: Vec::new(),
            hold: None,
        };
        let _ = core.put_now(Key::Head(0), Content::Head(head.clone()));
        let _ = core.put_now(Key::HeadR1(0), Content::Head(head));
        World {
            core,
            actors: BTreeMap::new(),
            next_actor: 1,
            client_busy: BTreeMap::new(),
            host_busy: BTreeMap::new(),
            retiring: BTreeMap::new(),
        }
    }

    pub fn run(cfg: Config, seed: u64, actions: &[Action]) -> Outcome {
        let mut w = World::new(cfg, seed);
        for (i, a) in actions.iter().enumerate() {
            w.apply(a);
            let v = check(&w.core);
            if !v.is_empty() {
                return Outcome {
                    violations: v,
                    at: Some(i),
                    stats: w.core.stats.clone(),
                    log: w.core.log.clone(),
                };
            }
        }
        Outcome {
            violations: Vec::new(),
            at: None,
            stats: w.core.stats.clone(),
            log: w.core.log.clone(),
        }
    }

    pub fn check(&self) -> Vec<String> {
        check(&self.core)
    }

    fn add(&mut self, actor: Actor) -> ActorId {
        let id = self.next_actor;
        self.next_actor += 1;
        self.actors.insert(id, actor);
        id
    }

    fn remove(&mut self, id: ActorId) {
        self.actors.remove(&id);
        self.client_busy.retain(|_, a| *a != id);
        self.host_busy.retain(|_, a| *a != id);
        self.retiring.retain(|_, a| *a != id);
    }

    fn nth(&self, pick: u16) -> Option<ActorId> {
        if self.actors.is_empty() {
            return None;
        }
        self.actors
            .keys()
            .nth(usize::from(pick) % self.actors.len())
            .copied()
    }

    pub fn actor_ids(&self) -> Vec<ActorId> {
        self.actors.keys().copied().collect()
    }

    pub fn describe(&self, id: ActorId) -> Option<String> {
        self.actors.get(&id).map(Actor::describe)
    }

    pub fn advance(&mut self, secs: Time) {
        self.core.now += secs.max(0);
    }

    pub fn start_backup(
        &mut self,
        client: ClientId,
        job: u8,
        variant: u8,
        under: Time,
    ) -> Option<ActorId> {
        if self.client_busy.contains_key(&client) || self.retiring.contains_key(&(client, job)) {
            return None;
        }
        let b = Backup::new(&mut self.core, client, job, variant, under);
        let id = self.add(Actor::Backup(Box::new(b)));
        self.client_busy.insert(client, id);
        Some(id)
    }

    pub fn start_prune(&mut self, host: u8, under: Time) -> Option<ActorId> {
        if self.host_busy.contains_key(&host) {
            return None;
        }
        let under = under.clamp(0, self.core.cfg.under_max.max(0));
        let id = self.add(Actor::Prune(Box::new(Prune::new(host, under))));
        self.host_busy.insert(host, id);
        Some(id)
    }

    pub fn start_settings(&mut self, host: u8, target: Time) -> Option<ActorId> {
        if self.host_busy.contains_key(&host) {
            return None;
        }
        let id = self.add(Actor::Settings(Box::new(SettingsChange::new(host, target))));
        self.host_busy.insert(host, id);
        Some(id)
    }

    pub fn start_rebuild(&mut self, host: u8) -> Option<ActorId> {
        if self.host_busy.contains_key(&host) {
            return None;
        }
        let id = self.add(Actor::Rebuild(Box::new(Rebuild::new(host))));
        self.host_busy.insert(host, id);
        Some(id)
    }

    pub fn start_reader(&mut self, key: SnapKey) -> ActorId {
        self.add(Actor::Reader(Box::new(Reader::new(key))))
    }

    /// forget 一個指定的 snapshot，不看任何規則（情境測試用）。
    pub fn start_forget_key(&mut self, key: SnapKey) -> ActorId {
        self.add(Actor::Forget(Forget {
            key,
            phase: FPhase::R1,
        }))
    }

    fn snapshot_keys(&self) -> Vec<SnapKey> {
        let keys: BTreeSet<SnapKey> = self
            .core
            .backend
            .keys()
            .filter_map(|k| match k {
                Key::Snap(s) | Key::SnapR1(s) => Some(*s),
                _ => None,
            })
            .collect();
        keys.into_iter().collect()
    }

    /// 某個 client 在某組 roots 上最新的 snapshot（主體讀得了的）。
    pub fn newest(&self, client: ClientId, job: u8) -> Option<SnapKey> {
        self.snapshot_keys()
            .into_iter()
            .filter(|k| k.client == client && self.core.backend.exists(&Key::Snap(*k)))
            .filter(|k| readable_snap(&self.core, *k).is_some_and(|s| s.job == job))
            .max_by_key(|k| k.ts)
    }

    /// forget 的規則（§9）：每個 client 在同一組 roots 上最新的那一份不刪；強制刪除時，照程序規則
    /// 等那個 client 的那組 roots 沒有進行中的 backup。
    fn start_forget(&mut self, pick: u16, force: bool) {
        let keys = self.snapshot_keys();
        if keys.is_empty() {
            return;
        }
        let key = keys[usize::from(pick) % keys.len()];
        let mut retire = None;
        if let Some(body) = readable_snap(&self.core, key) {
            if self.newest(key.client, body.job) == Some(key) {
                if !force {
                    return;
                }
                if self.core.cfg.forced_forget_waits_for_idle {
                    let busy = self
                        .client_busy
                        .get(&key.client)
                        .and_then(|a| self.actors.get(a));
                    if let Some(Actor::Backup(b)) = busy {
                        if b.job == body.job {
                            return;
                        }
                    }
                    retire = Some((key.client, body.job));
                }
            }
        }
        let id = self.start_forget_key(key);
        if let Some(r) = retire {
            self.retiring.insert(r, id);
        }
    }

    pub fn phase(&self, id: ActorId) -> Option<String> {
        self.actors.get(&id).map(|a| match a {
            Actor::Backup(b) => b.phase_name(),
            Actor::Prune(p) => p.phase_name(),
            other => other.describe(),
        })
    }

    pub fn step(&mut self, id: ActorId) -> Option<StepResult> {
        let actor = self.actors.get_mut(&id)?;
        let is_backup = matches!(actor, Actor::Backup(_));
        let r = actor.step(&mut self.core);
        match &r {
            StepResult::Continue => {}
            StepResult::Done => self.remove(id),
            StepResult::Abort(msg) => {
                if is_backup {
                    self.core.stats.backup_aborts += 1;
                }
                let line = format!("actor {id} stopped: {msg}");
                self.core.note(line);
                self.remove(id);
            }
        }
        self.clear_resolved_faults();
        Some(r)
    }

    /// 一直推進到這個 actor 結束。
    pub fn finish(&mut self, id: ActorId) -> Option<StepResult> {
        loop {
            match self.step(id)? {
                StepResult::Continue => {}
                other => return Some(other),
            }
        }
    }

    /// 推進到這個 actor 的下一個 phase 以 `phase` 開頭；actor 先結束就回 false。
    pub fn run_until(&mut self, id: ActorId, phase: &str) -> bool {
        loop {
            match self.phase(id) {
                None => return false,
                Some(p) if p.starts_with(phase) => return true,
                Some(_) => {
                    if self.step(id) != Some(StepResult::Continue) {
                        return false;
                    }
                }
            }
        }
    }

    fn clear_resolved_faults(&mut self) {
        if self.core.shard_fault && load_highest(&self.core).is_ok() {
            self.core.shard_fault = false;
        }
    }

    pub fn apply(&mut self, action: &Action) {
        match action {
            Action::Advance(d) => self.advance(*d),
            Action::StartBackup {
                client,
                job,
                variant,
                under,
            } => {
                let under = Time::from(*under) % (self.core.cfg.under_max.max(0) + 1);
                let _ = self.start_backup(client % 3 + 1, job % 2, variant % 32, under);
            }
            Action::StartPrune { host, under } => {
                let under = Time::from(*under) % (self.core.cfg.under_max.max(0) + 1);
                let _ = self.start_prune(host % 3, under);
            }
            Action::StartForget { pick, force } => self.start_forget(*pick, *force),
            Action::StartReader { pick } => {
                let keys: Vec<SnapKey> = self
                    .core
                    .honest
                    .iter()
                    .copied()
                    .filter(|k| self.core.backend.exists(&Key::Snap(*k)))
                    .collect();
                if !keys.is_empty() {
                    let key = keys[usize::from(*pick) % keys.len()];
                    self.start_reader(key);
                }
            }
            Action::StartSettings { host, grace_hours } => {
                let target = Time::from(*grace_hours).clamp(2, 120) * HOUR;
                let _ = self.start_settings(host % 3, target);
            }
            Action::StartRebuild { host } => {
                let _ = self.start_rebuild(host % 3);
            }
            Action::Step { pick } => {
                if let Some(id) = self.nth(*pick) {
                    self.step(id);
                }
            }
            Action::Finish { pick } => {
                if let Some(id) = self.nth(*pick) {
                    self.finish(id);
                }
            }
            Action::Crash { pick } => {
                if let Some(id) = self.nth(*pick) {
                    let line = format!("crash: {:?}", self.describe(id));
                    self.core.note(line);
                    self.remove(id);
                }
            }
            Action::LoseShard { pick } => {
                if let Ok(l) = load_highest(&self.core) {
                    let shards: Vec<ShardId> = l.head.shards.iter().flatten().copied().collect();
                    if !shards.is_empty() {
                        let sid = shards[usize::from(*pick) % shards.len()];
                        self.core.backend.delete(&Key::Shard(sid));
                        self.core.shard_fault = true;
                        self.core
                            .note(format!("fault: lost shard {sid:?} of head {}", l.g));
                    }
                }
            }
            Action::SetSkew { host, skew } => {
                let skew = Time::from(*skew);
                self.core.skew.insert(host % 3, skew);
                self.core.max_skew_seen = self.core.max_skew_seen.max(skew.abs());
            }
            Action::BackendSkew(s) => {
                let skew = Time::from(*s);
                self.core.backend.skew = skew;
                self.core.max_skew_seen = self.core.max_skew_seen.max(skew.abs());
            }
            Action::FailReadback => self.core.fail_next_readback = true,
            Action::LoseHeadBody { pick } => {
                let both: Vec<Gen> = self
                    .core
                    .backend
                    .keys()
                    .filter_map(|k| match k {
                        Key::HeadR1(g) if self.core.backend.exists(&Key::Head(*g)) => Some(*g),
                        _ => None,
                    })
                    .collect();
                if !both.is_empty() {
                    let g = both[usize::from(*pick) % both.len()];
                    self.core.backend.delete(&Key::Head(g));
                    self.core.note(format!("fault: lost the body of head {g}"));
                }
            }
            Action::Mal(m) => self.malicious(m),
        }
        self.clear_resolved_faults();
    }

    /// 情境測試用：被入侵的主機寫一個宣稱持有 `items`、副本卻是假的 pack，可以附 delta。
    pub fn mal_false_pack(&mut self, items: &[ItemId], kind: Kind, delta: bool) -> PackId {
        let core = &mut self.core;
        let entries: BTreeSet<ItemId> = items.iter().copied().collect();
        let id = core.pack_id(false);
        let body = PackBody {
            client: MALICIOUS,
            kind,
            trailer: Trailer::Readable {
                version: VERSION,
                kind,
                entries: entries.clone(),
            },
            copies: entries.iter().map(|i| (*i, false)).collect(),
        };
        let _ = core.put(
            Key::Pack {
                client: MALICIOUS,
                id,
            },
            Content::Pack(body),
        );
        if delta {
            let did = DeltaId(core.fresh_id());
            let _ = core.put(
                Key::Delta {
                    client: MALICIOUS,
                    id: did,
                },
                Content::Delta(DeltaBody {
                    readable: true,
                    client: MALICIOUS,
                    packs: vec![DeltaPack {
                        client: MALICIOUS,
                        pack: id,
                        kind,
                        entries,
                    }],
                }),
            );
        }
        core.note(format!("mal: false pack {id:?}"));
        id
    }

    /// 情境測試用：被入侵的主機在自己的前綴下寫一個 delta，替別台的 pack 宣稱 `items`。
    pub fn mal_foreign_delta(
        &mut self,
        victim: ClientId,
        pack: PackId,
        kind: Kind,
        items: &[ItemId],
    ) {
        let core = &mut self.core;
        let did = DeltaId(core.fresh_id());
        let _ = core.put(
            Key::Delta {
                client: MALICIOUS,
                id: did,
            },
            Content::Delta(DeltaBody {
                readable: true,
                client: MALICIOUS,
                packs: vec![DeltaPack {
                    client: victim,
                    pack,
                    kind,
                    entries: items.iter().copied().collect(),
                }],
            }),
        );
        core.note(format!("mal: foreign delta about {pack:?}"));
    }

    /// 情境測試用：所有實際上持有 `item` 真副本的 pack（主體或 `.r1` 在就算）。
    pub fn genuine_holders(&self, item: ItemId) -> Vec<(ClientId, PackId)> {
        let mut out: BTreeSet<(ClientId, PackId)> = BTreeSet::new();
        for (k, o) in self.core.backend.iter() {
            if let (Key::Pack { client, id } | Key::PackR1 { client, id }, Content::Pack(p)) =
                (k, &o.content)
            {
                if p.copies.get(&item) == Some(&true) {
                    out.insert((*client, *id));
                }
            }
        }
        out.into_iter().collect()
    }

    /// 情境測試用：後端上某個 client 的 pack。
    pub fn packs_of(&self, client: ClientId) -> Vec<PackId> {
        self.core
            .backend
            .keys()
            .filter_map(|k| match k {
                Key::Pack { client: c, id } if *c == client => Some(*id),
                _ => None,
            })
            .collect()
    }

    fn malicious(&mut self, m: &Mal) {
        let core = &mut self.core;
        let n = core.universe.len() as u32;
        if n == 0 {
            return;
        }
        let item = |i: &u16| ItemId(u32::from(*i) % n);
        match m {
            Mal::FalsePack { items, meta, delta } => {
                let kind = if *meta { Kind::Meta } else { Kind::Data };
                let entries: BTreeSet<ItemId> = items
                    .iter()
                    .map(item)
                    .filter(|i| core.universe.kind(*i) == kind)
                    .collect();
                if entries.is_empty() {
                    return;
                }
                let id = core.pack_id(false);
                let body = PackBody {
                    client: MALICIOUS,
                    kind,
                    trailer: Trailer::Readable {
                        version: VERSION,
                        kind,
                        entries: entries.clone(),
                    },
                    copies: entries.iter().map(|i| (*i, false)).collect(),
                };
                let _ = core.put(
                    Key::Pack {
                        client: MALICIOUS,
                        id,
                    },
                    Content::Pack(body),
                );
                if *delta {
                    let did = DeltaId(core.fresh_id());
                    let _ = core.put(
                        Key::Delta {
                            client: MALICIOUS,
                            id: did,
                        },
                        Content::Delta(DeltaBody {
                            readable: true,
                            client: MALICIOUS,
                            packs: vec![DeltaPack {
                                client: MALICIOUS,
                                pack: id,
                                kind,
                                entries,
                            }],
                        }),
                    );
                }
                core.note(format!("mal: false pack {id:?}"));
            }
            Mal::ForeignDelta { pack_pick, items } => {
                let victims: Vec<(ClientId, PackId, Kind)> = core
                    .backend
                    .iter()
                    .filter_map(|(k, o)| match (k, &o.content) {
                        (Key::Pack { client, id }, Content::Pack(p)) if *client != MALICIOUS => {
                            Some((*client, *id, p.kind))
                        }
                        _ => None,
                    })
                    .collect();
                if victims.is_empty() {
                    return;
                }
                let (vc, vp, kind) = victims[usize::from(*pack_pick) % victims.len()];
                let entries: BTreeSet<ItemId> = items
                    .iter()
                    .map(item)
                    .filter(|i| core.universe.kind(*i) == kind)
                    .collect();
                if entries.is_empty() {
                    return;
                }
                let did = DeltaId(core.fresh_id());
                let _ = core.put(
                    Key::Delta {
                        client: MALICIOUS,
                        id: did,
                    },
                    Content::Delta(DeltaBody {
                        readable: true,
                        client: MALICIOUS,
                        packs: vec![DeltaPack {
                            client: vc,
                            pack: vp,
                            kind,
                            entries,
                        }],
                    }),
                );
                core.note(format!("mal: foreign delta about {vp:?}"));
            }
            Mal::JunkSnap { r1 } => {
                let key = SnapKey {
                    client: MALICIOUS,
                    ts: core.fresh_id(),
                };
                let body = SnapBody {
                    readable: false,
                    version: VERSION,
                    job: 0,
                    root: ItemId(0),
                    parent: None,
                };
                let _ = core.put(Key::Snap(key), Content::Snap(body.clone()));
                if *r1 {
                    let _ = core.put(Key::SnapR1(key), Content::Snap(body));
                }
                core.note(format!("mal: junk snapshot {key:?}"));
            }
            Mal::JunkPack => {
                let id = core.pack_id(false);
                let body = PackBody {
                    client: MALICIOUS,
                    kind: Kind::Data,
                    trailer: Trailer::Unreadable,
                    copies: BTreeMap::new(),
                };
                let _ = core.put(
                    Key::Pack {
                        client: MALICIOUS,
                        id,
                    },
                    Content::Pack(body),
                );
            }
            Mal::ForgedSnap { item: i } => {
                let key = SnapKey {
                    client: MALICIOUS,
                    ts: core.fresh_id(),
                };
                let body = SnapBody {
                    readable: true,
                    version: VERSION,
                    job: 0,
                    root: item(i),
                    parent: None,
                };
                let _ = core.put(Key::Snap(key), Content::Snap(body));
            }
            Mal::UnknownVersionPack => {
                let seed = core.fresh_id();
                let junk = core.universe.intern(ItemDef::Junk(seed));
                let id = core.pack_id(false);
                let body = PackBody {
                    client: MALICIOUS,
                    kind: Kind::Data,
                    trailer: Trailer::Readable {
                        version: VERSION + 1,
                        kind: Kind::Data,
                        entries: [junk].into_iter().collect(),
                    },
                    copies: [(junk, true)].into_iter().collect(),
                };
                let _ = core.put(
                    Key::Pack {
                        client: MALICIOUS,
                        id,
                    },
                    Content::Pack(body),
                );
            }
            Mal::GroundPack {
                items,
                meta,
                delta,
                genuine,
            } => {
                let kind = if *meta { Kind::Meta } else { Kind::Data };
                let mut copies = BTreeMap::new();
                for (i, raw) in items.iter().enumerate() {
                    let it = item(raw);
                    if core.universe.kind(it) != kind {
                        continue;
                    }
                    let available = core.backend.iter().any(|(k, o)| {
                        matches!(k, Key::Pack { .. } | Key::PackR1 { .. })
                            && matches!(&o.content, Content::Pack(p) if p.copies.get(&it) == Some(&true))
                    });
                    copies.insert(it, (genuine >> (i % 8)) & 1 == 1 && available);
                }
                if copies.is_empty() {
                    return;
                }
                let entries: BTreeSet<ItemId> = copies.keys().copied().collect();
                let id = core.pack_id(true);
                let body = PackBody {
                    client: MALICIOUS,
                    kind,
                    trailer: Trailer::Readable {
                        version: VERSION,
                        kind,
                        entries: entries.clone(),
                    },
                    copies,
                };
                let _ = core.put(
                    Key::Pack {
                        client: MALICIOUS,
                        id,
                    },
                    Content::Pack(body),
                );
                if *delta {
                    let did = DeltaId(core.fresh_id());
                    let _ = core.put(
                        Key::Delta {
                            client: MALICIOUS,
                            id: did,
                        },
                        Content::Delta(DeltaBody {
                            readable: true,
                            client: MALICIOUS,
                            packs: vec![DeltaPack {
                                client: MALICIOUS,
                                pack: id,
                                kind,
                                entries,
                            }],
                        }),
                    );
                }
                core.note(format!("mal: ground pack {id:?}"));
            }
            Mal::FutureSnap { item: i } => {
                let key = SnapKey {
                    client: MALICIOUS,
                    ts: core.fresh_id(),
                };
                let body = SnapBody {
                    readable: true,
                    version: VERSION + 1,
                    job: 0,
                    root: item(i),
                    parent: None,
                };
                let _ = core.put(Key::Snap(key), Content::Snap(body));
            }
        }
    }

    /// 收尾的活性檢查（每個隨機 case 結束時）：停掉所有 actor、時鐘歸零、最新 head 讀不了就重建、
    /// forget 被入侵主機的 snapshot（操作者的處置），然後跑 `SETTLE_ROUNDS` 輪 prune，每輪推進一個
    /// grace 加一小時。每一輪都要完成；最後誠實前綴下不能留下沒有被任何 snapshot 引用的 pack，
    /// 每個誠實的 snapshot 都要讀得回來。
    pub fn settle(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for id in self.actor_ids() {
            self.remove(id);
        }
        self.core.skew.clear();
        self.core.backend.skew = 0;
        for _ in 0..3 {
            if load_highest(&self.core).is_ok() {
                break;
            }
            if let Some(id) = self.start_rebuild(0) {
                let r = self.finish(id);
                self.core.note(format!("settle: rebuild {r:?}"));
            }
        }
        if load_highest(&self.core).is_err() {
            out.push("liveness: the newest head is still unreadable after three rebuilds".into());
            return out;
        }
        let mal: Vec<SnapKey> = self
            .snapshot_keys()
            .into_iter()
            .filter(|k| k.client == MALICIOUS)
            .collect();
        for k in mal {
            let id = self.start_forget_key(k);
            self.finish(id);
        }
        // 單一時鐘出錯只會延後刪除，延後的量不超過出現過的偏差：先等過它。
        let delay = self.core.max_skew_seen;
        self.advance(delay);
        for round in 0..SETTLE_ROUNDS {
            let step = match load_highest(&self.core) {
                Ok(l) => {
                    // 每輪至少推進目前用得到的最長時間：grace、drain 的舊 grace、hold 的 grace，
                    // 以及 condemned 記錄裡最大的 age（grace 調低過時，舊記錄的 age 比較長）。
                    let h = &l.head;
                    let old = h.drain.map_or(0, |d| d.old_grace);
                    let hold = h.hold.map_or(0, |d| d.grace);
                    let age = l
                        .map
                        .values()
                        .filter_map(|r| match r.status {
                            Status::Condemned { age, .. } => Some(age),
                            _ => None,
                        })
                        .max()
                        .unwrap_or(0);
                    h.settings.grace.max(old).max(hold).max(age) + HOUR
                }
                Err(e) => {
                    out.push(format!(
                        "liveness: round {round}: newest head unreadable: {e:?}"
                    ));
                    return out;
                }
            };
            self.advance(step);
            let Some(id) = self.start_prune(0, 0) else {
                out.push("liveness: host 0 is busy".into());
                return out;
            };
            match self.finish(id) {
                Some(StepResult::Done) => {}
                other => out.push(format!("liveness: prune round {round} ended {other:?}")),
            }
        }
        let mut referenced: BTreeSet<ItemId> = BTreeSet::new();
        for k in self.snapshot_keys() {
            if let Some(s) = readable_snap(&self.core, k) {
                referenced.extend(self.core.universe.reachable(s.root));
            }
        }
        for (key, obj) in self.core.backend.iter() {
            let (Key::Pack { client, id } | Key::PackR1 { client, id }, Content::Pack(p)) =
                (key, &obj.content)
            else {
                continue;
            };
            if *client == MALICIOUS {
                continue;
            }
            if matches!(&p.trailer, Trailer::Readable { version, .. } if *version != VERSION) {
                continue;
            }
            let useful = p
                .copies
                .iter()
                .any(|(i, genuine)| *genuine && referenced.contains(i));
            if !useful {
                out.push(format!(
                    "liveness: unreferenced pack {id:?} of client {client} survived"
                ));
            }
        }
        let honest: Vec<SnapKey> = self
            .core
            .honest
            .iter()
            .copied()
            .filter(|k| self.core.backend.exists(&Key::Snap(*k)))
            .collect();
        for k in honest {
            let id = self.start_reader(k);
            self.finish(id);
        }
        out.extend(check(&self.core));
        out
    }

    /// [`World::run`] 之後再做 [`World::settle`]。
    pub fn run_and_settle(cfg: Config, seed: u64, actions: &[Action]) -> Outcome {
        let mut w = World::new(cfg, seed);
        for (i, a) in actions.iter().enumerate() {
            w.apply(a);
            let v = check(&w.core);
            if !v.is_empty() {
                return Outcome {
                    violations: v,
                    at: Some(i),
                    stats: w.core.stats.clone(),
                    log: w.core.log.clone(),
                };
            }
        }
        let v = w.settle();
        Outcome {
            at: if v.is_empty() {
                None
            } else {
                Some(actions.len())
            },
            violations: v,
            stats: w.core.stats.clone(),
            log: w.core.log.clone(),
        }
    }
}

/// 收尾時跑幾輪 prune：垃圾要先比 grace 老才被標，再過一個 age 才刪，hold 也要先結束。
pub const SETTLE_ROUNDS: usize = 6;
