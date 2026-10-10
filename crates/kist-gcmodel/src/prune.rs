//! prune（§11.3 的 P0–P9）。

use std::collections::{BTreeMap, BTreeSet};

use crate::core::{fetch, load_highest, read_head, read_pack, read_snap, Copy, Core};
use crate::types::*;
use crate::universe::Universe;
use crate::world::StepResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PPhase {
    Probe,
    Load,
    ListSnaps,
    ListDeltas,
    ListPacks,
    ListShards,
    Newcomers,
    Walk(usize),
    Plan,
    RepackPut(usize),
    RepackCheck(usize),
    PutShards,
    PutHead,
    PutHeadR1,
    Delete(usize),
}

/// P2 列出來的一個 pack。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed {
    pub client: ClientId,
    pub body: bool,
    pub r1: bool,
    pub mtime: Time,
}

#[derive(Clone, Debug)]
struct Repack {
    old: PackId,
    kind: Kind,
    items: BTreeSet<ItemId>,
    new: Option<PackId>,
}

#[derive(Clone, Debug)]
pub struct Prune {
    pub host: u8,
    pub phase: PPhase,
    start: Time,
    under: Time,
    /// P0 探針的後端時間。
    probe_time: Time,
    g: Gen,
    head: Option<HeadBody>,
    n_t: Time,
    n_w: Time,
    base: Map,
    map: Map,
    age: Time,
    hold_active: bool,
    snaps: Vec<SnapKey>,
    deltas: Vec<(Key, Option<DeltaBody>)>,
    listed: BTreeMap<PackId, Listed>,
    shard_list: Vec<(ShardId, Time)>,
    referenced: BTreeSet<ItemId>,
    bad_ignored: Vec<SnapKey>,
    bad_only: bool,
    newmap: Map,
    repacks: Vec<Repack>,
    new_head: Option<HeadBody>,
    deletions: Vec<Key>,
    times: BTreeMap<Gen, (Time, Time)>,
}

pub fn refuse(core: &mut Core, msg: String) -> StepResult {
    core.stats.prune_refusals += 1;
    core.note(msg.clone());
    StepResult::Abort(msg)
}

/// P0：條件建立兩次必須得到「已存在」；後端時間與本機牆鐘相差不超過容差。回傳探針的後端時間。
pub fn probe_backend(core: &mut Core, host: u8) -> Result<Time, String> {
    let key = Key::Probe(core.fresh_id());
    if core.put_now(key, Content::Probe).is_err() {
        return Err("P0: probe name collision".into());
    }
    let second = core.put_now(key, Content::Probe);
    let mtime = core.backend.get(&key).map(|o| o.mtime);
    core.backend.delete(&key);
    if second.is_ok() {
        return Err("P0: the backend accepted a second create (not create-only)".into());
    }
    let wall = core.host_wall(host);
    match mtime {
        Some(t) if (t - wall).abs() <= PROBE_TOLERANCE => Ok(t),
        Some(t) => Err(format!("P0: backend time {t} vs local clock {wall}")),
        None => Err("P0: probe vanished".into()),
    }
}

/// P4：新進者的記錄。
pub fn newcomer_record(body: &PackBody, client: ClientId, since: Gen) -> Result<Record, String> {
    match &body.trailer {
        Trailer::Readable { version, .. } if *version != VERSION => {
            Err(format!("P4: pack trailer version {version} is unknown"))
        }
        Trailer::Readable { kind, entries, .. } if *kind == body.kind => Ok(Record {
            client,
            kind: *kind,
            entries: entries.clone(),
            by_prune: false,
            since,
            status: Status::Live,
            held: false,
        }),
        _ => Ok(Record {
            client,
            kind: body.kind,
            entries: BTreeSet::new(),
            by_prune: false,
            since,
            status: Status::Unreadable,
            held: false,
        }),
    }
}

