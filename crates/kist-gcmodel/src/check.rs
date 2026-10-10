//! 不變式（§11.5），每一步之後檢查。

use std::collections::BTreeSet;

use crate::core::{fetch, load_highest, read_deltas, Copy, Core};
use crate::reader::candidates;
use crate::types::*;

/// 回傳違反的不變式（空的＝全部成立）：
/// - R1：誠實 client commit 過、還沒被 forget 的 snapshot，引用的每個 item 都還有真副本；
/// - 最新的 head 讀得全（故障拿掉分片、還沒重建的期間除外）；
/// - 找得到：照讀取端的規則（最新 head 的 map 與當下的 delta、依排序、驗證失敗換下一份），每個
///   item 都找得到真副本（分片故障期間除外）；
/// - restore actor 沒有在 snapshot 還在時讀失敗；
/// - 沒有 P3 的「資料遺失」拒絕（模型沒有遺失 pack 的故障，出現就是誤報）。
pub fn check(core: &Core) -> Vec<String> {
    let mut out = Vec::new();
    let mut genuine: BTreeSet<ItemId> = BTreeSet::new();
    for (key, obj) in core.backend.iter() {
        if let (Key::Pack { .. } | Key::PackR1 { .. }, Content::Pack(p)) = (key, &obj.content) {
            genuine.extend(p.copies.iter().filter(|(_, g)| **g).map(|(i, _)| *i));
        }
    }
    let committed: Vec<(SnapKey, ItemId)> = core
        .honest
        .iter()
        .filter_map(
            |k| match core.backend.get(&Key::Snap(*k)).map(|o| &o.content) {
                Some(Content::Snap(s)) => Some((*k, s.root)),
                _ => None,
            },
        )
        .collect();
    for (key, root) in &committed {
        for id in core.universe.reachable(*root) {
            if !genuine.contains(&id) {
                out.push(format!(
                    "R1: {key:?} references {id:?}, which has no genuine copy left"
                ));
            }
        }
    }
    out.extend(
        core.alarms
            .iter()
            .map(|a| format!("false data-loss alarm (no pack was lost by a fault): {a}")),
    );
    let newest = load_highest(core);
    if core.shard_fault {
        out.extend(core.reader_failures.iter().cloned());
        return out;
    }
    let loaded = match newest {
        Ok(l) => l,
        Err(e) => {
            out.push(format!("the newest head cannot be loaded: {e:?}"));
            return out;
        }
    };
    let deltas = read_deltas(core, &loaded.map);
    let nf = core.cfg.mutations.newcomer_first;
    for (key, root) in &committed {
        for id in core.universe.reachable(*root) {
            let kind = core.universe.kind(id);
            let found = candidates(&loaded.map, &deltas, kind, id, nf)
                .into_iter()
                .any(|(c, p)| fetch(core, c, p, id) == Copy::Genuine);
            if !found {
                out.push(format!(
                    "findability: {key:?} references {id:?}, but no holder in Map({}) or the current deltas has it",
                    loaded.g
                ));
            }
        }
    }
    out.extend(core.reader_failures.iter().cloned());
    out
}
