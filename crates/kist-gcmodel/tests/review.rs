//! 外部審查（2026-10-10，獨立的 agent，在 8dc76bd 上寫的）原樣搬進來當回歸測試；只改了 m1（見該測試
//! 的註解）。
//!
//! Adversarial review of the GC model. `f*_break` / `f4_*` / `f5_*` assert what the spec promises
//! and FAIL on the unmodified protocol (`Mutations::default()`, `quiet()`: eps 0, under 0, so
//! `eps_max + under_max < M`; `forced_forget_waits_for_idle = true`). `*_control` runs the same
//! interleaving without the fault and PASSES. `a*` are attacks that did not break it (PASS).
//! `m*` show the model differing from the spec (FAIL). `census_*` are ignored diagnostics.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::*;
use kist_gcmodel::*;

/// "" when R1 holds, else the count and the first violation.
fn r1(w: &World) -> String {
    let v: Vec<String> = w
        .check()
        .into_iter()
        .filter(|m| m.starts_with("R1"))
        .collect();
    v.first().map_or(String::new(), |f| {
        format!("{} R1 violations, first: {f}", v.len())
    })
}

fn map_of(w: &World, g: Gen) -> Option<Map> {
    let Some(Content::Head(h)) = w.core.backend.get(&Key::Head(g)).map(|o| &o.content) else {
        return None;
    };
    let mut map = Map::new();
    for sid in h.shards.iter().flatten() {
        match w.core.backend.get(&Key::Shard(*sid)).map(|o| &o.content) {
            Some(Content::Shard(s)) => map.extend(s.records.clone()),
            _ => return None,
        }
    }
    Some(map)
}

fn shards_of(w: &World, g: Gen) -> Vec<ShardId> {
    match w.core.backend.get(&Key::Head(g)).map(|o| &o.content) {
        Some(Content::Head(h)) => h.shards.iter().flatten().copied().collect(),
        _ => Vec::new(),
    }
}

fn t_of(w: &World, g: Gen) -> Time {
    w.core.backend.get(&Key::Head(g)).unwrap().mtime
}

fn newest_gen(w: &World) -> Gen {
    w.core
        .backend
        .keys()
        .filter_map(|k| if let Key::Head(g) = k { Some(*g) } else { None })
        .max()
        .unwrap()
}

/// Lose one shard object that heads `from..=lost` share and head `from - 1` does not.
fn lose_shared_shard(w: &mut World, from: Gen, lost: Gen) {
    let (older, first, newest) = (
        shards_of(w, from - 1),
        shards_of(w, from),
        shards_of(w, lost),
    );
    let pick = newest
        .iter()
        .position(|s| first.contains(s) && !older.contains(s))
        .expect("a shard shared by the lost heads");
    w.apply(&Action::LoseShard { pick: pick as u8 });
    assert!(w.core.shard_fault);
}

// ---- F1: the grace drain ends on the backend clock alone (§11.4 "直到 T(g) ≥ T(調低的世代) +
// 舊 grace"; §11.5 "後端時鐘偏慢 … 只會延後刪除"). Backend clock 4 h slow (after P0) while the
// settings command writes the lowering head (72 h → 2 h). A backup that passed B4 under 72 h
// reuses s0's packs; s0 is forgotten after the early drain end; the packs get the 2 h age.

fn drain(skew: Time) -> World {
    drain_with(quiet(), skew)
}

