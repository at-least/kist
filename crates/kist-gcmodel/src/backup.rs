//! backup（§11.2 的 B1–B5）。

use std::collections::{BTreeMap, BTreeSet};

use crate::core::{fetch, load_highest, read_deltas, read_pack, readable_snap, Copy, Core};
use crate::types::*;
use crate::world::StepResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Map,
    Delta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Holder {
    pub client: ClientId,
    pub pack: PackId,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BPhase {
    Load,
    Plan,
    PutPack(usize),
    PutDelta(usize),
    Gate,
    GateHeads,
    PutSnapR1,
    PutSnap,
}

#[derive(Clone, Debug)]
pub struct Backup {
    pub client: ClientId,
    pub job: u8,
    pub variant: u8,
    pub phase: BPhase,
    start: Time,
    /// 經過時間的低估（時鐘＋snapshot PUT 的延遲），≤ `under_max`。
    under: Time,
    g_s: Gen,
    grace_s: Time,
    grace_c: Time,
    replicas: u8,
    map: Map,
    deltas: BTreeMap<PackId, DeltaPack>,
    parent: Option<SnapKey>,
    parent_items: BTreeSet<ItemId>,
    root: ItemId,
    reused: BTreeMap<ItemId, Holder>,
    used_subtree: bool,
    to_write: Vec<(Kind, Vec<ItemId>)>,
    written: Vec<(PackId, Kind, BTreeSet<ItemId>)>,
    since_delta: Vec<usize>,
    delta_every: usize,
    delta_checks: Vec<(ItemId, Holder)>,
    pub key: SnapKey,
}

impl Backup {
    pub fn new(core: &mut Core, client: ClientId, job: u8, variant: u8, under: Time) -> Self {
        let key = SnapKey {
            client,
            ts: core.fresh_id(),
        };
        let delta_every = 1 + core.below(3) as usize;
        Backup {
            client,
            job,
            variant,
            phase: BPhase::Load,
            start: core.now,
            under: under.clamp(0, core.cfg.under_max.max(0)),
            g_s: 0,
            grace_s: 0,
            grace_c: 0,
            replicas: 0,
            map: Map::new(),
            deltas: BTreeMap::new(),
            parent: None,
            parent_items: BTreeSet::new(),
            root: ItemId(0),
            reused: BTreeMap::new(),
            used_subtree: false,
            to_write: Vec::new(),
            written: Vec::new(),
            since_delta: Vec::new(),
            delta_every,
            delta_checks: Vec::new(),
            key,
        }
    }

    pub fn phase_name(&self) -> String {
        format!("{:?}", self.phase)
    }

    /// 量到的經過時間 E（§11.1）：真實經過減去低估。
    fn elapsed(&self, core: &Core) -> Time {
        core.now - self.start - self.under
    }

    fn too_long(&self, core: &Core, grace: Time) -> bool {
        !core.cfg.mutations.no_elapsed_check && self.elapsed(core) >= grace - M
    }

    pub fn step(&mut self, core: &mut Core) -> StepResult {
        match self.phase {
            BPhase::Load => self.load(core),
            BPhase::Plan => self.plan(core),
            BPhase::PutPack(i) => self.put_pack(core, i),
            BPhase::PutDelta(next) => self.put_delta(core, next),
            BPhase::Gate => self.gate(core),
            BPhase::GateHeads => self.gate_heads(core),
            BPhase::PutSnapR1 => self.put_snap_r1(core),
            BPhase::PutSnap => self.put_snap(core),
        }
    }

    /// B1。
    fn load(&mut self, core: &mut Core) -> StepResult {
        let loaded = match load_highest(core) {
            Ok(l) => l,
            Err(e) => return StepResult::Abort(format!("B1: cannot read the newest head: {e:?}")),
        };
        if core.lone_head(loaded.g) {
            return StepResult::Abort(format!("B1: head {} has no .r1 yet", loaded.g));
        }
        self.g_s = loaded.g;
        self.grace_s = loaded.head.settings.grace;
        self.replicas = loaded.head.settings.replicas;
        self.deltas = read_deltas(core, &loaded.map);
        self.map = loaded.map;
        // parent：自己在同一組 roots 上最新的 snapshot。
        let mine: Vec<SnapKey> = core
            .backend
            .keys()
            .filter_map(|k| match k {
                Key::Snap(s) if s.client == self.client => Some(*s),
                _ => None,
            })
            .collect();
        let mut best: Option<(SnapKey, ItemId)> = None;
        for s in mine {
            if let Some(body) = readable_snap(core, s) {
                if body.job == self.job && best.is_none_or(|(b, _)| s.ts > b.ts) {
                    best = Some((s, body.root));
                }
            }
        }
        if let Some((s, root)) = best {
            self.parent = Some(s);
            self.parent_items = core.universe.reachable(root);
        }
        self.root = core.universe.tree(self.job, self.variant);
        self.phase = BPhase::Plan;
        StepResult::Continue
    }

    /// B2：決定沿用還是重新寫。
    fn plan(&mut self, core: &mut Core) -> StepResult {
        let mut done = BTreeSet::new();
        let mut new_items = Vec::new();
        self.visit(core, self.root, &mut done, &mut new_items);
        let per = core.cfg.max_items_per_pack.max(1);
        for kind in [Kind::Data, Kind::Meta] {
            let items: Vec<ItemId> = new_items
                .iter()
                .copied()
                .filter(|i| core.universe.kind(*i) == kind)
                .collect();
            for chunk in items.chunks(per) {
                self.to_write.push((kind, chunk.to_vec()));
            }
        }
        self.phase = if self.to_write.is_empty() {
            BPhase::Gate
        } else {
            BPhase::PutPack(0)
        };
        StepResult::Continue
    }

    fn visit(
        &mut self,
        core: &mut Core,
        id: ItemId,
        done: &mut BTreeSet<ItemId>,
        new_items: &mut Vec<ItemId>,
    ) {
        if done.contains(&id) {
            return;
        }
        // 從 parent 沿用整棵子樹：只判斷子樹的根。
        if core.universe.kind(id) == Kind::Meta && self.parent_items.contains(&id) {
            if let Some(h) = self.trusted_holder(core, id) {
                self.reused.insert(id, h);
                self.used_subtree = true;
                done.extend(core.universe.reachable(id));
                return;
            }
        }
        for child in core.universe.children(id) {
            self.visit(core, child, done, new_items);
        }
        if let Some(h) = self.trusted_holder(core, id) {
            self.reused.insert(id, h);
        } else {
            new_items.push(id);
        }
        done.insert(id);
    }

    /// B2 的兩個條件：有種類相符的持有者，而且可信。
    fn trusted_holder(&mut self, core: &mut Core, id: ItemId) -> Option<Holder> {
        let kind = core.universe.kind(id);
        let muts = core.cfg.mutations;
        let mut cands: Vec<((u8, u32, PackId), Holder, bool)> = Vec::new();
        for (pid, rec) in &self.map {
            if rec.kind != kind || !rec.entries.contains(&id) {
                continue;
            }
            let usable = match rec.status {
                Status::Live => true,
                Status::Condemned { .. } => muts.reuse_condemned,
                _ => false,
            };
            if !usable {
                continue;
            }
            let holder = Holder {
                client: rec.client,
                pack: *pid,
                source: Source::Map,
            };
            cands.push((rank(rec, *pid, muts.newcomer_first), holder, rec.by_prune));
        }
        cands.sort_by_key(|c| c.0);
        for (pid, dp) in &self.deltas {
            if dp.kind == kind && dp.entries.contains(&id) {
                let holder = Holder {
                    client: dp.client,
                    pack: *pid,
                    source: Source::Delta,
                };
                cands.push(((9, 0, *pid), holder, false));
            }
        }
        for (_, h, by_prune) in cands {
            let verified = core
                .verified
                .get(&self.client)
                .is_some_and(|v| v.contains(&(id, h.pack)));
            if muts.trust_all || by_prune || h.client == self.client || verified {
                return Some(h);
            }
            if fetch(core, h.client, h.pack, id) == Copy::Genuine {
                core.verified
                    .entry(self.client)
                    .or_default()
                    .insert((id, h.pack));
                return Some(h);
            }
        }
        None
    }

    /// B3：寫 pack（mpack 連同 `.r1`）。
    fn put_pack(&mut self, core: &mut Core, i: usize) -> StepResult {
        let Some((kind, items)) = self.to_write.get(i).cloned() else {
            return StepResult::Abort("B3: no such pack".into());
        };
        let id = core.pack_id(false);
        let entries: BTreeSet<ItemId> = items.iter().copied().collect();
        let body = PackBody {
            client: self.client,
            kind,
            trailer: Trailer::Readable {
                version: VERSION,
                kind,
                entries: entries.clone(),
            },
            copies: items.iter().map(|i| (*i, true)).collect(),
        };
        if core
            .put(
                Key::Pack {
                    client: self.client,
                    id,
                },
                Content::Pack(body.clone()),
            )
            .is_err()
        {
            return StepResult::Abort("B3: pack name collision".into());
        }
        if kind == Kind::Meta && self.replicas == 1 {
            let _ = core.put(
                Key::PackR1 {
                    client: self.client,
                    id,
                },
                Content::Pack(body),
            );
        }
        self.written.push((id, kind, entries));
        self.since_delta.push(self.written.len() - 1);
        let last = i + 1 == self.to_write.len();
        self.phase = if last || self.since_delta.len() >= self.delta_every {
            BPhase::PutDelta(i + 1)
        } else {
            BPhase::PutPack(i + 1)
        };
        StepResult::Continue
    }

    /// B3：寫 delta；`E ≥ grace − M` 之後不再寫。
    fn put_delta(&mut self, core: &mut Core, next: usize) -> StepResult {
        if !self.too_long(core, self.grace_s) && !self.since_delta.is_empty() {
            let packs = self
                .since_delta
                .iter()
                .filter_map(|i| self.written.get(*i))
                .map(|(id, kind, entries)| DeltaPack {
                    client: self.client,
                    pack: *id,
                    kind: *kind,
                    entries: entries.clone(),
                })
                .collect();
            let id = DeltaId(core.fresh_id());
            let _ = core.put(
                Key::Delta {
                    client: self.client,
                    id,
                },
                Content::Delta(DeltaBody {
                    readable: true,
                    client: self.client,
                    packs,
                }),
            );
        }
        self.since_delta.clear();
        self.phase = if next < self.to_write.len() {
            BPhase::PutPack(next)
        } else {
            BPhase::Gate
        };
        StepResult::Continue
    }

    /// B4：緊接在寫 snapshot 之前。
    fn gate(&mut self, core: &mut Core) -> StepResult {
        let loaded = match load_highest(core) {
            Ok(l) => l,
            Err(e) => return StepResult::Abort(format!("B4: cannot read the newest head: {e:?}")),
        };
        if loaded.g < self.g_s {
            return StepResult::Abort("B4: generation went backwards".into());
        }
        self.grace_c = loaded.head.settings.grace;
        if self.too_long(core, self.grace_s.min(self.grace_c)) {
            return StepResult::Abort("B4: backup took too long".into());
        }
        let muts = core.cfg.mutations;
        self.delta_checks.clear();
        for (item, h) in &self.reused {
            match loaded.map.get(&h.pack) {
                Some(rec) => {
                    if matches!(
                        rec.status,
                        Status::Tombstone { .. } | Status::Unreadable | Status::Unknown
                    ) {
                        return StepResult::Abort(format!(
                            "B4: holder {:?} is {:?}",
                            h.pack, rec.status
                        ));
                    }
                    if loaded.g != self.g_s
                        && !muts.no_b4_entries_check
                        && !rec.entries.contains(item)
                    {
                        return StepResult::Abort(format!(
                            "B4: {:?} no longer claims {item:?}",
                            h.pack
                        ));
                    }
                }
                None => match h.source {
                    Source::Map => {
                        return StepResult::Abort(format!("B4: holder {:?} left the map", h.pack))
                    }
                    Source::Delta => self.delta_checks.push((*item, *h)),
                },
            }
        }
        for (id, _, _) in &self.written {
            if let Some(rec) = loaded.map.get(id) {
                if matches!(rec.status, Status::Tombstone { .. }) {
                    return StepResult::Abort(format!("B4: own pack {id:?} is a tombstone"));
                }
            }
        }
        self.phase = BPhase::GateHeads;
        StepResult::Continue
    }

    /// B4（續）：來自 delta 的 pack 做 HEAD 並讀 trailer；沿用了子樹就 HEAD parent。
    fn gate_heads(&mut self, core: &mut Core) -> StepResult {
        for (item, h) in &self.delta_checks {
            let Some(p) = read_pack(core, h.client, h.pack) else {
                return StepResult::Abort(format!("B4: delta pack {:?} is gone", h.pack));
            };
            let ok = match &p.trailer {
                Trailer::Readable {
                    version,
                    kind,
                    entries,
                } => *version == VERSION && *kind == p.kind && entries.contains(item),
                Trailer::Unreadable => false,
            };
            if !ok {
                return StepResult::Abort(format!(
                    "B4: delta pack {:?} does not hold {item:?}",
                    h.pack
                ));
            }
        }
        if self.used_subtree && !core.cfg.mutations.no_parent_head {
            if let Some(parent) = self.parent {
                if !core.backend.exists(&Key::Snap(parent)) {
                    return StepResult::Abort("B4: parent snapshot is gone".into());
                }
            }
        }
        self.phase = BPhase::PutSnapR1;
        StepResult::Continue
    }

    fn snap_body(&self) -> SnapBody {
        SnapBody {
            readable: true,
            version: VERSION,
            job: self.job,
            root: self.root,
            parent: self.parent,
        }
    }

    /// B5：E、`.r1`。
    fn put_snap_r1(&mut self, core: &mut Core) -> StepResult {
        if self.too_long(core, self.grace_s.min(self.grace_c)) {
            return StepResult::Abort("B5: backup took too long".into());
        }
        if self.replicas == 1 {
            let _ = core.put(Key::SnapR1(self.key), Content::Snap(self.snap_body()));
        }
        self.phase = BPhase::PutSnap;
        StepResult::Continue
    }

    /// B5：E、主體＝commit。
    fn put_snap(&mut self, core: &mut Core) -> StepResult {
        if self.too_long(core, self.grace_s.min(self.grace_c)) {
            return StepResult::Abort("B5: backup took too long".into());
        }
        if core
            .put(Key::Snap(self.key), Content::Snap(self.snap_body()))
            .is_err()
        {
            return StepResult::Abort("B5: snapshot name collision".into());
        }
        core.honest.insert(self.key);
        core.stats.commits += 1;
        core.note(format!(
            "commit {:?} job {} variant {} (g_s {}, reused {}, packs {}, subtree {})",
            self.key,
            self.job,
            self.variant,
            self.g_s,
            self.reused.len(),
            self.written.len(),
            self.used_subtree
        ));
        StepResult::Done
    }
}