/// 列出 `packs/` 與 `mpacks/`（含 `.r1`）。
pub fn list_packs(core: &Core) -> BTreeMap<PackId, Listed> {
    let mut listed: BTreeMap<PackId, Listed> = BTreeMap::new();
    for (key, obj) in core.backend.iter() {
        match key {
            Key::Pack { client, id } => {
                let e = listed.entry(*id).or_insert(Listed {
                    client: *client,
                    body: false,
                    r1: false,
                    mtime: obj.mtime,
                });
                e.body = true;
                e.mtime = obj.mtime;
            }
            Key::PackR1 { client, id } => {
                let e = listed.entry(*id).or_insert(Listed {
                    client: *client,
                    body: false,
                    r1: false,
                    mtime: obj.mtime,
                });
                e.r1 = true;
            }
            _ => {}
        }
    }
    listed
}

/// P6：每個被引用的 ID 的正本。
pub fn canonical(
    map: &Map,
    referenced: &BTreeSet<ItemId>,
    universe: &Universe,
    listed: &BTreeMap<PackId, Listed>,
    newcomer_first: bool,
) -> BTreeMap<ItemId, PackId> {
    let mut best: BTreeMap<ItemId, ((u8, u32, PackId), PackId)> = BTreeMap::new();
    for (pid, rec) in map {
        if !matches!(rec.status, Status::Live | Status::Condemned { .. })
            || !listed.contains_key(pid)
        {
            continue;
        }
        let r = rank(rec, *pid, newcomer_first);
        for id in rec.entries.intersection(referenced) {
            if universe.kind(*id) != rec.kind {
                continue;
            }
            let better = best.get(id).is_none_or(|(br, _)| r < *br);
            if better {
                best.insert(*id, (r, *pid));
            }
        }
    }
    best.into_iter().map(|(i, (_, p))| (i, p)).collect()
}

fn invert(canon: &BTreeMap<ItemId, PackId>) -> BTreeMap<PackId, BTreeSet<ItemId>> {
    let mut out: BTreeMap<PackId, BTreeSet<ItemId>> = BTreeMap::new();
    for (id, p) in canon {
        out.entry(*p).or_default().insert(*id);
    }
    out
}

impl Prune {
    pub fn new(host: u8, under: Time) -> Self {
        Prune {
            host,
            phase: PPhase::Probe,
            start: 0,
            under,
            probe_time: 0,
            g: 0,
            head: None,
            n_t: 0,
            n_w: 0,
            base: Map::new(),
            map: Map::new(),
            age: 0,
            hold_active: false,
            snaps: Vec::new(),
            deltas: Vec::new(),
            listed: BTreeMap::new(),
            shard_list: Vec::new(),
            referenced: BTreeSet::new(),
            bad_ignored: Vec::new(),
            bad_only: false,
            newmap: Map::new(),
            repacks: Vec::new(),
            new_head: None,
            deletions: Vec::new(),
            times: BTreeMap::new(),
        }
    }

    pub fn phase_name(&self) -> String {
        format!("{:?}", self.phase)
    }

    fn gen_times(&mut self, core: &Core, g: Gen) -> Option<(Time, Time)> {
        if let Some(v) = self.times.get(&g) {
            return Some(*v);
        }
        let (h, t) = read_head(core, g)?;
        self.times.insert(g, (t, h.w));
        Some((t, h.w))
    }

    fn grace(&self) -> Time {
        self.head
            .as_ref()
            .map_or(DEFAULT_GRACE, |h| h.settings.grace)
    }

    fn run_elapsed(&self, core: &Core) -> Time {
        core.now - self.start - self.under
    }

    fn out_of_time(&self, core: &Core) -> bool {
        !core.cfg.mutations.no_run_bound && self.run_elapsed(core) >= self.grace() - M
    }

    /// P7d 的兩個時鐘：`T(c) + age ≤ T(g)` 而且 `w(c) + age ≤ w(g)`。
    fn aged(&mut self, core: &Core, at: Gen, age: Time) -> Option<bool> {
        let (t_c, w_c) = self.gen_times(core, at)?;
        let t_ok = t_c + age <= self.n_t;
        let w_ok = w_c + age <= self.n_w;
        let muts = core.cfg.mutations;
        Some(if muts.only_writer_clock {
            w_ok
        } else if muts.only_backend_clock {
            t_ok
        } else {
            t_ok && w_ok
        })
    }

