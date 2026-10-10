//! Second adversarial review of the v4 GC protocol (commit 0529f65), scope: everything changed since
//! the first review (8dc76bd) plus the clock-window class.
//!
//! 第二輪外部審查（2026-10-10，獨立的 agent，在 0529f65 上寫的）搬進來當回歸測試。改過的地方：G1 的
//! 兩個情境原本以「PUT 成功之後刪掉 `.r1`」模擬 PUT 失敗（之後主體再遺失，就是假設排除的兩份都遺失），
//! 現在改用模型的 `Action::FailHeadR1Put`，也不再斷言寫入者照樣刪除（那是修正之前的行為）；檔尾加上
//! 修正之後的反向對照（`mutation_*`）。修正之後所有 `*_break` 都通過，名稱照舊。
//!
//! Naming, same as review.rs: `<id>_<what>_break` asserts what the spec promises and FAILS on the
//! unmodified protocol (`Mutations::default()`); `<id>_<what>_control` is the same interleaving
//! without the fault and PASSES; `a<n>_*` are attacks that did not break it (PASS); `m<n>_*` show
//! the model differing from the spec. All scenarios use `quiet()` (eps 0, under 0) unless noted.
//! Ids: `d1` = doubt A (T(g) moves when a head body is lost), `c1`–`c3` = the clock-window class
//! (one fault on each clock at different times), `g1` = spec gap (P9 without the head's `.r1`),
//! `s*` = a skew-injection strategy for the random search (reaches the class; `c1_block_search_*`
//! is the class found by that search on the unmodified protocol).
//!
//! Candidate fixes were tried in a scratch copy of the crate (not in this repo): with them all
//! `*_break` tests here pass and the existing suite stays green except
//! `scenarios::mutation_only_writer_clock_deletes_early`, whose interleaving the C fix neutralises
//! (a start-side fault still shows the w clause carries safety). See the per-section notes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use kist_gcmodel::*;

// ---- helpers (copied from review.rs, which keeps them private) ----

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

