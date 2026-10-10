//! restore（§10 讀取端）：最大世代讀不了就退回較舊的；持有者依排序逐一嘗試，驗證失敗就試下一份；
//! 全部失敗就重新載入一次。

use std::collections::{BTreeMap, BTreeSet};

use crate::core::{fetch, load_with_fallback, read_deltas, readable_snap, Copy, Core};
use crate::types::*;
use crate::world::StepResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdPhase {
    Load,
    Snap,
    Items,
}

#[derive(Clone, Debug)]
pub struct Reader {
    pub key: SnapKey,
    pub phase: RdPhase,
    map: Map,
    deltas: BTreeMap<PackId, DeltaPack>,
    stack: Vec<ItemId>,
    seen: BTreeSet<ItemId>,
    reloaded: BTreeSet<ItemId>,
    fault_seen: bool,
}

/// 一個 item 的候選持有者，依讀取順序：live、condemned（各依排序）、delta 的 pack、tombstone。
pub fn candidates(
    map: &Map,
    deltas: &BTreeMap<PackId, DeltaPack>,
    universe_kind: Kind,
    id: ItemId,
    newcomer_first: bool,
) -> Vec<(ClientId, PackId)> {
    let mut ranked: Vec<((u8, u32, PackId), ClientId, PackId)> = map
        .iter()
        .filter(|(_, r)| {
            r.kind == universe_kind && r.entries.contains(&id) && r.status != Status::Unreadable
        })
        .map(|(p, r)| {
            let mut key = rank(r, *p, newcomer_first);
            if matches!(r.status, Status::Tombstone { .. }) {
                key.0 = 5;
            }
            (key, r.client, *p)
        })
        .collect();
    for (p, dp) in deltas {
        if dp.kind == universe_kind && dp.entries.contains(&id) {
            ranked.push(((4, 0, *p), dp.client, *p));
        }
    }
    ranked.sort();
    ranked.into_iter().map(|(_, c, p)| (c, p)).collect()
}

impl Reader {
    pub fn new(key: SnapKey) -> Self {
        Reader {
            key,
            phase: RdPhase::Load,
            map: Map::new(),
            deltas: BTreeMap::new(),
            stack: Vec::new(),
            seen: BTreeSet::new(),
            reloaded: BTreeSet::new(),
            fault_seen: false,
        }
    }

    fn reload(&mut self, core: &Core) {
        self.fault_seen |= core.shard_fault;
        match load_with_fallback(core) {
            Some(l) => {
                self.deltas = read_deltas(core, &l.map);
                self.map = l.map;
            }
            None => {
                self.map = Map::new();
                self.deltas = read_deltas(core, &self.map);
            }
        }
    }

    pub fn step(&mut self, core: &mut Core) -> StepResult {
        self.fault_seen |= core.shard_fault;
        match self.phase {
            RdPhase::Load => {
                self.reload(core);
                self.phase = RdPhase::Snap;
                StepResult::Continue
            }
            RdPhase::Snap => match readable_snap(core, self.key) {
                Some(s) => {
                    self.stack.push(s.root);
                    self.phase = RdPhase::Items;
                    StepResult::Continue
                }
                None => StepResult::Done,
            },
            RdPhase::Items => self.item(core),
        }
    }

    fn item(&mut self, core: &mut Core) -> StepResult {
        let Some(id) = self.stack.pop() else {
            core.stats.reads_ok += 1;
            return StepResult::Done;
        };
        if self.seen.contains(&id) {
            return StepResult::Continue;
        }
        let kind = core.universe.kind(id);
        let nf = core.cfg.mutations.newcomer_first;
        let found = candidates(&self.map, &self.deltas, kind, id, nf)
            .into_iter()
            .any(|(c, p)| fetch(core, c, p, id) == Copy::Genuine);
        if found {
            self.seen.insert(id);
            if kind == Kind::Meta {
                self.stack.extend(core.universe.children(id));
            }
            return StepResult::Continue;
        }
        if self.reloaded.insert(id) {
            self.reload(core);
            self.stack.push(id);
            return StepResult::Continue;
        }
        let committed = core.backend.exists(&Key::Snap(self.key));
        if committed && !self.fault_seen && !core.shard_fault && core.honest.contains(&self.key) {
            core.reader_failures
                .push(format!("restore of {:?} could not read {id:?}", self.key));
        }
        StepResult::Done
    }
}