    pub fn step(&mut self, core: &mut Core) -> StepResult {
        match self.phase {
            PPhase::Probe => match probe_backend(core, self.host) {
                Ok(t) => {
                    self.probe_time = t;
                    self.start = core.now;
                    self.phase = PPhase::Load;
                    StepResult::Continue
                }
                Err(e) => refuse(core, e),
            },
            PPhase::Load => self.load(core),
            PPhase::ListSnaps => {
                let mut keys = BTreeSet::new();
                for k in core.backend.keys() {
                    if let Key::Snap(s) | Key::SnapR1(s) = k {
                        keys.insert(*s);
                    }
                }
                self.snaps = keys.into_iter().collect();
                self.phase = PPhase::ListDeltas;
                StepResult::Continue
            }
            PPhase::ListDeltas => {
                for (k, obj) in core.backend.iter() {
                    if let Key::Delta { client, .. } = k {
                        let body = match &obj.content {
                            Content::Delta(d) if d.readable && d.client == *client => {
                                Some(d.clone())
                            }
                            _ => None,
                        };
                        self.deltas.push((*k, body));
                    }
                }
                self.phase = PPhase::ListPacks;
                StepResult::Continue
            }
            PPhase::ListPacks => {
                self.listed = list_packs(core);
                self.phase = PPhase::ListShards;
                StepResult::Continue
            }
            PPhase::ListShards => {
                for (k, obj) in core.backend.iter() {
                    if let Key::Shard(s) = k {
                        self.shard_list.push((*s, obj.mtime));
                    }
                }
                self.phase = PPhase::Newcomers;
                StepResult::Continue
            }
            PPhase::Newcomers => self.newcomers(core),
            PPhase::Walk(i) => self.walk(core, i),
            PPhase::Plan => self.plan(core),
            PPhase::RepackPut(i) => self.repack_put(core, i),
            PPhase::RepackCheck(i) => self.repack_check(core, i),
            PPhase::PutShards => self.put_shards(core),
            PPhase::PutHead => self.put_head(core),
            PPhase::PutHeadR1 => self.put_head_r1(core),
            PPhase::Delete(i) => self.delete(core, i),
        }
    }

    /// P1。
    fn load(&mut self, core: &mut Core) -> StepResult {
        let loaded = match load_highest(core) {
            Ok(l) => l,
            Err(e) => return refuse(core, format!("P1: cannot read the newest head: {e:?}")),
        };
        let wall = core.host_wall(self.host);
        if loaded.t > wall + M {
            return refuse(
                core,
                format!(
                    "P1: T({}) = {} is ahead of the local clock {wall}",
                    loaded.g, loaded.t
                ),
            );
        }
        self.g = loaded.g;
        self.n_t = loaded.t;
        self.n_w = loaded.head.w;
        self.times.insert(loaded.g, (loaded.t, loaded.head.w));
        self.age = loaded.head.settings.grace;
        if let Some(d) = loaded.head.drain {
            let Some((t_from, _)) = self.gen_times(core, d.from) else {
                return refuse(core, format!("P1: head {} is missing", d.from));
            };
            if self.n_t < t_from + d.old_grace && !core.cfg.mutations.no_drain {
                self.age = d.old_grace;
            }
        }
        if let Some(h) = loaded.head.hold {
            let Some((t_lost, _)) = self.gen_times(core, h.lost) else {
                return refuse(core, format!("P1: head {} is missing", h.lost));
            };
            self.hold_active = !core.cfg.mutations.no_hold && self.n_t < t_lost + h.grace;
        }
        self.base = loaded.map.clone();
        self.map = loaded.map;
        self.head = Some(loaded.head);
        self.phase = PPhase::ListSnaps;
        StepResult::Continue
    }