fn shards_of(w: &World, g: Gen) -> Vec<ShardId> {
    let key = if w.core.backend.exists(&Key::Head(g)) {
        Key::Head(g)
    } else {
        Key::HeadR1(g)
    };
    match w.core.backend.get(&key).map(|o| &o.content) {
        Some(Content::Head(h)) => h.shards.iter().flatten().copied().collect(),
        _ => Vec::new(),
    }
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

/// `Action::LoseHeadBody` picks among the heads that have both copies, in key order.
fn lose_head_body(w: &mut World, g: Gen) {
    let both: Vec<Gen> = w
        .core
        .backend
        .keys()
        .filter_map(|k| match k {
            Key::HeadR1(h) if w.core.backend.exists(&Key::Head(*h)) => Some(*h),
            _ => None,
        })
        .collect();
    let pick = both
        .iter()
        .position(|h| *h == g)
        .expect("head with both copies") as u16;
    w.apply(&Action::LoseHeadBody { pick });
    assert!(!w.core.backend.exists(&Key::Head(g)));
    assert!(w.core.backend.exists(&Key::HeadR1(g)));
}

fn mtime(w: &World, key: Key) -> Time {
    w.core.backend.get(&key).unwrap().mtime
}

fn log_has(w: &World, needle: &str) -> bool {
    w.core.log.iter().any(|l| l.contains(needle))
}

/// The log lines that contain any of `needles`, for assertion messages.
fn trace(w: &World, needles: &[&str]) -> String {
    w.core
        .log
        .iter()
        .filter(|l| needles.iter().any(|n| l.contains(n)))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// (T, w) of head g as a reader sees it now: body, else `.r1` (core::read_head).
fn head_times(w: &World, g: Gen) -> (Time, Time) {
    for key in [Key::Head(g), Key::HeadR1(g)] {
        if let Some(o) = w.core.backend.get(&key) {
            if let Content::Head(h) = &o.content {
                return (o.mtime, h.w);
            }
        }
    }
    panic!("head {g} is gone")
}

/// Spread of T and of w over heads `from..=to` as the rebuild computes it.
fn spreads(w: &World, from: Gen, to: Gen) -> (Time, Time) {
    let v: Vec<(Time, Time)> = (from..=to).map(|g| head_times(w, g)).collect();
    let t = v.iter().map(|x| x.0).max().unwrap() - v.iter().map(|x| x.0).min().unwrap();
    let wv = v.iter().map(|x| x.1).max().unwrap() - v.iter().map(|x| x.1).min().unwrap();
    (t, wv)
}

// ==== D1 (doubt A): T(g) moves later when a head body is lost after a late `.r1` ====
//
// §10 defines T(g) as "head g 的後端修改時間" and claims it is always available; the model reads it
// from the body, or from `.r1` when the body is gone. P1's repair (new in 0529f65) writes a missing
// `.r1` when a maintenance loader reads the head, which can be a whole grace after the body. If the
// body is lost afterwards, T(g) jumps to the repair time. §11.4's keep rule takes max − min of T over
// the lost generations, so a later T for the oldest lost head shrinks the spread and the rule keeps
// Map(g_r) although a pack that is live there was tombstoned (and is being deleted) in the lost
// generations — the exact data loss F3 had, with honest clocks.
//
// Skeleton = review.rs `rebuild_keep_with` (F3) with the backend skew replaced by: the gen-2 writer
// crashes between body and `.r1`; the gen-3 prune repairs `.r1` 72 h later; the body of head 2 is
// lost after the gen-4 deleter has planned (its plan read T(2) from the body).
//
// Candidate fix (scratch-checked): the keep rule's T clause counts only if every head in
// g_r+1..=g_lost still has its body (a `.r1` mtime is not that generation's T); otherwise only the w
// clause can keep Map(g_r).

fn late_r1(lose_body: bool) -> World {
    late_r1_with(Mutations::default(), lose_body)
}

fn late_r1_with(muts: Mutations, lose_body: bool) -> World {
    let mut w = World::new(with_mutations(muts), 1);
    let _a = backup(&mut w, 1, 0, 0);
    let s1 = backup(&mut w, 1, 1, 0); // packs X, Y hold job-1-only items
    forget(&mut w, s1); // client 1 idle: the (1, job 1) roots retire
    w.advance(100 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 1: X, Y live
    let _c = backup(&mut w, 2, 0, 31);
    let p2 = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(p2, "PutHeadR1")); // gen 2 body: X, Y condemned
    w.apply(&Action::CrashHeadWriter { pick: 0 }); // ... and no `.r1`
    assert!(!w.core.backend.exists(&Key::HeadR1(2)));
    let t2_body = mtime(&w, Key::Head(2));
    w.advance(72 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 3; its P1 repaired head 2's `.r1`
    assert!(log_has(&w, "repaired the .r1 of head 2"));
    assert_eq!(mtime(&w, Key::HeadR1(2)), t2_body + 72 * HOUR);
    let del = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(del, "Delete")); // gen 4: X, Y tombstoned (read T(2) from the body)
    assert!(w.core.stats.tombstoned > 0, "{:?}", w.core.stats);
    if lose_body {
        lose_head_body(&mut w, 2); // a single-copy loss on a head that has both copies
    }
    lose_shared_shard(&mut w, 2, 4);
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    let b = w.start_backup(1, 1, 0, 0).unwrap(); // the retired job again; X, Y are its own
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_eq!(w.finish(del), Some(StepResult::Done));
    w
}

#[test]
fn d1_late_r1_then_body_loss_fools_the_keep_rule_break() {
    let w = late_r1(true);
    let (t_spread, w_spread) = spreads(&w, 2, 4);
    assert_eq!(
        r1(&w),
        "",
        "lost gens 2..=4 as the rebuild read them: T spread {t_spread} s, w spread {w_spread} s \
         (a = {} s)\n{}",
        DEFAULT_GRACE,
        trace(
            &w,
            &[
                "repaired",
                "fault:",
                "rebuild: head",
                "commit",
                "tombstone",
                "wrote head"
            ]
        )
    );
}

#[test]
fn d1_late_r1_then_body_loss_fools_the_keep_rule_control() {
    let w = late_r1(false);
    assert!(log_has(&w, "keep false"));
    assert_eq!(r1(&w), "");
}

// ==== C1: the two-clock rule does not survive one fault on EACH clock, even if never simultaneous ====
//
// §11.1 "兩種寫法都讓單一時鐘出錯只能延後刪除"; §11.5 "後端時鐘偏慢或寫入者時鐘出錯都只會延後刪除";
// §11.6 assumes only "後端時鐘與寫入者的牆鐘不會同時往同一個方向錯". P7d compares T(c) with T(g) and
// w(c) with w(g): a backend fault at head c's write corrupts only the T clause and a writer-clock
// fault at head g's write corrupts only the w clause, so one fault of each kind, at two different
// times, satisfies both clauses early. Here the backend clock is 80 h slow only during prune c (after
// its P0, restored when it ends) and host 1 is 200 h ahead only during prune e (after its P0,
// restored when it ends): at no instant are two clocks wrong. Each fault alone is the existing
// scenarios.rs `clock_fault` (both pass, see `c1_*_single_fault_*`); together the parked backup's
// packs are tombstoned 2 h after their condemnation and deleted under it. The random search cannot
// produce this: `common::action(clock)` injects either host skew or backend skew in a case, never both.
//
// Candidate fix (scratch-checked for P7d, drain end, hold end, the settings drain check and rebuild
// case B's age): measure the "now" of each window per clock as min(base head's value, P0's value),
// i.e. (min(T(g), N_P), min(w(g), local wall at P0)), as P5 already does with the P0 pair. P0
// cross-checks the two P0 values, so a clock that is wrong at only one instant can corrupt at most
// one clause of the start (head c) and none of the "now"; §11.6's "不會同時" then suffices.

fn seq_clock_faults(backend_slow_at_c: bool, host_ahead_at_e: bool) -> (World, Option<StepResult>) {
    seq_clock_faults_with(Mutations::default(), backend_slow_at_c, host_ahead_at_e)
}

fn seq_clock_faults_with(
    muts: Mutations,
    backend_slow_at_c: bool,
    host_ahead_at_e: bool,
) -> (World, Option<StepResult>) {
    let mut w = World::new(with_mutations(muts), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 1
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1")); // reuses s0's packs, B4 passed
    forget(&mut w, s0);
    w.advance(HOUR);
    let c = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(c, "Load")); // P0 passed with honest clocks
    if backend_slow_at_c {
        w.apply(&Action::BackendSkew(-80 * 3600));
    }
    assert_eq!(w.finish(c), Some(StepResult::Done)); // gen 2: s0's packs condemned
    w.apply(&Action::BackendSkew(0));
    assert!(w.core.stats.condemned > 0);
    w.advance(HOUR);
    let e = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(e, "Load")); // P0 passed with honest clocks
    if host_ahead_at_e {
        w.apply(&Action::SetSkew {
            host: 1,
            skew: 200 * 3600,
        });
    }
    assert_eq!(w.finish(e), Some(StepResult::Done)); // gen 3
    w.apply(&Action::SetSkew { host: 1, skew: 0 });
    w.advance(HOUR);
    let _ = prune(&mut w, 0); // gen 4, honest clocks: P7d compares (T, w) of heads 2 and 3
    let r = w.finish(b); // E = 3 h
    (w, r)
}

#[test]
fn c1_p7d_backend_slow_at_c_then_writer_ahead_at_base_break() {
    let (w, r) = seq_clock_faults(true, true);
    assert_eq!(r, Some(StepResult::Done));
    let (t2, w2) = head_times(&w, 2);
    let (t3, w3) = head_times(&w, 3);
    assert_eq!(
        r1(&w),
        "",
        "T(2) + 72 h = {} <= T(3) = {t3}; w(2) + 72 h = {} <= w(3) = {w3}; deleted {}\n{}",
        t2 + DEFAULT_GRACE,
        w2 + DEFAULT_GRACE,
        w.core.stats.packs_deleted,
        trace(&w, &["wrote head", "commit", "forget"])
    );
}

#[test]
fn c1_p7d_backend_slow_at_c_then_writer_ahead_at_base_control() {
    let (w, r) = seq_clock_faults(false, false);
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

#[test]
fn c1_p7d_single_fault_backend_slow_only() {
    let (w, r) = seq_clock_faults(true, false);
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

#[test]
fn c1_p7d_single_fault_writer_ahead_only() {
    let (w, r) = seq_clock_faults(false, true);
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

// ==== C2: the same class ends a grace drain early (F1's timeline) ====
//
// review.rs F1: the lowering head (72 h → 2 h) is written with the backend clock 4 h slow, so the T
// clause of the drain end passes 4 h early; the fix made the w clause hold it. Add one more fault at a
// different time: host 0 is 5 h ahead only during the next prune (after its P0), so w of the base
// head the following prune compares against is 5 h late. The drain ends at 148 h instead of 152 h,
// s0's packs get the 2 h age, and the backup that passed B4 under 72 h loses them.

fn drain_seq(backend_slow_at_lowering: bool, host_ahead_at_next: bool) -> World {
    drain_seq_with(
        Mutations::default(),
        backend_slow_at_lowering,
        host_ahead_at_next,
    )
}

fn drain_seq_with(
    muts: Mutations,
    backend_slow_at_lowering: bool,
    host_ahead_at_next: bool,
) -> World {
    let mut w = World::new(with_mutations(muts), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1")); // B4 passed under grace 72 h
    let s = w.start_settings(1, 2 * HOUR).unwrap();
    assert!(w.run_until(s, "settings h1 PutHead")); // P0 and the plan, honest clocks
    if backend_slow_at_lowering {
        w.apply(&Action::BackendSkew(-4 * 3600));
    }
    assert_eq!(w.step(s), Some(StepResult::Continue)); // head 2, drain { old_grace: 72 h }
    assert_eq!(w.step(s), Some(StepResult::Done)); // its .r1
    w.apply(&Action::BackendSkew(0));
    w.advance(68 * HOUR); // 148 h; an honest drain lasts until 152 h
    let p = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(p, "Load")); // P0 with honest clocks
    if host_ahead_at_next {
        w.apply(&Action::SetSkew {
            host: 0,
            skew: 5 * 3600,
        });
    }
    assert_eq!(w.finish(p), Some(StepResult::Done)); // head 3, w(3) = 153 h
    w.apply(&Action::SetSkew { host: 0, skew: 0 });
    forget(&mut w, s0);
    for h in [0, 2, 0] {
        w.advance(h * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
    }
    assert_eq!(w.finish(b), Some(StepResult::Done)); // E = 70 h < 72 h − M
    w
}

#[test]
fn c2_drain_end_backend_slow_at_lowering_then_writer_ahead_break() {
    let w = drain_seq(true, true);
    assert_eq!(
        r1(&w),
        "",
        "deleted {}\n{}",
        w.core.stats.packs_deleted,
        trace(&w, &["wrote head", "settings:", "commit", "forget"])
    );
}

#[test]
fn c2_drain_end_backend_slow_at_lowering_then_writer_ahead_control() {
    assert_ok(&drain_seq(false, false));
}

#[test]
fn c2_drain_end_single_faults_only_delay() {
    assert_ok(&drain_seq(true, false));
    assert_ok(&drain_seq(false, true));
}

// ==== C3: the same class ends a rebuild hold early (F2's timeline) ====
//
// review.rs F2 (data-loss variant): the slow deleter writes head 4 with the backend clock 2 h slow, so
// the T clause of the hold end passes 2 h early; the fix made the w clause hold it. Add host 2 being
// 2 h ahead only during the first prune after the rebuild (after its P0): the next prune compares
// max w of the lost heads + grace against that late w and ends the hold while the slow deleter is
// still inside its run bound. The forged snapshot revives a held pack, its owner reuses it, and the
// slow deleter deletes it.

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

fn newest_gen(w: &World) -> Gen {
    w.core
        .backend
        .keys()
        .filter_map(|k| match k {
            Key::Head(g) | Key::HeadR1(g) => Some(*g),
            _ => None,
        })
        .max()
        .unwrap()
}

fn hold_seq(backend_slow_at_deleter: bool, host_ahead_after_rebuild: bool) -> World {
    hold_seq_with(
        Mutations::default(),
        backend_slow_at_deleter,
        host_ahead_after_rebuild,
    )
}

fn hold_seq_with(
    muts: Mutations,
    backend_slow_at_deleter: bool,
    host_ahead_after_rebuild: bool,
) -> World {
    let mut w = World::new(with_mutations(muts), 1);
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
    assert!(w.run_until(slow, "Load")); // P0 with honest clocks
    if backend_slow_at_deleter {
        w.apply(&Action::BackendSkew(-2 * 3600));
    }
    assert!(w.run_until(slow, "Delete")); // head 4 tombstones s0's packs
    w.apply(&Action::BackendSkew(0));
    let newest = newest_gen(&w);
    let older = shards_of(&w, newest - 1);
    for sid in shards_of(&w, newest) {
        if !older.contains(&sid) {
            w.core.backend.delete(&Key::Shard(sid));
            w.core.lost_shards.insert(sid);
            w.core.shard_fault = true;
        }
    }
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    w.advance(70 * HOUR + 30 * 60); // the slow deleter is still inside its run bound
    let p = w.start_prune(2, 0).unwrap();
    assert!(w.run_until(p, "Load")); // P0 with honest clocks
    if host_ahead_after_rebuild {
        w.apply(&Action::SetSkew {
            host: 2,
            skew: 2 * 3600,
        });
    }
    assert_eq!(w.finish(p), Some(StepResult::Done)); // base = rebuilt head: hold active
    w.apply(&Action::SetSkew { host: 2, skew: 0 });
    let held: Vec<PackId> = map_of(&w, newest_gen(&w))
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
    assert_eq!(prune(&mut w, 2), StepResult::Done); // honest clocks
    let b = w.start_backup(1, 0, 0, 0).unwrap();
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_eq!(w.finish(slow), Some(StepResult::Done));
    w
}

#[test]
fn c3_hold_end_backend_slow_at_deleter_then_writer_ahead_break() {
    let w = hold_seq(true, true);
    assert_eq!(
        r1(&w),
        "",
        "{}",
        trace(&w, &["wrote head", "rebuild: head", "commit", "mal:"])
    );
}

#[test]
fn c3_hold_end_backend_slow_at_deleter_then_writer_ahead_control() {
    assert_eq!(r1(&hold_seq(false, false)), "");
}

#[test]
fn c3_hold_end_single_faults_only_delay() {
    assert_eq!(r1(&hold_seq(true, false)), "");
    assert_eq!(r1(&hold_seq(false, true)), "");
}

// ==== D1, second mechanism: no repair needed, the writer itself writes `.r1` late ====
//
// The gen-2 writer stalls 72 h between its two PUTs (alive the whole time, so LoseLoneHeadBody never
// applies to it); its own `.r1` lands 72 h after the body, then it runs out of its run bound before
// P9. Same keep-rule error after the body loss. Heads "always" writing `.r1` (§13) is what makes this
// reachable for every head, whatever `replicas` is.

fn stalled_r1_writer(lose_body: bool) -> World {
    stalled_r1_writer_with(Mutations::default(), lose_body)
}

fn stalled_r1_writer_with(muts: Mutations, lose_body: bool) -> World {
    let mut w = World::new(with_mutations(muts), 1);
    let _a = backup(&mut w, 1, 0, 0);
    let s1 = backup(&mut w, 1, 1, 0);
    forget(&mut w, s1);
    w.advance(100 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 1
    let _c = backup(&mut w, 2, 0, 31);
    let p2 = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(p2, "PutHeadR1")); // gen 2 body
    let t2_body = mtime(&w, Key::Head(2));
    w.advance(72 * HOUR);
    let r = w.finish(p2); // `.r1` now; the deletions are out of time
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("out of time")),
        "{r:?}"
    );
    assert_eq!(mtime(&w, Key::HeadR1(2)), t2_body + 72 * HOUR);
    assert!(!log_has(&w, "repaired the .r1 of head 2"));
    assert_eq!(prune(&mut w, 1), StepResult::Done); // gen 3
    let del = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(del, "Delete")); // gen 4 tombstones X, Y
    if lose_body {
        lose_head_body(&mut w, 2);
    }
    lose_shared_shard(&mut w, 2, 4);
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    let b = w.start_backup(1, 1, 0, 0).unwrap();
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_eq!(w.finish(del), Some(StepResult::Done));
    w
}

#[test]
fn d1_stalled_writer_late_r1_then_body_loss_break() {
    let w = stalled_r1_writer(true);
    assert_eq!(
        r1(&w),
        "",
        "{}",
        trace(&w, &["fault:", "rebuild: head", "commit"])
    );
}

#[test]
fn d1_stalled_writer_late_r1_then_body_loss_control() {
    let w = stalled_r1_writer(false);
    assert!(log_has(&w, "keep false"));
    assert_eq!(r1(&w), "");
}

// ==== S: a skew-injection strategy that reaches the clock-window class ====
//
// Why `common::action` cannot reach it: (1) a skew stays until the next skew action, so it is mostly
// in force at some prune's P0, which refuses ("P0: backend time … vs local clock …"); the dangerous
// window is between a prune's P0 and its head write, a few steps that a randomly placed skew rarely
// hits and is rarely reset after; (2) each case injects host skew OR backend skew, never both;
// (3) `Step`/`Finish` pick a random actor, so a backup is rarely parked past B4 while several prunes
// run around it. The blocks below fix exactly those three: a prune whose clock goes wrong right
// after its P0 and is restored when it ends; a backup parked after B4; everything else plain.
// `Mode::Both` mixes the two clock kinds in one case.

#[derive(Clone, Debug)]
enum Block {
    Backup {
        client: u8,
        variant: u8,
    },
    /// Start a backup and run it until B4 has passed (phase PutSnapR1); leave it parked.
    Park {
        client: u8,
        variant: u8,
    },
    /// `Action::Finish` on some actor.
    Resume {
        pick: u16,
    },
    /// `Action::StartForget { force: true }` (obeys the §9 procedure rule), run to completion.
    Forget {
        pick: u16,
    },
    Advance {
        hours: u8,
    },
    Prune {
        host: u8,
    },
    /// A prune whose backend clock (or host clock) is off by `skew_h` hours from right after its P0
    /// until it ends.
    SkewedPrune {
        host: u8,
        backend: bool,
        skew_h: i16,
    },
    /// A settings change to `grace_h` hours whose clock is off by `skew_h` hours while it writes the
    /// head (after its P0 and plan).
    SkewedSettings {
        host: u8,
        backend: bool,
        skew_h: i16,
        grace_h: u8,
    },
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Hosts,
    Backend,
    Both,
}

fn block(mode: Mode) -> proptest::strategy::BoxedStrategy<Block> {
    use proptest::prelude::*;
    let kind: BoxedStrategy<bool> = match mode {
        Mode::Hosts => Just(false).boxed(),
        Mode::Backend => Just(true).boxed(),
        Mode::Both => any::<bool>().boxed(),
    };
    let skewed = (0u8..3, kind.clone(), -200i16..200).prop_map(|(host, backend, skew_h)| {
        Block::SkewedPrune {
            host,
            backend,
            skew_h,
        }
    });
    let settings =
        (0u8..3, kind, -200i16..200, 2u8..100).prop_map(|(host, backend, skew_h, grace_h)| {
            Block::SkewedSettings {
                host,
                backend,
                skew_h,
                grace_h,
            }
        });
    prop_oneof![
        1 => settings,
        3 => (1u8..3, 0u8..2).prop_map(|(client, variant)| Block::Backup { client, variant }),
        2 => (1u8..3, 0u8..2).prop_map(|(client, variant)| Block::Park { client, variant }),
        2 => any::<u16>().prop_map(|pick| Block::Resume { pick }),
        2 => any::<u16>().prop_map(|pick| Block::Forget { pick }),
        3 => (1u8..100).prop_map(|hours| Block::Advance { hours }),
        3 => (0u8..3).prop_map(|host| Block::Prune { host }),
        3 => skewed,
    ]
    .boxed()
}

fn apply_block(w: &mut World, b: &Block) {
    match *b {
        Block::Backup { client, variant } => {
            if let Some(id) = w.start_backup(client, 0, variant, 0) {
                w.finish(id);
            }
        }
        Block::Park { client, variant } => {
            if let Some(id) = w.start_backup(client, 0, variant, 0) {
                w.run_until(id, "PutSnapR1");
            }
        }
        Block::Resume { pick } => w.apply(&Action::Finish { pick }),
        Block::Forget { pick } => {
            let before = w.actor_ids();
            w.apply(&Action::StartForget { pick, force: true });
            if let Some(id) = w.actor_ids().into_iter().find(|a| !before.contains(a)) {
                w.finish(id);
            }
        }
        Block::Advance { hours } => w.advance(Time::from(hours) * HOUR),
        Block::Prune { host } => {
            if let Some(id) = w.start_prune(host, 0) {
                w.finish(id);
            }
        }
        Block::SkewedPrune {
            host,
            backend,
            skew_h,
        } => {
            if let Some(id) = w.start_prune(host, 0) {
                if w.run_until(id, "Load") {
                    let s = i32::from(skew_h) * 3600;
                    if backend {
                        w.apply(&Action::BackendSkew(s));
                    } else {
                        w.apply(&Action::SetSkew { host, skew: s });
                    }
                    w.finish(id);
                }
                w.apply(&Action::BackendSkew(0));
                w.apply(&Action::SetSkew { host, skew: 0 });
            }
        }
        Block::SkewedSettings {
            host,
            backend,
            skew_h,
            grace_h,
        } => {
            if let Some(id) = w.start_settings(host, Time::from(grace_h) * HOUR) {
                if w.run_until(id, &format!("settings h{host} PutHead")) {
                    let s = i32::from(skew_h) * 3600;
                    if backend {
                        w.apply(&Action::BackendSkew(s));
                    } else {
                        w.apply(&Action::SetSkew { host, skew: s });
                    }
                    w.finish(id);
                }
                w.apply(&Action::BackendSkew(0));
                w.apply(&Action::SetSkew { host, skew: 0 });
            }
        }
    }
}

/// Runs the blocks, checking the invariants after each one. `Err` carries the violations and log.
fn run_blocks(cfg: &Config, blocks: &[Block]) -> Result<(), String> {
    let mut w = World::new(cfg.clone(), 1);
    for (i, b) in blocks.iter().enumerate() {
        apply_block(&mut w, b);
        let v = w.check();
        if !v.is_empty() {
            return Err(format!(
                "after block {i} ({b:?}):\n  {}\nlog:\n{}",
                v.join("\n  "),
                trace(&w, &["wrote head", "commit", "forget", "stopped"])
            ));
        }
    }
    Ok(())
}

/// Fixed-seed search (same runner settings as random.rs); `Err` is the shrunk input.
fn search(cfg: &Config, mode: Mode, cases: u32, seed: u8) -> Result<(), Vec<Block>> {
    use proptest::collection::vec;
    use proptest::test_runner::{
        Config as PtConfig, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner,
    };
    let mut runner = TestRunner::new_with_rng(
        PtConfig {
            cases,
            max_shrink_iters: 5000,
            failure_persistence: None,
            ..PtConfig::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]),
    );
    let result = runner.run(&vec(block(mode), 1..60), |blocks| {
        run_blocks(cfg, &blocks).map_err(TestCaseError::fail)
    });
    match result {
        Ok(()) => Ok(()),
        Err(TestError::Fail(_, blocks)) => Err(blocks),
        Err(e) => panic!("search aborted: {e}"),
    }
}

/// Replays the shrunk input (the world is deterministic), so the violations and log printed belong
/// to exactly that input.
fn replay(cfg: &Config, blocks: &[Block]) -> String {
    let e = run_blocks(cfg, blocks).expect_err("the shrunk input no longer fails");
    format!("{e}\nminimal failing input: {blocks:#?}")
}

fn with_mutations(m: Mutations) -> Config {
    Config {
        mutations: m,
        ..quiet()
    }
}

fn named(name: &str) -> Mutations {
    let mut m = Mutations::default();
    match name {
        "only_backend_clock" => m.only_backend_clock = true,
        "only_writer_clock" => m.only_writer_clock = true,
        "one_clock_windows" => m.one_clock_windows = true,
        "no_elapsed_check" => m.no_elapsed_check = true,
        "default" => {}
        other => panic!("unknown mutation {other}"),
    }
    m
}

/// The four mutations the existing search never finds (ADR 026: "四組都沒有反例") are each found by
/// the block strategy, in the clock mode that matches the mutation, with random.rs
/// `mutation_search`'s budget (2000 cases, seed 7). With `Mutations::default()` those modes find
/// nothing (`s2_*`), so each counterexample is the mutation's.
#[test]
fn s1_block_search_reaches_the_four_clock_window_mutations() {
    let mut missed = Vec::new();
    for (name, mode) in [
        ("only_backend_clock", Mode::Backend),
        ("only_writer_clock", Mode::Hosts),
        ("one_clock_windows", Mode::Backend),
        ("no_elapsed_check", Mode::Backend),
    ] {
        let cfg = with_mutations(named(name));
        match search(&cfg, mode, 2000, 7) {
            Ok(()) => missed.push(name),
            Err(blocks) => {
                let e = replay(&cfg, &blocks);
                assert!(e.contains("R1:"), "{name}: {e}");
            }
        }
    }
    assert!(missed.is_empty(), "no counterexample for {missed:?}");
}

/// Control: the unmodified protocol under the same strategy, one clock kind per case.
#[test]
fn s2_block_search_one_clock_kind_on_head_control() {
    let cfg = quiet();
    for mode in [Mode::Hosts, Mode::Backend] {
        for seed in [7u8, 8] {
            if let Err(blocks) = search(&cfg, mode, 2000, seed) {
                panic!("{mode:?} seed {seed}: {}", replay(&cfg, &blocks));
            }
        }
    }
}

/// C1 found by random search: the same strategy with both clock kinds allowed in a case finds the
/// class on the unmodified protocol (seed 7, 2000 cases); the shrunk input is a C1 interleaving.
#[test]
fn c1_block_search_with_both_clock_kinds_break() {
    let cfg = quiet();
    if let Err(blocks) = search(&cfg, Mode::Both, 2000, 7) {
        panic!("{}", replay(&cfg, &blocks));
    }
}

// ==== G1 (spec gap): P9 does not wait for the head's `.r1` ====
//
// P8 says only "成功之後才寫 head 的 .r1"; nothing makes P9 wait for that PUT to succeed, and the model
// does exactly that (`put_head_r1`: `let _ = core.put_since(…)`, then P9). §13 and B1 rely on "主體在、
// .r1 不在：寫入者還在兩次 PUT 之間，或已經當掉". A failed or timed-out `.r1` PUT (ordinary on S3) after
// which the writer goes on breaks that: the writer deletes, its head stays single-copy, and a later
// single-copy loss of that body (the modeled LoseLoneHeadBody, legal once nobody is about to write
// the `.r1`) erases the generation whose plan the deletions came from. The model cannot fail a PUT
// except with "exists"; the failure is emulated by removing the `.r1` right after the PUT step.
// (Moved into the repo: the failure is now the model's `FailHeadR1Put` fault, and the writer stops
// without deleting; see the file header.)
//
// Candidate fix (scratch-checked): P9 starts only once the head's `.r1` is confirmed (PUT success,
// "exists", or a HEAD); otherwise stop without deleting (settings and rebuild delete nothing).

fn r1_put_failed(muts: Mutations, fail: bool) -> (World, StepResult) {
    let mut w = World::new(
        Config {
            mutations: muts,
            ..quiet()
        },
        1,
    );
    let s0 = backup(&mut w, 1, 0, 0);
    let _s1 = backup(&mut w, 1, 0, 31);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 1
    forget(&mut w, s0); // not the newest of (1, 0)
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 2: s0-only packs condemned
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 3
    let p = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(p, "PutHeadR1")); // head 4 body: s0-only packs tombstoned
    if fail {
        w.apply(&Action::FailHeadR1Put); // the `.r1` PUT fails
    }
    let rp = w.finish(p);
    if !fail {
        assert_eq!(rp, Some(StepResult::Done));
        assert!(w.core.stats.packs_deleted > 0);
    }
    if fail {
        w.apply(&Action::LoseLoneHeadBody { pick: 0 });
        assert!(!w.core.backend.exists(&Key::Head(4)));
    }
    w.advance(HOUR);
    let r = prune(&mut w, 1);
    (w, r)
}