fn drain_with(cfg: Config, skew: Time) -> World {
    let mut w = World::new(cfg, 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1")); // B4 passed under grace 72 h
    let s = w.start_settings(1, 2 * HOUR).unwrap();
    assert_eq!(w.step(s), Some(StepResult::Continue)); // P0, honest backend clock
    assert_eq!(w.step(s), Some(StepResult::Continue)); // P1, plan
    w.core.backend.skew = skew;
    assert_eq!(w.step(s), Some(StepResult::Done)); // head with drain { old_grace: 72 h }
    w.core.backend.skew = 0;
    w.advance(68 * HOUR); // 148 h; an honest drain lasts until 152 h
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    forget(&mut w, s0);
    for h in [0, 2, 0] {
        w.advance(h * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
    }
    assert_eq!(w.finish(b), Some(StepResult::Done)); // E = 70 h < 72 h − M
    w
}

#[test]
fn f1_drain_ends_on_backend_clock_alone_break() {
    assert_eq!(r1(&drain(-4 * HOUR)), "");
}

#[test]
fn f1_drain_ends_on_backend_clock_alone_control() {
    assert_ok(&drain(0));
}

/// Boundary of F1 in this timeline: the early drain end must exceed new grace (2 h) + M, so a
/// 3 h slow backend clock (outside the random ±2 h range anyway) is not enough.
#[test]
fn a5_drain_survives_a_three_hour_slow_backend() {
    assert_ok(&drain(-3 * HOUR));
}

// ---- F2: the rebuild hold ends on the backend clock alone (§11.4 "在 T(g) < T(lost) + grace 時
// … 那些 prune 的整輪時限在 T(lost) + grace 之前結束"). Clone of scenarios.rs
// `rebuild_with_a_slow_deleter`; the backend clock is 2 h slow (inside the random `BackendSkew`
// range) after the slow deleter's P0. Its run bound is 71 h from P0 on its own clock; the hold
// ends 70 h after P0. The deleter deletes inside that window.

fn hold_setup(cfg: Config, skew: Time) -> (World, ActorId) {
    let mut w = World::new(cfg, 1);
    let s0 = backup(&mut w, 1, 0, 0);
    let _s2 = backup(&mut w, 2, 1, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    forget(&mut w, s0);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let slow = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(slow, "Load")); // P0 with an honest backend clock
    w.core.backend.skew = skew;
    assert!(w.run_until(slow, "Delete"));
    w.core.backend.skew = 0;
    let newest = newest_gen(&w);
    let older = shards_of(&w, newest - 1);
    for sid in shards_of(&w, newest) {
        if !older.contains(&sid) {
            w.core.backend.delete(&Key::Shard(sid)); // same fault as the original scenario
            w.core.shard_fault = true;
        }
    }
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    w.advance(70 * HOUR + 30 * 60); // the slow deleter is still inside its run bound
    assert_eq!(prune(&mut w, 2), StepResult::Done); // base = rebuilt head: hold active
    (w, slow)
}

fn hold_gc_stuck(skew: Time) -> (World, Vec<StepResult>) {
    let (mut w, slow) = hold_setup(quiet(), skew);
    assert_eq!(w.finish(slow), Some(StepResult::Done));
    let rounds = [0, 1, 72, 72]
        .into_iter()
        .map(|h| {
            w.advance(h * HOUR);
            prune(&mut w, 2)
        })
        .collect();
    (w, rounds)
}

#[test]
fn f2_hold_ends_on_backend_clock_alone_gc_stuck_break() {
    let (w, rounds) = hold_gc_stuck(-2 * HOUR);
    assert_ok(&w);
    assert!(rounds.iter().all(|r| *r == StepResult::Done), "{rounds:?}");
}

#[test]
fn f2_hold_ends_on_backend_clock_alone_gc_stuck_control() {
    let (w, rounds) = hold_gc_stuck(0);
    assert_ok(&w);
    assert!(rounds.iter().all(|r| *r == StepResult::Done), "{rounds:?}");
}

/// Data loss: the attacker (existing `Mal::ForgedSnap`) references an item whose only copy is in
/// a pack the slow deleter tombstoned; after the hold, a prune revives that pack as canonical, an
/// honest backup of its owner reuses it, and the slow deleter deletes it.
fn hold_data_loss(skew: Time) -> World {
    hold_data_loss_with(quiet(), skew)
}

fn hold_data_loss_with(cfg: Config, skew: Time) -> World {
    let (mut w, slow) = hold_setup(cfg, skew);
    let held: BTreeSet<PackId> = map_of(&w, newest_gen(&w))
        .unwrap()
        .into_iter()
        .filter(|(_, r)| r.held)
        .map(|(p, _)| p)
        .collect();
    let s0_root = w.core.universe.tree(0, 0);
    let item = w
        .core
        .universe
        .reachable(s0_root)
        .into_iter()
        .find(|i| {
            let h = w.genuine_holders(*i);
            !h.is_empty() && h.iter().all(|(_, p)| held.contains(p))
        })
        .unwrap();
    w.apply(&Action::Mal(Mal::ForgedSnap {
        item: item.0 as u16,
    }));
    assert_eq!(prune(&mut w, 2), StepResult::Done);
    let b = w.start_backup(1, 0, 0, 0).unwrap();
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_eq!(w.finish(slow), Some(StepResult::Done));
    w
}

#[test]
fn f2_hold_ends_on_backend_clock_alone_data_loss_break() {
    assert_eq!(r1(&hold_data_loss(-2 * HOUR)), "");
}

#[test]
fn f2_hold_ends_on_backend_clock_alone_data_loss_control() {
    assert_eq!(r1(&hold_data_loss(0)), "");
}

// ---- F3: the rebuild keep rule "T(g_lost) < T(g_r + 1) + a" assumes T monotone in g, while P1
// tolerates "較舊的 head 之間的時間異常". Tombstoning needs T(c) + age ≤ T(d−1), d ≤ g_lost; a
// 10 min backward backend step (inside the random range) during the deleter's run gives
// T(4) < T(2) + 72 h ≤ T(3). One lost shard shared by heads 2..4 leaves g_r = 1.

fn rebuild_keep(skew: Time) -> World {
    rebuild_keep_with(quiet(), skew)
}

fn rebuild_keep_with(cfg: Config, skew: Time) -> World {
    let mut w = World::new(cfg, 1);
    let _a = backup(&mut w, 1, 0, 0);
    let s1 = backup(&mut w, 1, 1, 0); // new packs X, Y hold job-1-only items
    forget(&mut w, s1); // client 1 idle: allowed
    w.advance(100 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 1: X, Y live
    let _c = backup(&mut w, 2, 0, 31);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 2: X, Y condemned; c2 packs in
    w.advance(72 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 3: unchanged shards
    let del = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(del, "Load"));
    w.core.backend.skew = skew;
    assert!(w.run_until(del, "Delete")); // gen 4: X, Y tombstoned
    w.core.backend.skew = 0;
    lose_shared_shard(&mut w, 2, 4);
    assert!(map_of(&w, 2).is_none() && map_of(&w, 3).is_none() && map_of(&w, 1).is_some());
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    let b = w.start_backup(1, 1, 0, 0).unwrap(); // the retired job again; X, Y are its own
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_eq!(w.finish(del), Some(StepResult::Done));
    w
}

#[test]
fn f3_rebuild_keep_rule_assumes_monotone_t_break() {
    assert_eq!(r1(&rebuild_keep(-600)), "");
}

#[test]
fn f3_rebuild_keep_rule_assumes_monotone_t_control() {
    assert_eq!(r1(&rebuild_keep(0)), "");
}

// ---- F4: one attacker object stops GC with no self-healing (ADR 026 lists only the one-grace
// junk-snapshot stall). P4 "解得開但版本不認得 → 拒絕"; the rebuild reads trailers "照 P4";
// §10: newest head unreadable → "backup 與 prune 停止".

#[test]
fn f4_unknown_version_pack_stalls_gc_forever() {
    let mut w = World::new(quiet(), 1);
    let _s = backup(&mut w, 1, 0, 0);
    w.apply(&Action::Mal(Mal::UnknownVersionPack));
    let rounds: Vec<StepResult> = (0..4)
        .map(|_| {
            w.advance(DEFAULT_GRACE);
            prune(&mut w, 0)
        })
        .collect();
    assert_eq!(rounds.last(), Some(&StepResult::Done), "{rounds:?}");
}

#[test]
fn f4_unknown_version_pack_and_a_shard_loss_stop_all_backups() {
    let mut w = World::new(quiet(), 1);
    let _s = backup(&mut w, 1, 0, 0);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.apply(&Action::Mal(Mal::UnknownVersionPack));
    w.apply(&Action::LoseShard { pick: 0 });
    let rb = w.start_rebuild(1).unwrap();
    let rebuild = w.finish(rb);
    let b = w.start_backup(2, 0, 3, 0).unwrap();
    assert_eq!(w.finish(b), Some(StepResult::Done), "rebuild: {rebuild:?}");
}

/// Readable snapshots under the attacker's own prefix: `Mal::ForgedSnap` on an item with no
/// holder (every aborted backup interns such items), and one with v = 5 (the attacker holds
/// `K_snap`; P5 "解得開但版本不認得 → 拒絕"). Both stop GC until a maintainer forgets them.
fn attacker_snapshot(w: &mut World, unknown_version: bool) {
    if unknown_version {
        let root = w.core.universe.tree(0, 0);
        let key = Key::Snap(SnapKey {
            client: MALICIOUS,
            ts: 999_999,
        });
        let body = SnapBody {
            readable: true,
            version: VERSION + 1,
            job: 1,
            root,
            parent: None,
        };
        w.core.put(key, Content::Snap(body)).unwrap();
    } else {
        let unstored = w.core.universe.intern(ItemDef::Chunk(4242));
        assert!(w.genuine_holders(unstored).is_empty());
        let item = unstored.0 as u16;
        w.apply(&Action::Mal(Mal::ForgedSnap { item }));
    }
}

/// 負責人的決定之外、由設計決定（2026-10-10）：這兩種 snapshot 可能代表真的資料遺失，所以照樣
/// 大聲拒絕，不會自己過期；解法是 `forget <key>`（a4），責任可以追到那個前綴。這個測試釘住「一直
/// 拒絕」，殘餘風險記在 ADR 026。
#[test]
fn f4_attacker_snapshots_stall_gc_until_forgotten() {
    let mut last = Vec::new();
    for unknown_version in [false, true] {
        let mut w = World::new(quiet(), 1);
        let _s = backup(&mut w, 1, 0, 0);
        attacker_snapshot(&mut w, unknown_version);
        w.advance(2 * DEFAULT_GRACE);
        last.push(prune(&mut w, 0));
    }
    assert!(
        last.iter()
            .all(|r| matches!(r, StepResult::Abort(m) if m.starts_with("P5"))),
        "{last:?}"
    );
}

// ---- F5: P3 "資料遺失，請執行 check 與修復" fires on a superseded base. q loads the same head as
// p; p wins g+1 and deletes a pack that is condemned in Map(g); q then lists packs.

#[test]
fn f5_superseded_prune_reports_false_data_loss() {
    let (w, r) = superseded_prune(quiet());
    assert_ok(&w);
    // 修正後（2026-10-10）：P3 拒絕之前重新列出 head，發現已有較新的世代，就以輸掉柵欄結束。
    assert!(
        matches!(&r, StepResult::Abort(m) if m.contains("already exists") || m.starts_with("superseded")),
        "{r:?}"
    );
}

fn superseded_prune(cfg: Config) -> (World, StepResult) {
    let mut w = World::new(cfg, 1);
    let s0 = backup(&mut w, 1, 0, 0);
    let _s1 = backup(&mut w, 1, 0, 31);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    forget(&mut w, s0); // not the newest of (client 1, job 0)
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let p = w.start_prune(0, 0).unwrap();
    let q = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(q, "ListPacks"));
    assert_eq!(w.finish(p), Some(StepResult::Done));
    assert!(w.core.stats.packs_deleted > 0);
    let r = w.finish(q).unwrap();
    (w, r)
}

// ---- 修正的反向對照（2026-10-10）：打開修正前的規則，上面的情境要重新違反。

fn old_rule(m: Mutations) -> Config {
    Config {
        mutations: m,
        ..quiet()
    }
}

fn one_clock() -> Config {
    old_rule(Mutations {
        one_clock_windows: true,
        ..Mutations::default()
    })
}

#[test]
fn mutation_one_clock_drain_end_loses_data() {
    assert_ne!(r1(&drain_with(one_clock(), -4 * HOUR)), "");
}

#[test]
fn mutation_one_clock_hold_end_loses_data() {
    assert_ne!(r1(&hold_data_loss_with(one_clock(), -2 * HOUR)), "");
}

#[test]
fn mutation_one_clock_keep_rule_loses_data() {
    assert_ne!(r1(&rebuild_keep_with(one_clock(), -600)), "");
}

#[test]
fn mutation_no_superseded_check_raises_a_false_alarm() {
    let (w, r) = superseded_prune(old_rule(Mutations {
        no_superseded_check: true,
        ..Mutations::default()
    }));
    assert!(
        matches!(&r, StepResult::Abort(m) if m.starts_with("P3")),
        "{r:?}"
    );
    let v = w.check();
    assert!(
        v.iter().any(|m| m.starts_with("false data-loss alarm")),
        "{v:#?}"
    );
}

// ---- M1: `World::start_forget` checks the procedure rule (§9 "只能在它沒有進行中的 backup 時做")
// only when the forget starts; a same-client, same-roots backup may start between its two steps.

#[test]
fn m1_forget_procedure_rule_checked_only_at_start() {
    let mut w = World::new(quiet(), 1);
    assert!(w.core.cfg.forced_forget_waits_for_idle);
    let s3 = backup(&mut w, 3, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let bp = w.start_backup(1, 0, 2, 0).unwrap();
    assert!(w.run_until(bp, "Gate"));
    forget(&mut w, s3);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert_eq!(w.finish(bp), Some(StepResult::Done));
    let parent = w.newest(1, 0).unwrap();
    w.advance(60 * HOUR);
    let keys: BTreeSet<SnapKey> = w
        .core
        .backend
        .keys()
        .filter_map(|k| match k {
            Key::Snap(s) | Key::SnapR1(s) => Some(*s),
            _ => None,
        })
        .collect();
    let pick = keys.iter().position(|k| *k == parent).unwrap() as u16;
    w.apply(&Action::StartForget { pick, force: true }); // client 1 idle: accepted
    let f = *w.actor_ids().last().unwrap();
    assert_eq!(w.step(f), Some(StepResult::Continue)); // `.r1` deleted
                                                       // 修正後（2026-10-10）：強制 forget 進行中，這組 roots 在退役，不能開始新的 backup。
    assert!(w.start_backup(1, 0, 6, 0).is_none());
    assert_eq!(w.step(f), Some(StepResult::Done)); // body deleted
    let b = w.start_backup(1, 0, 6, 0).unwrap(); // after the forget: a fresh start, no parent
    w.advance(13 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_eq!(r1(&w), "");
}

// ---- M2: `Core::put` sets mtime = now − ε at the step where the object becomes visible, so two
// heads written at the same `now` can get T(g+1) < T(g) with an honest backend clock.

#[test]
fn m2_epsilon_inverts_consecutive_head_times() {
    let cfg = Config {
        repack: RepackPolicy::Never,
        ..Config::default()
    };
    let inverted = (1..200u64).find_map(|seed| {
        let mut w = World::new(cfg.clone(), seed);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        (t_of(&w, 2) < t_of(&w, 1)).then(|| (seed, t_of(&w, 1), t_of(&w, 2)))
    });
    assert_eq!(inverted, None, "(seed, T(1), T(2))");
}

// ---- Attempts that did not break it.

/// The attacker claims a chunk and a metadata item before anyone stores them; its false packs
/// enter the map first and outrank the honest copies written later.
#[test]
fn a1_attacker_ranked_first_as_canonical() {
    let mut w = World::new(quiet(), 1);
    let _s0 = backup(&mut w, 1, 0, 0);
    let roots = w.core.universe.tree(0, 31);
    let chunk = w.core.universe.intern(ItemDef::Chunk(401));
    w.mal_false_pack(&[chunk], Kind::Data, true);
    w.mal_false_pack(&[roots], Kind::Meta, true);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let s1 = backup(&mut w, 2, 0, 31);
    for _ in 0..6 {
        w.advance(40 * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        assert_ok(&w);
    }
    let r = w.start_reader(s1);
    assert_eq!(w.finish(r), Some(StepResult::Done));
    assert_ok(&w);
    assert!(w.core.stats.false_claims_removed > 0);
}

/// A backup that passed B4 before a condemn-all rebuild (honest clocks) commits against the
/// rebuilt map (everything condemned + held) while the slow deleter still deletes.
#[test]
fn a2_backup_in_flight_across_a_condemn_all_rebuild() {
    let mut w = World::new(quiet(), 1);
    let _a = backup(&mut w, 1, 0, 0);
    let s1 = backup(&mut w, 1, 1, 0);
    forget(&mut w, s1);
    w.advance(100 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let _c = backup(&mut w, 2, 0, 31);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(72 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(3, 1, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1"));
    let del = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(del, "Delete"));
    lose_shared_shard(&mut w, 2, 4);
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    assert!(w.core.log.join("\n").contains("keep false"));
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_eq!(w.finish(del), Some(StepResult::Done));
    for h in [1, 30, 30, 30, 30, 30] {
        w.advance(h * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        assert_ok(&w);
    }
}

/// Backend clock 80 h slow while a backup writes its packs: a prune condemns them before the
/// commit (P7b's mtime test fails open; spec "安全性不靠它").
#[test]
fn a3_fresh_packs_condemned_early_by_a_slow_backend() {
    let mut w = World::new(quiet(), 1);
    let _s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 1, 9, 0).unwrap();
    assert!(w.run_until(b, "PutPack"));
    w.core.backend.skew = -80 * HOUR;
    assert!(w.run_until(b, "Gate"));
    w.core.backend.skew = 0;
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert!(w.core.stats.condemned > 0);
    w.advance(30 * HOUR);
    assert_eq!(w.finish(b), Some(StepResult::Done));
    for _ in 0..4 {
        w.advance(40 * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        assert_ok(&w);
    }
}

/// F4's attacker snapshots cause no deletion and `forget <key>` clears them.
#[test]
fn a4_forget_clears_attacker_snapshots() {
    let mut w = World::new(quiet(), 1);
    let _s = backup(&mut w, 1, 0, 0);
    attacker_snapshot(&mut w, false);
    attacker_snapshot(&mut w, true);
    assert!(matches!(prune(&mut w, 0), StepResult::Abort(_)));
    let forged: Vec<SnapKey> = w
        .core
        .backend
        .keys()
        .filter_map(|k| match k {
            Key::Snap(s) if s.client == MALICIOUS => Some(*s),
            _ => None,
        })
        .collect();
    assert_eq!(forged.len(), 2);
    for k in forged {
        forget(&mut w, k);
    }
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert_ok(&w);
}

// ---- Census of `common::action()` (ignored; run with --run-ignored only).

fn sample(clock: Clock, seed: u8) -> Vec<(u64, Vec<Action>)> {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::{Config as PtConfig, RngAlgorithm, TestRng, TestRunner};
    let mut runner = TestRunner::new_with_rng(
        PtConfig::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]),
    );
    (0..512)
        .map(|_| input(clock).new_tree(&mut runner).unwrap().current())
        .collect()
}

#[test]
#[ignore]
fn census_random_strategy() {
    use std::collections::BTreeMap;
    for (clock, seed) in [(Clock::Hosts, 1u8), (Clock::Backend, 2u8)] {
        let mut c: BTreeMap<String, u64> = BTreeMap::new();
        let mut bump = |k: &str| *c.entry(k.to_string()).or_default() += 1;
        let mut max_skew = 0i64;
        for (s, actions) in sample(clock, seed) {
            for a in &actions {
                match a {
                    Action::BackendSkew(k) => max_skew = max_skew.max(i64::from(*k).abs()),
                    Action::Mal(Mal::UnknownVersionPack) => bump("action UnknownVersionPack"),
                    _ => {}
                }
            }
            let out = World::run(Config::default(), s, &actions);
            let mut tail = 0;
            for l in &out.log {
                if l.contains("wrote head") || l.contains("settings: grace") {
                    tail = 0;
                }
                if l.contains("fault: lost shard") {
                    bump("shard losses");
                }
                // Completed rebuilds only ("rebuild: head g (g_lost …, g_r …, keep …, …)").
                if let Some(i) = l.find("rebuild: head ").filter(|_| l.contains("g_lost ")) {
                    bump("rebuilds");
                    let n = |key: &str| -> u64 {
                        let r = &l[i..];
                        let j = r.find(key).unwrap() + key.len();
                        r[j..]
                            .split(|ch: char| !ch.is_ascii_digit())
                            .next()
                            .unwrap()
                            .parse()
                            .unwrap()
                    };
                    if n("g_r ") + 1 < n("g_lost ") {
                        bump("rebuilds with g_r < g_lost - 1");
                    }
                    if l.contains("keep false") {
                        bump("rebuilds condemn-all");
                    }
                }
                if let Some(i) = l.find("stopped: P") {
                    tail += 1;
                    let key: String = l[i + 9..].chars().take(14).collect();
                    bump(&format!("stop {key}"));
                }
            }
            if tail >= 3 {
                bump("cases ending with >= 3 prune refusals in a row");
            }
        }
        println!("== {clock:?} seed {seed}: 512 cases, max |BackendSkew| {max_skew} s");
        let mut v: Vec<_> = c.into_iter().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        for (k, n) in v.iter().take(14) {
            println!("{n:7}  {k}");
        }
    }
}

/// F5 occurs in the existing `random_interleavings_with_a_bad_backend_clock` configuration
/// (Config::default(), Clock::Backend, seed 2, 512 cases) and check() never reports it: every P3
/// refusal, replayed, happened on a superseded base.
#[test]
#[ignore]
fn census_p3_refusals_are_on_superseded_bases() {
    let mut n = 0;
    for (case, (s, actions)) in sample(Clock::Backend, 2).into_iter().enumerate() {
        let out = World::run(Config::default(), s, &actions);
        if !out.log.iter().any(|l| l.contains("stopped: P3")) {
            continue;
        }
        assert!(out.violations.is_empty());
        let mut w = World::new(Config::default(), s);
        let mut seen = 0;
        for a in &actions {
            w.apply(a);
            let p3: Vec<String> = w
                .core
                .log
                .iter()
                .filter(|l| l.contains("stopped: P3"))
                .cloned()
                .collect();
            for l in p3.iter().skip(seen) {
                let pid = l
                    .split("PackId(")
                    .nth(1)
                    .unwrap()
                    .split(')')
                    .next()
                    .unwrap();
                let pid = PackId(pid.parse().unwrap());
                let g = newest_gen(&w);
                let rec = map_of(&w, g).map(|m| m.get(&pid).map(|r| r.status));
                println!("case {case}: {l}\n    newest head {g}: {pid:?} = {rec:?}");
                assert!(matches!(
                    rec,
                    Some(Some(Status::Tombstone { .. })) | Some(None)
                ));
                n += 1;
            }
            seen = p3.len();
        }
    }
    println!("{n} P3 refusals, all on superseded bases, none reported by check()");
    assert!(n > 0);
}
