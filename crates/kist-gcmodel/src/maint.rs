//! 維護角色的其他命令：forget、改 settings、重建（§9、§11.4）。

use std::collections::BTreeSet;

use crate::core::{
    list_heads, load_gen, load_highest, now_pair, read_head, read_pack, window_passed, Core,
};
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
    RepairHead,
    PutHead,
    PutHeadR1,
}

/// 改 grace：一個不刪任何東西、走柵欄的世代。
#[derive(Clone, Debug)]
pub struct SettingsChange {
    pub host: u8,
    pub phase: SPhase,
    target: Time,
    /// P0 的後端時間與本機牆鐘。
    probe: (Time, Time),
    loaded_at: Time,
    head_put_at: Time,
    base_head: Option<HeadBody>,
    new_head: Option<HeadBody>,
}

impl SettingsChange {
    /// 已寫 head 主體、還沒寫 `.r1` 的世代。
    pub fn pending_head_r1(&self) -> Option<Gen> {
        match self.phase {
            SPhase::RepairHead => self.base_head.as_ref().map(|h| h.g),
            SPhase::PutHeadR1 => self.new_head.as_ref().map(|h| h.g),
            _ => None,
        }
    }

    pub fn new(host: u8, target: Time) -> Self {
        SettingsChange {
            host,
            phase: SPhase::Probe,
            target: target.max(MIN_GRACE),
            probe: (0, 0),
            loaded_at: 0,
            head_put_at: 0,
            base_head: None,
            new_head: None,
        }
    }