#[test]
fn g1_p9_after_a_failed_head_r1_put_break() {
    let (mut w, r) = r1_put_failed(Mutations::default(), true);
    w.advance(100 * HOUR);
    let later = prune(&mut w, 2);
    let v = w.check();
    assert!(
        r == StepResult::Done && v.is_empty(),
        "next prune: {r:?}\n100 h later: {later:?}\ncheck: {v:#?}\n{}",
        trace(&w, &["wrote head", "fault:", "stopped", "forget"])
    );
}

#[test]
fn g1_p9_after_a_failed_head_r1_put_control() {
    let (w, r) = r1_put_failed(Mutations::default(), false);
    assert_eq!(r, StepResult::Done);
    assert_ok(&w);
}

/// G1 with finding 11's preconditions (forged snapshot, two prunes on one base) is data loss, and
/// B1's lone-head rule does not help, because the generation that replaces the lost one is
/// durable. W1 lists before the forged snapshot (it tombstones s1's packs); W2 lists after it and
/// finishes its walk (it revives them); W1's `.r1` PUT fails, W1 deletes the packs; W1's lone body is
/// lost; W2 wins generation 4 with the packs live; s1's owner backs up the same tree, reuses its own
/// packs from durable head 4 and commits.
fn r1_put_failed_then_rewritten(muts: Mutations, fail: bool) -> World {
    let mut w = World::new(
        Config {
            mutations: muts,
            ..quiet()
        },
        1,
    );
    let s1 = backup(&mut w, 1, 0, 0);
    let root = match &w.core.backend.get(&Key::Snap(s1)).unwrap().content {
        Content::Snap(b) => b.root,
        _ => unreachable!(),
    };
    forget(&mut w, s1);
    age_and_prune(&mut w, 3, 80); // s1's packs condemned, age passed by the next base
    w.advance(80 * HOUR);
    let w1 = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(w1, "ListDeltas")); // W1 listed the snapshots
    w.apply(&Action::Mal(Mal::ForgedSnap {
        item: u16::try_from(root.0).unwrap(),
    }));
    let w2 = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(w2, "Plan")); // W2 saw the forged snapshot and walked it
    assert!(w.run_until(w1, "PutHeadR1")); // W1's head 4 body: s1's packs tombstoned
    if fail {
        w.apply(&Action::FailHeadR1Put); // W1's `.r1` PUT fails
    }
    let r1_result = w.finish(w1);
    if !fail {
        assert_eq!(r1_result, Some(StepResult::Done)); // P9 deletes s1's packs
        assert!(w.core.stats.packs_deleted > 0);
    }
    if fail {
        w.apply(&Action::LoseLoneHeadBody { pick: 0 });
        assert!(!w.core.backend.exists(&Key::Head(4)));
    }
    let r = w.finish(w2);
    if fail {
        assert_eq!(r, Some(StepResult::Done)); // W2 wrote generation 4
    } else {
        assert!(
            matches!(&r, Some(StepResult::Abort(m)) if m.contains("already exists")),
            "{r:?}"
        );
    }
    let b = w.start_backup(1, 0, 0, 0).unwrap();
    assert_eq!(w.finish(b), Some(StepResult::Done));
    w
}