    /// P3、P4。
    fn newcomers(&mut self, core: &mut Core) -> StepResult {
        let mut dropped = Vec::new();
        for (pid, rec) in &self.map {
            if self.listed.contains_key(pid) {
                continue;
            }
            match rec.status {
                Status::Live | Status::Condemned { .. } => {
                    if self.hold_active && rec.held {
                        dropped.push(*pid);
                    } else {
                        let msg = format!("P3: pack {pid:?} ({:?}) is missing", rec.status);
                        return refuse(core, msg);
                    }
                }
                Status::Unreadable => dropped.push(*pid),
                Status::Tombstone { .. } => {}
            }
        }
        for pid in dropped {
            self.map.remove(&pid);
        }
        let next = self.g + 1;
        let fresh: Vec<(PackId, Listed)> = self
            .listed
            .iter()
            .filter(|(p, _)| !self.map.contains_key(p))
            .map(|(p, l)| (*p, *l))
            .collect();
        for (pid, l) in fresh {
            let Some(body) = read_pack(core, l.client, pid) else {
                continue;
            };
            match newcomer_record(&body, l.client, next) {
                Ok(rec) => {
                    self.map.insert(pid, rec);
                }
                Err(e) => return refuse(core, e),
            }
        }
        self.phase = if self.snaps.is_empty() {
            PPhase::Plan
        } else {
            PPhase::Walk(0)
        };
        StepResult::Continue
    }

    /// P5：一次走訪一個 snapshot。
    fn walk(&mut self, core: &mut Core, i: usize) -> StepResult {
        let Some(key) = self.snaps.get(i).copied() else {
            self.phase = PPhase::Plan;
            return StepResult::Continue;
        };
        let body = read_snap(core, &Key::Snap(key));
        let r1 = read_snap(core, &Key::SnapR1(key));
        if body.is_some() || r1.is_some() {
            let readable = [&body, &r1]
                .into_iter()
                .flatten()
                .find(|s| s.readable)
                .cloned();
            match readable {
                None => return self.bad_snapshot(core, key),
                Some(s) => {
                    if s.version != VERSION {
                        return refuse(
                            core,
                            format!("P5: snapshot {key:?} has unknown version {}", s.version),
                        );
                    }
                    if let Err(e) = self.walk_dag(core, s.root) {
                        return refuse(core, e);
                    }
                }
            }
        }
        self.phase = if i + 1 < self.snaps.len() {
            PPhase::Walk(i + 1)
        } else {
            PPhase::Plan
        };
        StepResult::Continue
    }

    fn bad_snapshot(&mut self, core: &mut Core, key: SnapKey) -> StepResult {
        let Some(head) = self.head.clone() else {
            return refuse(core, "P5: no head".into());
        };
        if let Some(b) = head.bad.iter().find(|b| b.key == key).copied() {
            let Some((t_first, _)) = self.gen_times(core, b.first) else {
                return refuse(core, format!("P5: head {} is missing", b.first));
            };
            // 比 P0 探針的後端時間，不比 T(g)：等待期間每次 prune 都停在這裡、不寫新的 head，
            // T(g) 不會前進。
            if t_first + head.settings.grace <= self.probe_time {
                core.note(format!("P5: ignoring undecryptable snapshot {key:?}"));
                self.bad_ignored.push(key);
                let i = self.snaps.iter().position(|s| *s == key).unwrap_or(0);
                self.phase = if i + 1 < self.snaps.len() {
                    PPhase::Walk(i + 1)
                } else {
                    PPhase::Plan
                };
                return StepResult::Continue;
            }
            return refuse(
                core,
                format!(
                    "P5: undecryptable snapshot {key:?}, recorded at {}",
                    b.first
                ),
            );
        }
        // 第一次看到：以柵欄寫一個只記錄它的世代，然後停止。
        let mut nh = head;
        nh.g = self.g + 1;
        nh.bad.push(Bad {
            key,
            first: self.g + 1,
        });
        core.stats.prune_refusals += 1;
        core.note(format!("P5: first sight of undecryptable snapshot {key:?}"));
        self.new_head = Some(nh);
        self.bad_only = true;
        self.phase = PPhase::PutHead;
        StepResult::Continue
    }