    pub fn step(&mut self, core: &mut Core) -> StepResult {
        match self.phase {
            SPhase::Probe => match probe_backend(core, self.host) {
                Ok(t) => {
                    self.probe = (t, core.host_wall(self.host));
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
                let now_t = loaded.t.min(core.host_wall(self.host) + M);
                self.loaded_at = core.now;
                let mut nh = loaded.head.clone();
                let cur = nh.settings.grace;
                if self.target == cur {
                    return StepResult::Done;
                }
                if let Some(d) = nh.drain {
                    let Some((h_from, t_from)) = read_head(core, d.from) else {
                        return refuse(core, format!("settings: head {} is missing", d.from));
                    };
                    let muts = core.cfg.mutations;
                    let over = window_passed(
                        muts,
                        (t_from, h_from.w),
                        now_pair(muts, (now_t, loaded.head.w), self.probe),
                        d.old_grace,
                    );
                    if !over {
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
                self.base_head = Some(loaded.head);
                self.new_head = Some(nh);
                self.phase = SPhase::RepairHead;
                StepResult::Continue
            }
            SPhase::RepairHead => {
                let Some(h) = &self.base_head else {
                    return refuse(core, "settings: no head to repair".into());
                };
                if !core.repair_head_r1(&h.clone(), self.loaded_at) {
                    return StepResult::Abort(format!(
                        "settings: the .r1 of head {} was not written",
                        h.g
                    ));
                }
                self.phase = SPhase::PutHead;
                StepResult::Continue
            }
            SPhase::PutHead => {
                let Some(mut nh) = self.new_head.take() else {
                    return refuse(core, "settings: no plan".into());
                };
                nh.w = core.host_wall(self.host);
                let g = nh.g;
                if !core.fence_put(g, &nh, self.loaded_at) {
                    core.stats.fence_losses += 1;
                    return StepResult::Abort(format!("settings: head {g} already exists"));
                }
                self.head_put_at = core.now;
                core.stats.generations += 1;
                core.note(format!("settings: grace {} at head {g}", nh.settings.grace));
                self.new_head = Some(nh);
                self.phase = SPhase::PutHeadR1;
                StepResult::Continue
            }
            SPhase::PutHeadR1 => {
                let Some(nh) = &self.new_head else {
                    return refuse(core, "settings: no head to replicate".into());
                };
                if core.head_r1_put_fails(nh.g) {
                    return StepResult::Abort(format!(
                        "settings: the .r1 of head {} was not written",
                        nh.g
                    ));
                }
                let _ = core.put_since(
                    Key::HeadR1(nh.g),
                    Content::Head(nh.clone()),
                    self.head_put_at,
                );
                StepResult::Done
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RPhase {
    Probe,
    Load,
    RepairHead,
    Build,
    PutShards,
    PutHead,
    PutHeadR1,
}

/// 重建（§11.4）：最新 head 的分片遺失時，以柵欄寫 `g_lost + 1`。
#[derive(Clone, Debug)]
pub struct Rebuild {
    pub host: u8,
    pub phase: RPhase,
    start: Time,
    /// P0 的後端時間與本機牆鐘。
    probe: (Time, Time),
    loaded_at: Time,
    head_put_at: Time,
    g_lost: Gen,
    g_r: Gen,
    case_keep: bool,
    age: Time,
    base: Map,
    /// 讀到的 `g_lost` 的 head，補寫它的 `.r1` 用。
    base_head: Option<HeadBody>,
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
            probe: (0, 0),
            loaded_at: 0,
            head_put_at: 0,
            g_lost: 0,
            g_r: 0,
            case_keep: false,
            age: DEFAULT_GRACE,
            base: Map::new(),
            base_head: None,
            head: None,
            newmap: Map::new(),
            new_head: None,
        }
    }

    pub fn step(&mut self, core: &mut Core) -> StepResult {
        match self.phase {
            RPhase::Probe => match probe_backend(core, self.host) {
                Ok(t) => {
                    self.probe = (t, core.host_wall(self.host));
                    self.start = core.now;
                    self.phase = RPhase::Load;
                    StepResult::Continue
                }
                Err(e) => refuse(core, e),
            },
            RPhase::Load => self.load(core),
            RPhase::RepairHead => {
                let Some(h) = &self.base_head else {
                    return refuse(core, "rebuild: no head to repair".into());
                };
                if !core.repair_head_r1(&h.clone(), self.loaded_at) {
                    return StepResult::Abort(format!(
                        "rebuild: the .r1 of head {} was not written",
                        h.g
                    ));
                }
                self.phase = RPhase::Build;
                StepResult::Continue
            }
            RPhase::Build => self.build(core),
            RPhase::PutShards => self.put_shards(core),
            RPhase::PutHead => self.put_head(core),
            RPhase::PutHeadR1 => {
                let Some(nh) = &self.new_head else {
                    return refuse(core, "rebuild: no head to replicate".into());
                };
                if core.head_r1_put_fails(nh.g) {
                    return StepResult::Abort(format!(
                        "rebuild: the .r1 of head {} was not written",
                        nh.g
                    ));
                }
                let _ = core.put_since(
                    Key::HeadR1(nh.g),
                    Content::Head(nh.clone()),
                    self.head_put_at,
                );
                StepResult::Done
            }
        }
    }

    fn load(&mut self, core: &mut Core) -> StepResult {
        self.loaded_at = core.now;
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
        // a：g_r 到 g_lost 的 head 裡最小的 grace（drain 的舊 grace 也算，g_r 是第一個標記者的
        // 基礎世代）；hold 的 grace 取最大。遺失的世代 g_r+1..g_lost 各自的 T 與 w 取最小、最大。
        let mut min_age = Time::MAX;
        let mut max_grace = 0;
        let (mut t_lo, mut t_hi, mut w_lo, mut w_hi) = (Time::MAX, Time::MIN, Time::MAX, Time::MIN);
        for g in g_r..=g_lost {
            let Some((h, t)) = read_head(core, g) else {
                return refuse(core, format!("rebuild: head {g} is missing"));
            };
            max_grace = max_grace.max(h.settings.grace);
            min_age = min_age.min(h.settings.grace);
            if let Some(d) = h.drain {
                min_age = min_age.min(d.old_grace);
            }
            if g > g_r {
                t_lo = t_lo.min(t);
                t_hi = t_hi.max(t);
                w_lo = w_lo.min(h.w);
                w_hi = w_hi.max(h.w);
            }
        }
        let Some((_, t_r1)) = read_head(core, g_r + 1) else {
            return refuse(core, "rebuild: head g_r+1 is missing".into());
        };
        let loaded_r = match load_gen(core, g_r) {
            Ok(l) => l,
            Err(e) => return refuse(core, format!("rebuild: {e:?}")),
        };
        // 標成 tombstone 要兩個時鐘都過了一個 age；整段遺失的世代在 w 上都不到一個 age，就不可能有
        // Map(g_r) 裡 live 的 pack 被標成 tombstone（外部審查 F3）。只看 w：T 是物件的修改時間，主體
        // 遺失時改讀 `.r1`、主體被 check --repair 重建時都會變，標 tombstone 那時讀到的 T 和現在讀到的
        // 可能不同；w 在 head 的內容裡，兩份相同（第二輪外部審查 D1）。
        let muts = core.cfg.mutations;
        self.case_keep = if muts.one_clock_windows {
            t_lost < t_r1 + min_age
        } else if muts.keep_rule_t_clause {
            t_hi - t_lo < min_age || w_hi - w_lo < min_age
        } else {
            w_hi - w_lo < min_age
        };
        self.g_lost = g_lost;
        self.g_r = g_r;
        self.base = loaded_r.map;
        self.base_head = Some(lost_head.clone());
        let mut nh = lost_head;
        // 案例 B 的 age：drain 還沒結束（兩個時鐘）就用舊的 grace。
        self.age = nh.settings.grace;
        if let Some(d) = nh.drain {
            if let Some((h_from, t_from)) = read_head(core, d.from) {
                let over = window_passed(
                    muts,
                    (t_from, h_from.w),
                    now_pair(muts, (t_lost, nh.w), self.probe),
                    d.old_grace,
                );
                if !over && !core.cfg.mutations.no_drain {
                    self.age = d.old_grace;
                }
            }
        }
        let new_hold = Hold {
            from: g_r + 1,
            lost: g_lost,
            grace: max_grace,
        };
        // 已有 hold 就合併：涵蓋兩段遺失的世代、取較大的 grace。
        nh.hold = Some(match nh.hold {
            Some(old) => Hold {
                from: old.from.min(new_hold.from),
                lost: old.lost.max(new_hold.lost),
                grace: old.grace.max(new_hold.grace),
            },
            None => new_hold,
        });
        if core.cfg.mutations.no_hold {
            nh.hold = None;
        }
        nh.g = g_lost + 1;
        self.head = Some(nh);
        self.phase = RPhase::RepairHead;
        StepResult::Continue
    }

    fn build(&mut self, core: &mut Core) -> StepResult {
        let r = self.g_lost + 1;
        let listed: Vec<(PackId, Listed)> = list_packs(core).into_iter().collect();
        let age = self.age;
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
            let mut rec = newcomer_record(&body, l.client, r);
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
            let _ = core.put_since(
                Key::Shard(sid),
                Content::Shard(ShardBody {
                    index: s,
                    records: recs,
                }),
                self.loaded_at,
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
        if !core.fence_put(g, &nh, self.loaded_at) {
            core.stats.fence_losses += 1;
            return StepResult::Abort(format!("rebuild: head {g} already exists"));
        }
        self.head_put_at = core.now;
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
        self.new_head = Some(nh);
        self.phase = RPhase::PutHeadR1;
        StepResult::Continue
    }

    /// 已寫 head 主體、還沒寫 `.r1` 的世代。
    pub fn pending_head_r1(&self) -> Option<Gen> {
        match self.phase {
            RPhase::RepairHead => self.base_head.as_ref().map(|h| h.g),
            RPhase::PutHeadR1 => self.new_head.as_ref().map(|h| h.g),
            _ => None,
        }
    }
}