#[test]
fn g1_failed_head_r1_put_then_rewrite_loses_data_break() {
    let w = r1_put_failed_then_rewritten(Mutations::default(), true);
    assert_eq!(
        r1(&w),
        "",
        "{}",
        trace(&w, &["wrote head", "fault:", "commit", "stopped"])
    );
}

#[test]
fn g1_failed_head_r1_put_then_rewrite_loses_data_control() {
    assert_eq!(
        r1(&r1_put_failed_then_rewritten(Mutations::default(), false)),
        ""
    );
}

// ==== M1 (model fidelity): a head's `.r1` can get an earlier mtime than its body ====
//
// §13: head 先寫主體再寫 .r1; lib.rs: 修改時間是請求開始的時間，物件在請求結束才看得到. The `.r1`
// request starts after the body request ended, so mtime(.r1) ≥ mtime(body). `put_since` bounds ε only
// by the writer's load time, so when the head is written later than the load, the two ε are drawn
// independently and the `.r1` can be up to `eps_max` older than the body. When such a body is lost,
// T(g) moves EARLIER by up to 20 min (the D1 direction is later). Within the ε budget, so harmless for
// the existing rules; listed so the implementation's T(g) does not copy it.
#[test]
fn m1_head_r1_mtime_can_precede_its_body() {
    let cfg = Config {
        repack: RepackPolicy::Never,
        ..Config::default()
    };
    let found = (1..300u64).find_map(|seed| {
        let mut w = World::new(cfg.clone(), seed);
        let p = w.start_prune(0, 0).unwrap();
        assert!(w.run_until(p, "PutShards"));
        w.advance(HOUR); // the head is written an hour after P1
        assert_eq!(w.finish(p), Some(StepResult::Done));
        let (body, r1c) = (mtime(&w, Key::Head(1)), mtime(&w, Key::HeadR1(1)));
        (r1c < body).then_some((seed, body, r1c))
    });
    assert_eq!(found, None, "(seed, mtime(body), mtime(.r1))");
}

