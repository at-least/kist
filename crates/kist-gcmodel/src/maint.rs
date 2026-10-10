//! 維護角色的其他命令：forget、改 settings、重建（§9、§11.4）。

use std::collections::BTreeSet;

use crate::core::{list_heads, load_gen, load_highest, read_head, read_pack, Core};
use crate::prune::{list_packs, newcomer_record, probe_backend, refuse, Listed};
use crate::types::*;
use crate::world::StepResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FPhase {
    R1,
    Body,
}

/// forget：先刪 `.r1` 再刪主體。
#[derive(Clone, Debug)]
pub struct Forget {
    pub key: SnapKey,
    pub phase: FPhase,
}

impl Forget {
    pub fn step(&mut self, core: &mut Core) -> StepResult {
        match self.phase {
            FPhase::R1 => {
                core.backend.delete(&Key::SnapR1(self.key));
                self.phase = FPhase::Body;
                StepResult::Continue
            }
            FPhase::Body => {
                core.backend.delete(&Key::Snap(self.key));
                core.note(format!("forget {:?}", self.key));
                StepResult::Done
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SPhase {
    Probe,
    Load,
    PutHead,
}

/// 改 grace：一個不刪任何東西、走柵欄的世代。
#[derive(Clone, Debug)]
pub struct SettingsChange {
    pub host: u8,
    pub phase: SPhase,
    target: Time,
    new_head: Option<HeadBody>,
}

impl SettingsChange {
    pub fn new(host: u8, target: Time) -> Self {
        SettingsChange {
            host,
            phase: SPhase::Probe,
            target: target.max(MIN_GRACE),
            new_head: None,
        }
    }

    pub fn step(&mut self, core: &mut Core) -> StepResult {
        match self.phase {
            SPhase::Probe => match probe_backend(core, self.host) {
                Ok(_) => {
                    self.phase = SPhase::Load;
                    StepResult::Continue
                }
                Err(e) => refuse(core, e),
            },
            SPhase::Load => {
                let loaded = match load_highest(core) {
                    Ok(l) => l,
                    Err(e) => return refuse(core, format!("settings: {e:?}")),
                };
                if loaded.t > core.host_wall(self.host) + M {
                    return refuse(core, "settings: newest head is in the future".into());
                }
                let mut nh = loaded.head.clone();
                let cur = nh.settings.grace;
                if self.target == cur {
                    return StepResult::Done;
                }
                if let Some(d) = nh.drain {
                    let Some((_, t_from)) = read_head(core, d.from) else {
                        return refuse(core, format!("settings: head {} is missing", d.from));
                    };
                    if loaded.t < t_from + d.old_grace {
                        return refuse(core, "settings: a grace drain is in progress".into());
                    }
                    nh.drain = None;
                }
                if self.target < cur {
                    nh.drain = Some(Drain {
                        from: loaded.g + 1,
                        old_grace: cur,
                    });
                }
                nh.settings.grace = self.target;
                nh.g = loaded.g + 1;
                self.new_head = Some(nh);
                self.phase = SPhase::PutHead;
                StepResult::Continue
            }
            SPhase::PutHead => {
                let Some(mut nh) = self.new_head.take() else {
                    return refuse(core, "settings: no plan".into());
                };
                nh.w = core.host_wall(self.host);
                let g = nh.g;
                if core.put(Key::Head(g), Content::Head(nh.clone())).is_err() {
                    core.stats.fence_losses += 1;
                    return StepResult::Abort(format!("settings: head {g} already exists"));
                }
                if nh.settings.replicas == 1 {
                    let _ = core.put(Key::HeadR1(g), Content::Head(nh.clone()));
                }
                core.stats.generations += 1;
                core.note(format!("settings: grace {} at head {g}", nh.settings.grace));
                StepResult::Done
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RPhase {
    Probe,
    Load,
    Build,
    PutShards,
    PutHead,
}

/// 重建（§11.4）：最新 head 的分片遺失時，以柵欄寫 `g_lost + 1`。
#[derive(Clone, Debug)]
pub struct Rebuild {
    pub host: u8,
    pub phase: RPhase,
    start: Time,
    g_lost: Gen,
    g_r: Gen,
    case_keep: bool,
    base: Map,
    head: Option<HeadBody>,
    newmap: Map,
    new_head: Option<HeadBody>,
}

impl Rebuild {
    pub fn new(host: u8) -> Self {
        Rebuild {
            host,
            phase: RPhase::Probe,
            start: 0,
            g_lost: 0,
            g_r: 0,
            case_keep: false,
            base: Map::new(),
            head: None,
            newmap: Map::new(),
            new_head: None,
        }
    }

    pub fn step(&mut self, core: &mut Core) -> StepResult {
        match self.phase {
            RPhase::Probe => match probe_backend(core, self.host) {
                Ok(_) => {
                    self.start = core.now;
                    self.phase = RPhase::Load;
                    StepResult::Continue
                }
                Err(e) => refuse(core, e),
            },
            RPhase::Load => self.load(core),
            RPhase::Build => self.build(core),
            RPhase::PutShards => self.put_shards(core),
            RPhase::PutHead => self.put_head(core),
        }
    }

    fn load(&mut self, core: &mut Core) -> StepResult {
        let gens = list_heads(core);
        let Some(g_lost) = gens.last().copied() else {
            return refuse(core, "rebuild: no head".into());
        };
        if load_gen(core, g_lost).is_ok() {
            return StepResult::Done;
        }
        let Some(g_r) = gens
            .iter()
            .rev()
            .copied()
            .find(|g| load_gen(core, *g).is_ok())
        else {
            return refuse(core, "rebuild: no readable map".into());
        };
        let Some((lost_head, t_lost)) = read_head(core, g_lost) else {
            return refuse(core, "rebuild: newest head unreadable".into());
        };
        if lost_head.version != VERSION {
            return refuse(core, "rebuild: unknown head version".into());
        }
        // a：g_r+1 到 g_lost 的 head 裡最小的 grace（drain 的舊 grace 也算）；hold 的 grace 取最大。
        let mut min_age = Time::MAX;
        let mut max_grace = 0;
        for g in g_r..=g_lost {
            let Some((h, _)) = read_head(core, g) else {
                return refuse(core, format!("rebuild: head {g} is missing"));
            };
            max_grace = max_grace.max(h.settings.grace);
            if g > g_r {
                min_age = min_age.min(h.settings.grace);
                if let Some(d) = h.drain {
                    min_age = min_age.min(d.old_grace);
                }
            }
        }
        let Some((_, t_r1)) = read_head(core, g_r + 1) else {
            return refuse(core, "rebuild: head g_r+1 is missing".into());
        };
        let loaded_r = match load_gen(core, g_r) {
            Ok(l) => l,
            Err(e) => return refuse(core, format!("rebuild: {e:?}")),
        };
        self.case_keep = t_lost < t_r1 + min_age;
        self.g_lost = g_lost;
        self.g_r = g_r;
        self.base = loaded_r.map;
        let mut nh = lost_head;
        let new_hold = Hold {
            lost: g_lost,
            grace: max_grace,
        };
        nh.hold = match nh.hold {
            Some(old) => {
                let old_end = read_head(core, old.lost).map_or(Time::MIN, |(_, t)| t + old.grace);
                if old_end > t_lost + max_grace {
                    Some(old)
                } else {
                    Some(new_hold)
                }
            }
            None => Some(new_hold),
        };
        if core.cfg.mutations.no_hold {
            nh.hold = None;
        }
        nh.g = g_lost + 1;
        self.head = Some(nh);
        self.phase = RPhase::Build;
        StepResult::Continue
    }

    fn build(&mut self, core: &mut Core) -> StepResult {
        let r = self.g_lost + 1;
        let listed: Vec<(PackId, Listed)> = list_packs(core).into_iter().collect();
        let age = self
            .head
            .as_ref()
            .map_or(DEFAULT_GRACE, |h| h.settings.grace);
        let mut newmap = Map::new();
        if self.case_keep {
            for (pid, rec) in &self.base {
                let mut rec = rec.clone();
                if matches!(rec.status, Status::Condemned { .. }) {
                    rec.held = true;
                }
                newmap.insert(*pid, rec);
            }
        } else {
            for (pid, rec) in &self.base {
                if matches!(rec.status, Status::Tombstone { .. }) {
                    newmap.insert(*pid, rec.clone());
                }
            }
        }
        for (pid, l) in listed {
            if newmap.contains_key(&pid) {
                continue;
            }
            let Some(body) = read_pack(core, l.client, pid) else {
                continue;
            };
            let mut rec = match newcomer_record(&body, l.client, r) {
                Ok(rec) => rec,
                Err(e) => return refuse(core, format!("rebuild: {e}")),
            };
            if !self.case_keep && rec.status == Status::Live {
                rec.status = Status::Condemned { at: r, age };
                rec.held = true;
            }
            newmap.insert(pid, rec);
        }
        if core.cfg.mutations.no_hold {
            for rec in newmap.values_mut() {
                rec.held = false;
            }
        }
        self.newmap = newmap;
        self.phase = RPhase::PutShards;
        StepResult::Continue
    }

    fn put_shards(&mut self, core: &mut Core) -> StepResult {
        let Some(mut nh) = self.head.take() else {
            return refuse(core, "rebuild: no plan".into());
        };
        for s in 0..SHARDS {
            let recs: Map = self
                .newmap
                .iter()
                .filter(|(p, _)| shard_of(**p) == s)
                .map(|(p, r)| (*p, r.clone()))
                .collect();
            if recs.is_empty() {
                nh.shards[s] = None;
                continue;
            }
            let sid = ShardId(core.fresh_id());
            let _ = core.put(
                Key::Shard(sid),
                Content::Shard(ShardBody {
                    index: s,
                    records: recs,
                }),
            );
            nh.shards[s] = Some(sid);
        }
        self.new_head = Some(nh);
        self.phase = RPhase::PutHead;
        StepResult::Continue
    }

    fn put_head(&mut self, core: &mut Core) -> StepResult {
        let Some(mut nh) = self.new_head.take() else {
            return refuse(core, "rebuild: no plan".into());
        };
        if !core.cfg.mutations.no_run_bound && core.now - self.start >= nh.settings.grace - M {
            return refuse(core, "rebuild: out of time".into());
        }
        nh.w = core.host_wall(self.host);
        let g = nh.g;
        if core.put(Key::Head(g), Content::Head(nh.clone())).is_err() {
            core.stats.fence_losses += 1;
            return StepResult::Abort(format!("rebuild: head {g} already exists"));
        }
        if nh.settings.replicas == 1 {
            let _ = core.put(Key::HeadR1(g), Content::Head(nh.clone()));
        }
        core.stats.generations += 1;
        core.stats.rebuilds += 1;
        let kept: BTreeSet<PackId> = self.newmap.keys().copied().collect();
        core.note(format!(
            "rebuild: head {g} (g_lost {}, g_r {}, keep {}, records {})",
            self.g_lost,
            self.g_r,
            self.case_keep,
            kept.len()
        ));
        StepResult::Done
    }
}