    fn walk_dag(&mut self, core: &mut Core, root: ItemId) -> Result<(), String> {
        let nf = core.cfg.mutations.newcomer_first;
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !self.referenced.insert(id) {
                continue;
            }
            let kind = core.universe.kind(id);
            let mut holders: Vec<((u8, u32, PackId), PackId, ClientId)> = self
                .map
                .iter()
                .filter(|(p, r)| {
                    r.kind == kind
                        && r.entries.contains(&id)
                        && matches!(r.status, Status::Live | Status::Condemned { .. })
                        && self.listed.contains_key(p)
                })
                .map(|(p, r)| (rank(r, *p, nf), *p, r.client))
                .collect();
            holders.sort();
            if holders.is_empty() {
                return Err(format!("P5: {id:?} has no readable holder"));
            }
            if kind == Kind::Meta {
                let mut found = false;
                for (_, p, c) in holders {
                    match fetch(core, c, p, id) {
                        Copy::Genuine => {
                            found = true;
                            break;
                        }
                        Copy::False => {
                            if let Some(r) = self.map.get_mut(&p) {
                                r.entries.remove(&id);
                            }
                            core.stats.false_claims_removed += 1;
                            core.note(format!("P5: {p:?} falsely claims {id:?}"));
                        }
                        Copy::Missing => {}
                    }
                }
                if !found {
                    return Err(format!("P5: no holder of {id:?} could be read"));
                }
                stack.extend(core.universe.children(id));
            }
        }
        Ok(())
    }

    /// P6、P7。
    fn plan(&mut self, core: &mut Core) -> StepResult {
        let muts = core.cfg.mutations;
        let Some(head) = self.head.clone() else {
            return refuse(core, "P7: no head".into());
        };
        let grace = head.settings.grace;
        let next = self.g + 1;
        let mut newmap = self.map.clone();

        let mut aged_set = BTreeSet::new();
        let condemned: Vec<(PackId, Gen, Time)> = newmap
            .iter()
            .filter_map(|(p, r)| match r.status {
                Status::Condemned { at, age } => Some((*p, at, age)),
                _ => None,
            })
            .collect();
        for (p, at, age) in condemned {
            match self.aged(core, at, age) {
                Some(true) => {
                    aged_set.insert(p);
                }
                Some(false) => {}
                None => return refuse(core, format!("P7: head {at} is missing")),
            }
        }

        // P6 與刪前驗證：驗證時發現不實的項目就移除，再重算正本，直到沒有變動。
        let mut verdicts: BTreeMap<(ItemId, PackId), Copy> = BTreeMap::new();
        let canon = loop {
            let canon = canonical(
                &newmap,
                &self.referenced,
                &core.universe,
                &self.listed,
                muts.newcomer_first,
            );
            if self.hold_active || muts.no_verify_before_drop {
                break canon;
            }
            let canon_of = invert(&canon);
            let mut removals = Vec::new();
            for (pid, rec) in &newmap {
                if !aged_set.contains(pid) || canon_of.contains_key(pid) {
                    continue;
                }
                for id in rec.entries.intersection(&self.referenced) {
                    let Some(c) = canon.get(id) else {
                        continue;
                    };
                    let Some(crec) = newmap.get(c) else {
                        continue;
                    };
                    if crec.by_prune {
                        continue;
                    }
                    let v = *verdicts
                        .entry((*id, *c))
                        .or_insert_with(|| fetch(core, crec.client, *c, *id));
                    if v == Copy::False {
                        removals.push((*id, *c));
                    }
                }
            }
            if removals.is_empty() {
                break canon;
            }
            for (id, c) in removals {
                if let Some(r) = newmap.get_mut(&c) {
                    if r.entries.remove(&id) {
                        core.stats.false_claims_removed += 1;
                        core.note(format!("P7d: {c:?} falsely claims {id:?}"));
                    }
                }
                verdicts.remove(&(id, c));
            }
        };
        let canon_of = invert(&canon);

        let mut removed = Vec::new();
        let mut repack_cands = Vec::new();
        let ids: Vec<PackId> = newmap.keys().copied().collect();
        for pid in ids {
            let Some(rec) = newmap.get(&pid).cloned() else {
                continue;
            };
            let listed = self.listed.get(&pid).copied();
            let new_status = match rec.status {
                Status::Condemned { .. } => {
                    if canon_of.contains_key(&pid) {
                        if self.hold_active && rec.held {
                            None
                        } else {
                            core.stats.revived += 1;
                            Some(Status::Live)
                        }
                    } else if !self.hold_active && aged_set.contains(&pid) {
                        let ok = muts.no_verify_before_drop
                            || rec.entries.intersection(&self.referenced).all(|id| {
                                match canon.get(id) {
                                    None => false,
                                    Some(c) => {
                                        newmap.get(c).is_some_and(|cr| cr.by_prune)
                                            || verdicts.get(&(*id, *c)) == Some(&Copy::Genuine)
                                    }
                                }
                            });
                        if ok {
                            core.stats.tombstoned += 1;
                            Some(Status::Tombstone { at: next })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Status::Live => {
                    if !canon_of.contains_key(&pid) {
                        match listed {
                            Some(l) if l.mtime + grace <= self.n_t => {
                                core.stats.condemned += 1;
                                Some(Status::Condemned {
                                    at: next,
                                    age: self.age,
                                })
                            }
                            _ => None,
                        }
                    } else {
                        let sparse = canon_of
                            .get(&pid)
                            .is_some_and(|c| c.len() < rec.entries.len());
                        let old = listed.is_some_and(|l| l.mtime + grace <= self.n_t);
                        if !self.hold_active && sparse && old {
                            repack_cands.push(pid);
                        }
                        None
                    }
                }
                Status::Unreadable => {
                    if rec.since <= self.g {
                        match self.gen_times(core, rec.since) {
                            Some((t_s, _)) if t_s + grace <= self.n_t => {
                                core.stats.condemned += 1;
                                Some(Status::Condemned {
                                    at: next,
                                    age: self.age,
                                })
                            }
                            _ => None,
                        }
                    } else {
                        None
                    }
                }
                Status::Tombstone { at } => {
                    if listed.is_some() {
                        Some(Status::Tombstone { at: next })
                    } else {
                        let aged = self
                            .gen_times(core, at)
                            .is_some_and(|(t_d, _)| t_d + grace <= self.n_t);
                        let named = self.deltas.iter().any(|(_, b)| {
                            b.as_ref()
                                .is_some_and(|d| d.packs.iter().any(|dp| dp.pack == pid))
                        });
                        if aged && !named {
                            removed.push(pid);
                        }
                        None
                    }
                }
            };
            if let Some(s) = new_status {
                if let Some(r) = newmap.get_mut(&pid) {
                    r.status = s;
                }
            }
        }
        for pid in removed {
            newmap.remove(&pid);
        }

        // P7c：repack 的候選。
        for pid in repack_cands {
            let take = match core.cfg.repack {
                crate::core::RepackPolicy::Never => false,
                crate::core::RepackPolicy::Always => true,
                crate::core::RepackPolicy::Random => core.below(3) == 0,
            };
            if !take {
                continue;
            }
            let Some(rec) = newmap.get(&pid).cloned() else {
                continue;
            };
            let Some(items) = canon_of.get(&pid).cloned() else {
                continue;
            };
            let mut good = BTreeSet::new();
            let mut missing = false;
            for id in items {
                match fetch(core, rec.client, pid, id) {
                    Copy::Genuine => {
                        good.insert(id);
                    }
                    Copy::False => {
                        if let Some(r) = newmap.get_mut(&pid) {
                            r.entries.remove(&id);
                        }
                        core.stats.false_claims_removed += 1;
                    }
                    Copy::Missing => missing = true,
                }
            }
            if !missing && !good.is_empty() {
                self.repacks.push(Repack {
                    old: pid,
                    kind: rec.kind,
                    items: good,
                    new: None,
                });
            }
        }

        let mut nh = head;
        nh.g = next;
        if let Some(d) = nh.drain {
            if self
                .gen_times(core, d.from)
                .is_some_and(|(t_from, _)| self.n_t >= t_from + d.old_grace)
            {
                nh.drain = None;
            }
        }
        if nh.hold.is_some() && !self.hold_active {
            nh.hold = None;
            for r in newmap.values_mut() {
                r.held = false;
            }
        }
        self.newmap = newmap;
        self.new_head = Some(nh);
        self.phase = if self.repacks.is_empty() {
            PPhase::PutShards
        } else {
            PPhase::RepackPut(0)
        };
        StepResult::Continue
    }

    fn repack_put(&mut self, core: &mut Core, i: usize) -> StepResult {
        let replicas = self.head.as_ref().map_or(0, |h| h.settings.replicas);
        let Some(job) = self.repacks.get_mut(i) else {
            self.phase = PPhase::PutShards;
            return StepResult::Continue;
        };
        let id = PackId(core.fresh_id());
        let body = PackBody {
            client: MAINTENANCE,
            kind: job.kind,
            trailer: Trailer::Readable {
                version: VERSION,
                kind: job.kind,
                entries: job.items.clone(),
            },
            copies: job.items.iter().map(|i| (*i, true)).collect(),
        };
        let _ = core.put(pack_key(id), Content::Pack(body.clone()));
        if job.kind == Kind::Meta && replicas == 1 {
            let _ = core.put(
                Key::PackR1 {
                    client: MAINTENANCE,
                    id,
                },
                Content::Pack(body),
            );
        }
        job.new = Some(id);
        self.phase = PPhase::RepackCheck(i);
        StepResult::Continue
    }

    /// 讀回驗證；失敗就把新 pack 記為 unreadable，舊的維持 live。
    fn repack_check(&mut self, core: &mut Core, i: usize) -> StepResult {
        let next = self.g + 1;
        if let Some(job) = self.repacks.get(i).cloned() {
            if let Some(new) = job.new {
                let fail = std::mem::take(&mut core.fail_next_readback);
                let ok = !fail
                    && read_pack(core, MAINTENANCE, new).is_some_and(|p| {
                        job.items.iter().all(|it| p.copies.get(it) == Some(&true))
                    });
                if ok {
                    self.newmap.insert(
                        new,
                        Record {
                            client: MAINTENANCE,
                            kind: job.kind,
                            entries: job.items.clone(),
                            by_prune: true,
                            since: next,
                            status: Status::Live,
                            held: false,
                        },
                    );
                    if let Some(r) = self.newmap.get_mut(&job.old) {
                        r.status = Status::Condemned {
                            at: next,
                            age: self.age,
                        };
                    }
                    core.stats.repacked += 1;
                } else {
                    self.newmap.insert(
                        new,
                        Record {
                            client: MAINTENANCE,
                            kind: job.kind,
                            entries: BTreeSet::new(),
                            by_prune: false,
                            since: next,
                            status: Status::Unreadable,
                            held: false,
                        },
                    );
                    core.note(format!("P7c: read-back of {new:?} failed"));
                }
            }
        }
        self.phase = if i + 1 < self.repacks.len() {
            PPhase::RepackPut(i + 1)
        } else {
            PPhase::PutShards
        };
        StepResult::Continue
    }

    /// P8：寫出有變動的分片。
    fn put_shards(&mut self, core: &mut Core) -> StepResult {
        let Some(mut nh) = self.new_head.take() else {
            return refuse(core, "P8: no plan".into());
        };
        for s in 0..SHARDS {
            let new_recs: Map = self
                .newmap
                .iter()
                .filter(|(p, _)| shard_of(**p) == s)
                .map(|(p, r)| (*p, r.clone()))
                .collect();
            let old_recs: Map = self
                .base
                .iter()
                .filter(|(p, _)| shard_of(**p) == s)
                .map(|(p, r)| (*p, r.clone()))
                .collect();
            if new_recs == old_recs {
                continue;
            }
            if new_recs.is_empty() {
                nh.shards[s] = None;
                continue;
            }
            let sid = ShardId(core.fresh_id());
            let _ = core.put(
                Key::Shard(sid),
                Content::Shard(ShardBody {
                    index: s,
                    records: new_recs,
                }),
            );
            nh.shards[s] = Some(sid);
        }
        self.new_head = Some(nh);
        self.phase = PPhase::PutHead;
        StepResult::Continue
    }

    /// P8：時限內才以只在不存在時建立的方式寫 head g+1——柵欄。
    fn put_head(&mut self, core: &mut Core) -> StepResult {
        if self.out_of_time(core) {
            return refuse(core, "P8: out of time, not writing the head".into());
        }
        let Some(mut nh) = self.new_head.take() else {
            return refuse(core, "P8: no plan".into());
        };
        nh.w = core.host_wall(self.host);
        let g = nh.g;
        let won = core.put(Key::Head(g), Content::Head(nh.clone())).is_ok();
        self.new_head = Some(nh);
        if won {
            core.stats.generations += 1;
            core.note(format!("P8: host {} wrote head {g}", self.host));
        } else {
            core.stats.fence_losses += 1;
            if !core.cfg.mutations.no_fence {
                core.note(format!("P8: host {} lost the fence at {g}", self.host));
                return StepResult::Abort(format!("P8: head {g} already exists"));
            }
            core.note(format!(
                "P8: host {} lost the fence at {g} and goes on (mutation)",
                self.host
            ));
        }
        self.phase = PPhase::PutHeadR1;
        StepResult::Continue
    }

    fn put_head_r1(&mut self, core: &mut Core) -> StepResult {
        let replicas = self.head.as_ref().map_or(0, |h| h.settings.replicas);
        if let Some(nh) = &self.new_head {
            if replicas == 1 {
                let _ = core.put(Key::HeadR1(nh.g), Content::Head(nh.clone()));
            }
        }
        if self.bad_only {
            return StepResult::Done;
        }
        self.plan_deletions();
        self.phase = PPhase::Delete(0);
        StepResult::Continue
    }

    /// P9 的刪除清單。
    fn plan_deletions(&mut self) {
        let grace = self.grace();
        for (pid, rec) in &self.newmap {
            if !matches!(rec.status, Status::Tombstone { .. }) {
                continue;
            }
            if let Some(l) = self.listed.get(pid) {
                if l.r1 {
                    self.deletions.push(Key::PackR1 {
                        client: l.client,
                        id: *pid,
                    });
                }
                if l.body {
                    self.deletions.push(Key::Pack {
                        client: l.client,
                        id: *pid,
                    });
                }
            }
        }
        for (k, body) in &self.deltas {
            if let Some(d) = body {
                let absorbed = d.packs.iter().all(|dp| {
                    self.newmap.contains_key(&dp.pack) || !self.listed.contains_key(&dp.pack)
                });
                if absorbed {
                    self.deletions.push(*k);
                }
            }
        }
        let mut kept: BTreeSet<ShardId> = BTreeSet::new();
        if let Some(h) = &self.head {
            kept.extend(h.shards.iter().flatten());
        }
        if let Some(h) = &self.new_head {
            kept.extend(h.shards.iter().flatten());
        }
        for (sid, mtime) in &self.shard_list {
            if !kept.contains(sid) && mtime + grace <= self.n_t {
                self.deletions.push(Key::Shard(*sid));
            }
        }
        for key in &self.bad_ignored {
            self.deletions.push(Key::SnapR1(*key));
            self.deletions.push(Key::Snap(*key));
        }
    }

    /// P9：時限內逐一刪除。
    fn delete(&mut self, core: &mut Core, i: usize) -> StepResult {
        let Some(key) = self.deletions.get(i).copied() else {
            return StepResult::Done;
        };
        if self.out_of_time(core) {
            core.note(format!(
                "P9: host {} out of time, stopping deletions",
                self.host
            ));
            return StepResult::Abort("P9: out of time".into());
        }
        if core.backend.delete(&key) && matches!(key, Key::Pack { .. }) {
            core.stats.packs_deleted += 1;
        }
        self.phase = PPhase::Delete(i + 1);
        StepResult::Continue
    }
}

fn pack_key(id: PackId) -> Key {
    Key::Pack {
        client: MAINTENANCE,
        id,
    }
}