// ==== Attacks that did not break it ====

/// Hold merge (scope 2): a second rebuild before the first hold ends. The slow deleter of head 4 is
/// parked at P9; rebuild 1 writes head 5 with hold {4, 4}; head 5's shard is lost too; rebuild 2
/// (g_r = 3 again) merges to hold {from 4, lost 5}. The deleter then finishes inside its run bound;
/// the held records vanish and are removed; no alarm, nothing referenced lost, GC settles.
#[test]
fn a11_second_rebuild_merges_the_hold() {
    let mut w = World::new(quiet(), 1);
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
    assert!(w.run_until(slow, "Delete")); // head 4 tombstones s0's packs
    let older = shards_of(&w, 3);
    for sid in shards_of(&w, 4) {
        if !older.contains(&sid) {
            w.core.backend.delete(&Key::Shard(sid));
            w.core.lost_shards.insert(sid);
            w.core.shard_fault = true;
        }
    }
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done)); // head 5
    w.advance(10 * HOUR);
    w.apply(&Action::LoseShard { pick: 0 }); // head 5 unreadable as well
    assert!(w.core.shard_fault);
    let rb = w.start_rebuild(2).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done)); // head 6
    assert!(log_has(&w, "rebuild: head 6 (g_lost 5, g_r 3"));
    let hold = match &w.core.backend.get(&Key::Head(6)).unwrap().content {
        Content::Head(h) => h.hold,
        _ => unreachable!(),
    };
    assert_eq!(
        hold.map(|h| (h.from, h.lost)),
        Some((4, 5)),
        "merged hold {hold:?}"
    );
    w.advance(60 * HOUR); // 70 h after the deleter's P0
    assert_eq!(prune(&mut w, 2), StepResult::Done); // host 0 is the parked deleter
    assert_eq!(w.finish(slow), Some(StepResult::Done));
    assert!(w.core.stats.packs_deleted > 0);
    for h in [1, 40, 40, 40] {
        w.advance(h * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        assert_ok(&w);
    }
    let v = w.settle();
    assert!(v.is_empty(), "{v:#?}");
}

/// P1's clamp (scope 5): the backend clock is 100 h fast while prune e writes head 3 (T(3) in the
/// future) and host 0 is 100 h ahead from right after prune d's P0, so d's clamp `min(T(3), wall + M)`
/// does not bite and n_t is 100 h in the future. Only P7d's T clause is fooled: w(3) came from e's
/// honest host. Two faults, sequential, but both on the "now" side of one window.
#[test]
fn a1_p1_clamp_bypassed_by_a_fast_host_only_fools_the_t_clause() {
    let mut w = World::new(quiet(), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1"));
    forget(&mut w, s0);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 2: condemned
    w.advance(HOUR);
    let e = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(e, "Load"));
    w.apply(&Action::BackendSkew(100 * 3600));
    assert_eq!(w.finish(e), Some(StepResult::Done)); // gen 3, T(3) 100 h ahead
    w.apply(&Action::BackendSkew(0));
    w.advance(HOUR);
    let d = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(d, "Load"));
    w.apply(&Action::SetSkew {
        host: 0,
        skew: 100 * 3600,
    });
    assert_eq!(w.finish(d), Some(StepResult::Done));
    w.apply(&Action::SetSkew { host: 0, skew: 0 });
    assert!(!log_has(&w, "ahead of the local clock")); // the clamp did not apply
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert_eq!(w.finish(b), Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
    let v = w.settle();
    assert!(v.is_empty(), "{v:#?}");
}

/// P5's bad-snapshot wait (scope 1) under the C1 pattern: backend 70 h slow while the first-sight head
/// is written, then a later prune's host 70 h ahead right after its P0. The wait is still a full
/// grace: its "now" is the P0 pair (N_P, local wall at P0), which P0 cross-checks, so a clock that
/// goes wrong after P0 never reaches it. This is the property C1–C3 lack.
#[test]
fn a2_p5_wait_measured_at_p0_resists_two_sequential_faults() {
    let mut w = World::new(quiet(), 1);
    let _s1 = backup(&mut w, 1, 0, 0);
    w.apply(&Action::Mal(Mal::JunkSnap { r1: true }));
    let p = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(p, "Load"));
    w.apply(&Action::BackendSkew(-70 * 3600));
    assert_eq!(w.finish(p), Some(StepResult::Done)); // head 1 records it; T(1) 70 h early
    w.apply(&Action::BackendSkew(0));
    w.advance(10 * HOUR);
    let q = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(q, "Load"));
    w.apply(&Action::SetSkew {
        host: 1,
        skew: 70 * 3600,
    });
    let r = w.finish(q).unwrap();
    w.apply(&Action::SetSkew { host: 1, skew: 0 });
    assert!(
        matches!(&r, StepResult::Abort(m) if m.contains("undecryptable")),
        "{r:?}"
    );
    w.advance(61 * HOUR); // 71 h after first sight
    let r = prune(&mut w, 0);
    assert!(
        matches!(&r, StepResult::Abort(m) if m.contains("undecryptable")),
        "{r:?}"
    );
    w.advance(2 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert_ok(&w);
}

/// Keep rule (scope 1/2): F3's timeline with the deleter's HOST clock 100 h behind (or 10 min behind)
/// instead of the backend clock. The deleter's own w-clause pair (w(2), w(3)) stays in the lost range,
/// so the w spread cannot drop under a; case B.
#[test]
fn a3_keep_rule_host_regression_at_the_deleter() {
    for skew in [-600, -100 * 3600] {
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
        let del = w.start_prune(0, 0).unwrap();
        assert!(w.run_until(del, "Load"));
        w.apply(&Action::SetSkew { host: 0, skew });
        assert!(w.run_until(del, "Delete"));
        w.apply(&Action::SetSkew { host: 0, skew: 0 });
        lose_shared_shard(&mut w, 2, 4);
        let rb = w.start_rebuild(1).unwrap();
        assert_eq!(w.finish(rb), Some(StepResult::Done));
        assert!(log_has(&w, "keep false"), "skew {skew}");
        let b = w.start_backup(1, 1, 0, 0).unwrap();
        assert_eq!(w.finish(b), Some(StepResult::Done));
        assert_eq!(w.finish(del), Some(StepResult::Done));
        assert_eq!(r1(&w), "", "skew {skew}");
    }
}

/// Rebuild case B's age (scope 2, "case B ages"): grace lowered 72 h → 2 h at head 2 with the backend
/// 10 h slow; a rebuild 62 h later (g_r = 2, lost heads 3..4, case B) must still give the condemned
/// records the old 72 h age (the drain is not over on the w clock), because the hold is only 2 h long
/// (every head in g_r..g_lost has grace 2 h). The `one_clock_windows` run shows the site carries
/// safety: there the age becomes 2 h and the backup that passed B4 under 72 h loses its packs.
/// review.rs only covers the drain end, the hold end and the keep rule with that mutation.
fn case_b_age(cfg: Config) -> World {
    let mut w = World::new(cfg, 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 1
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1")); // reuses s0's packs under 72 h
    let s = w.start_settings(1, 2 * HOUR).unwrap();
    assert!(w.run_until(s, "settings h1 PutHead"));
    w.apply(&Action::BackendSkew(-10 * 3600));
    assert_eq!(w.finish(s), Some(StepResult::Done)); // head 2: drain from 2, old 72 h
    w.apply(&Action::BackendSkew(0));
    forget(&mut w, s0);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 3: condemned, age 72 h (drain)
    w.advance(61 * HOUR); // 142 h: T(2) + 72 h = 142 h, w(2) + 72 h = 152 h
    assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 4
    lose_shared_shard(&mut w, 3, 4);
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done)); // head 5, case B
    assert!(
        log_has(&w, "g_r 2, keep false"),
        "{}",
        w.core.log.join("\n")
    );
    for h in [2, 0, 1] {
        w.advance(h * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
    }
    assert_eq!(w.finish(b), Some(StepResult::Done)); // E = 65 h
    w
}

#[test]
fn a4_case_b_age_follows_the_drain_on_both_clocks() {
    let w = case_b_age(quiet());
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

#[test]
fn a4_case_b_age_one_clock_windows_loses_data() {
    let w = case_b_age(with_mutations(named("one_clock_windows")));
    assert_ne!(r1(&w), "");
}

/// Long stalls (scope 1): a prune parked between P1 and its listing for 70 h, and one parked between
/// its plan and the head write for 70 h, while backups start and commit around them. The fence and
/// the run bound decide; nothing is lost.
#[test]
fn a5_prunes_stalled_inside_their_run_bound() {
    for park_at in ["ListSnaps", "PutShards"] {
        let mut w = World::new(quiet(), 1);
        let s0 = backup(&mut w, 1, 0, 0);
        let _s1 = backup(&mut w, 1, 0, 31);
        w.advance(80 * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        forget(&mut w, s0);
        w.advance(HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done); // s0-only packs condemned
        w.advance(72 * HOUR);
        let p = w.start_prune(0, 0).unwrap();
        assert!(w.run_until(p, park_at));
        let b = w.start_backup(2, 0, 0, 0).unwrap(); // reuses what is live
        w.advance(35 * HOUR);
        assert_eq!(w.finish(b), Some(StepResult::Done));
        let b2 = w.start_backup(3, 0, 0, 0).unwrap();
        w.advance(35 * HOUR);
        assert_eq!(w.finish(b2), Some(StepResult::Done));
        let _ = w.finish(p);
        assert_ok(&w);
        age_and_prune(&mut w, 4, 80);
        let v = w.settle();
        assert!(v.is_empty(), "{park_at}: {v:#?}");
    }
}

/// A backup parked right after B4's map check (before the delta HEADs and B5) for longer than the
/// bound is stopped by B5's elapsed-time check.
#[test]
fn a6_backup_stalled_between_gate_and_gate_heads() {
    let mut w = World::new(quiet(), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "GateHeads"));
    forget(&mut w, s0);
    age_and_prune(&mut w, 3, 80);
    assert!(w.core.stats.packs_deleted > 0);
    let r = w.finish(b);
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("took too long")),
        "{r:?}"
    );
    assert_ok(&w);
}

/// Doubt B (scope 6): B4 reads a single-copy head. The backup planned on durable head 1 (s0's packs
/// live); prune P writes head 2 (they are condemned) and crashes before `.r1`; the backup's B4 reads
/// that lone head 2 and passes; head 2's body is lost and prune Q rewrites generation 2 from head 1.
/// Q's head can only condemn what is live in head 1 again; deleting needs another full age after
/// that, by which time the backup has committed (B5) and its snapshot revives the packs. Variant:
/// the lone g_c is a settings head that lowers grace to 2 h; B5 then holds the backup to 1 h.
#[test]
fn a7_b4_on_a_lone_head() {
    for lone_settings in [false, true] {
        let mut w = World::new(quiet(), 1);
        let s0 = backup(&mut w, 1, 0, 0);
        w.advance(80 * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done); // gen 1, durable
        let b = w.start_backup(2, 0, 0, 0).unwrap();
        assert!(w.run_until(b, "Gate")); // B1/B2 on head 1
        forget(&mut w, s0);
        if lone_settings {
            let s = w.start_settings(1, 2 * HOUR).unwrap();
            assert!(w.run_until(s, "settings h1 PutHeadR1"));
        } else {
            let p = w.start_prune(0, 0).unwrap();
            assert!(w.run_until(p, "PutHeadR1"));
        }
        w.apply(&Action::CrashHeadWriter { pick: 0 });
        assert!(!w.core.backend.exists(&Key::HeadR1(2)));
        assert!(w.run_until(b, "PutSnapR1")); // B4 read the lone head 2
        w.apply(&Action::LoseLoneHeadBody { pick: 0 });
        assert!(!w.core.backend.exists(&Key::Head(2)));
        assert_eq!(prune(&mut w, 1), StepResult::Done); // Q rewrites generation 2
        w.advance(68 * HOUR);
        assert_eq!(prune(&mut w, 1), StepResult::Done);
        let r = w.finish(b);
        if lone_settings {
            assert!(
                matches!(&r, Some(StepResult::Abort(m)) if m.contains("took too long")),
                "{r:?}"
            );
        } else {
            assert_eq!(r, Some(StepResult::Done));
        }
        assert_ok(&w);
        age_and_prune(&mut w, 4, 80);
        let v = w.settle();
        assert!(v.is_empty(), "lone settings {lone_settings}: {v:#?}");
    }
}

/// Fence and repair races (scopes 3, 6): (1) a settings command repairs head 2's `.r1` while prune P
/// is still alive between its two PUTs, then wins head 3; P's own `.r1` PUT gets "exists" and P goes
/// on to P9; (2) two maintenance writers race for the same generation: the second one's HEAD of
/// `.r1` sees nothing, its body PUT gets "exists", it loses. No fork either way.
#[test]
fn a8_fence_and_repair_races() {
    let mut w = World::new(quiet(), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    let _s1 = backup(&mut w, 1, 0, 31);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    forget(&mut w, s0);
    w.advance(HOUR);
    let p = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(p, "PutHeadR1")); // head 2 body, P alive
    let s = w.start_settings(1, 96 * HOUR).unwrap();
    assert_eq!(w.finish(s), Some(StepResult::Done)); // repaired head 2's `.r1`, wrote head 3
    assert!(log_has(&w, "repaired the .r1 of head 2"));
    assert_eq!(w.finish(p), Some(StepResult::Done));
    let body = w.core.backend.get(&Key::Head(2)).map(|o| o.content.clone());
    let r1c = w
        .core
        .backend
        .get(&Key::HeadR1(2))
        .map(|o| o.content.clone());
    assert_eq!(body, r1c);
    let q = w.start_prune(0, 0).unwrap();
    let s2 = w.start_settings(2, 72 * HOUR).unwrap();
    assert!(w.run_until(q, "PutHead"));
    assert!(w.run_until(s2, "settings h2 PutHead"));
    assert_eq!(w.step(q), Some(StepResult::Continue)); // q's body of head 4
    let r = w.step(s2).unwrap();
    assert!(
        matches!(&r, StepResult::Abort(m) if m.contains("already exists")),
        "{r:?}"
    );
    assert_eq!(w.finish(q), Some(StepResult::Done));
    assert_ok(&w);
    age_and_prune(&mut w, 4, 100);
    let v = w.settle();
    assert!(v.is_empty(), "{v:#?}");
}

/// Status `unknown` (scope 7) through a condemn-all rebuild and its hold: kept, never used, never
/// deleted, and prune and rebuild keep working.
#[test]
fn a9_unknown_version_pack_through_a_case_b_rebuild() {
    let mut w = World::new(quiet(), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    let _s1 = backup(&mut w, 2, 1, 0);
    w.apply(&Action::Mal(Mal::UnknownVersionPack));
    let unknown = w.packs_of(MALICIOUS);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    forget(&mut w, s0);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    lose_shared_shard(&mut w, 2, 3);
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    assert!(log_has(&w, "keep false"));
    let b = w.start_backup(3, 1, 0, 0).unwrap();
    assert_eq!(w.finish(b), Some(StepResult::Done));
    age_and_prune(&mut w, 5, 80);
    assert_eq!(w.packs_of(MALICIOUS), unknown);
    let v = w.settle();
    assert!(v.is_empty(), "{v:#?}");
    assert_eq!(w.packs_of(MALICIOUS), unknown);
}

/// Grace raise (settings, scope 1): 2 h → 72 h while a backup that passed B4 under 2 h is parked.
/// Records condemned before the raise keep their 2 h age; the parked backup is held to
/// min(grace(g_s), grace(g_c)) − M = 1 h by B5, so it never outlives them.
#[test]
fn a10_grace_raise_under_a_parked_backup() {
    for park_h in [0, 2] {
        let cfg = Config {
            grace: 2 * HOUR,
            ..quiet()
        };
        let mut w = World::new(cfg, 1);
        let s0 = backup(&mut w, 1, 0, 0);
        w.advance(3 * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
        let b = w.start_backup(2, 0, 0, 0).unwrap();
        assert!(w.run_until(b, "PutSnapR1"));
        forget(&mut w, s0);
        assert_eq!(prune(&mut w, 0), StepResult::Done); // condemned, age 2 h
        let s = w.start_settings(1, 72 * HOUR).unwrap();
        assert_eq!(w.finish(s), Some(StepResult::Done));
        w.advance(park_h * HOUR);
        for _ in 0..3 {
            assert_eq!(prune(&mut w, 0), StepResult::Done);
        }
        let r = w.finish(b);
        if park_h == 0 {
            assert_eq!(r, Some(StepResult::Done));
        } else {
            assert!(
                matches!(&r, Some(StepResult::Abort(m)) if m.contains("took too long")),
                "{r:?}"
            );
        }
        assert_ok(&w);
    }
}

/// Census (ignored; `--run-ignored only`, release recommended): the table behind `s1`, `s2`, `c1`.
#[test]
#[ignore]
fn s_census_block_search_table() {
    for name in [
        "only_backend_clock",
        "only_writer_clock",
        "one_clock_windows",
        "no_elapsed_check",
        "default",
    ] {
        for mode in [Mode::Hosts, Mode::Backend, Mode::Both] {
            for seed in 1u8..=8 {
                let r = search(&with_mutations(named(name)), mode, 2000, seed);
                println!(
                    "{name:20} {mode:?} seed {seed}: {}",
                    if r.is_ok() { "none" } else { "FOUND" }
                );
            }
        }
    }
}

// ==== 修正之後的反向對照（搬進 repo 時加的）：恢復修正之前的規則，同一條交錯就違反 ====

#[test]
fn mutation_keep_rule_t_clause_loses_data() {
    let m = Mutations {
        keep_rule_t_clause: true,
        ..Mutations::default()
    };
    assert_ne!(r1(&late_r1_with(m, true)), "");
    assert_ne!(r1(&stalled_r1_writer_with(m, true)), "");
}

#[test]
fn mutation_now_from_base_head_only_loses_data() {
    let m = Mutations {
        now_from_base_head_only: true,
        ..Mutations::default()
    };
    let (w, _) = seq_clock_faults_with(m, true, true);
    assert_ne!(r1(&w), "");
    assert_ne!(r1(&drain_seq_with(m, true, true)), "");
    assert_ne!(r1(&hold_seq_with(m, true, true)), "");
}

#[test]
fn mutation_p9_ignores_head_r1_failure_stops_gc_or_loses_data() {
    let m = Mutations {
        p9_ignores_head_r1_failure: true,
        ..Mutations::default()
    };
    let (w, r) = r1_put_failed(m, true);
    assert!(r != StepResult::Done || !w.check().is_empty(), "{r:?}");
    assert_ne!(r1(&r1_put_failed_then_rewritten(m, true)), "");
}
