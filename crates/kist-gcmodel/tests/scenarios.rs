//! 確定的情境：協定照常運作、每一條規則都承擔安全性（拿掉就違反不變式）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use kist_gcmodel::*;

/// 非空轉：沒人用的資料在約兩個 grace 之後真的被刪掉，留下的 snapshot 仍然完整。
#[test]
fn garbage_is_deleted_after_about_two_graces() {
    let mut w = World::new(quiet(), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let _s1 = backup(&mut w, 1, 0, 31);
    forget(&mut w, s0);
    let before = pack_count(&w);
    age_and_prune(&mut w, 4, 80);
    assert_ok(&w);
    let after = pack_count(&w);
    assert!(
        w.core.stats.packs_deleted > 0,
        "nothing was deleted: {:?}",
        w.core.stats
    );
    assert!(after < before, "{before} packs before, {after} after");
}

fn violates_r1(w: &World) -> bool {
    w.check().iter().any(|m| m.starts_with("R1"))
}

fn with(muts: Mutations) -> Config {
    Config {
        mutations: muts,
        ..quiet()
    }
}

/// B4 通過之後停住超過一個 grace：c2 沿用 c1 的 pack，c1 退役，prune 把那些 pack 標記、過期、
/// 刪除；c2 醒來時 B5 的 E 檢查必須擋下。
fn backup_stalled_after_its_gate(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1"));
    forget(&mut w, s0);
    age_and_prune(&mut w, 3, 80);
    assert!(w.core.stats.packs_deleted > 0, "{:?}", w.core.stats);
    let r = w.finish(b);
    (w, r)
}

#[test]
fn a_backup_stalled_after_its_gate_is_refused() {
    let (w, r) = backup_stalled_after_its_gate(Mutations::default());
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("took too long")),
        "{r:?}"
    );
    assert_ok(&w);
}

#[test]
fn mutation_no_elapsed_check_commits_dangling_references() {
    let (w, r) = backup_stalled_after_its_gate(Mutations {
        no_elapsed_check: true,
        ..Mutations::default()
    });
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 推進並跑 prune，`rounds` 輪；回傳第一個違反（沒有就是 None）。
fn prune_rounds(w: &mut World, rounds: usize, hours: Time) -> Option<Vec<String>> {
    for _ in 0..rounds {
        w.advance(hours * HOUR);
        let _ = prune(w, 0);
        let v = w.check();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

/// 惡意的 pack F 宣稱持有 chunk 200（資料，P5 不會讀它）。repack 把 chunk 200 從稀疏的 pack 搬到
/// `by_prune` 的新 pack N；F 與 N 同一代進 map、名稱較小，所以排序在前、成為正本，N 與舊 pack 都
/// 不是正本。只有刪前驗證能發現 F 是假的。
fn false_claim_over_a_repack(muts: Mutations) -> (World, Option<Vec<String>>) {
    let cfg = Config {
        repack: RepackPolicy::Always,
        mutations: muts,
        ..quiet()
    };
    let mut w = World::new(cfg, 1);
    let sa = backup(&mut w, 1, 0, 0);
    let _sb = backup(&mut w, 1, 0, 3);
    forget(&mut w, sa);
    if let Some(v) = prune_rounds(&mut w, 1, 80) {
        return (w, Some(v));
    }
    let chunk = w.core.universe.intern(ItemDef::Chunk(200));
    w.mal_false_pack(&[chunk], Kind::Data, false);
    let v = prune_rounds(&mut w, 6, 80);
    (w, v)
}

#[test]
fn verify_before_drop_catches_a_false_canonical_holder() {
    let (w, v) = false_claim_over_a_repack(Mutations::default());
    assert_eq!(v, None);
    assert!(w.core.stats.repacked > 0, "{:?}", w.core.stats);
    assert!(w.core.stats.false_claims_removed > 0, "{:?}", w.core.stats);
    assert!(w.core.stats.packs_deleted > 0, "{:?}", w.core.stats);
}

#[test]
fn mutation_no_verify_before_drop_loses_data() {
    let (_, v) = false_claim_over_a_repack(Mutations {
        no_verify_before_drop: true,
        ..Mutations::default()
    });
    let v = v.expect("no violation");
    assert!(v.iter().any(|m| m.starts_with("R1")), "{v:#?}");
}

#[derive(Clone, Copy)]
enum Jump {
    /// prune e 的主機牆鐘在 P0 之後跳快 200 小時（`w(e)` 太大；下一輪的 P0 讀數擋住它）。
    WriterAhead,
    /// prune c 的主機牆鐘在 P1 之後慢了 80 小時（`w(c)` 太小；在 P1 之前慢，P1 就以本機時鐘 + M 當作
    /// 現在，什麼都不標）。
    WriterBehind,
    /// 後端時鐘在 P0 之後慢了 80 小時（`T(c)` 太小）。
    BackendBehind,
}

/// c2 沿用 c1 的 pack 並通過 B4；c1 退役，prune c 把那些 pack 標成 condemned。之後某個時鐘出錯，
/// 讓「已過一個 grace」在其中一個時鐘上提早成立。兩個時鐘都要過期才刪，所以 c2 照常 commit。
fn clock_fault(muts: Mutations, jump: Jump) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1"));
    forget(&mut w, s0);
    w.advance(HOUR);
    let c = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(c, "Load"));
    match jump {
        Jump::BackendBehind => w.core.backend.skew = -80 * HOUR,
        Jump::WriterBehind => {
            assert!(w.run_until(c, "ListSnaps"));
            w.core.skew.insert(0, -80 * HOUR);
        }
        Jump::WriterAhead => {}
    }
    assert_eq!(w.finish(c), Some(StepResult::Done));
    w.core.backend.skew = 0;
    w.core.skew.insert(0, 0);
    assert!(w.core.stats.condemned > 0, "{:?}", w.core.stats);
    w.advance(HOUR);
    let e = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(e, "Load"));
    if let Jump::WriterAhead = jump {
        w.core.skew.insert(1, 200 * HOUR);
    }
    assert_eq!(w.finish(e), Some(StepResult::Done));
    w.core.skew.insert(1, 0);
    w.advance(HOUR);
    let _ = prune(&mut w, 0);
    let r = w.finish(b);
    (w, r)
}

#[test]
fn a_writer_clock_jump_alone_does_not_delete_early() {
    let (w, r) = clock_fault(Mutations::default(), Jump::WriterAhead);
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

#[test]
fn a_writer_clock_lag_alone_does_not_delete_early() {
    let (w, r) = clock_fault(Mutations::default(), Jump::WriterBehind);
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

/// 只比 `w`：寫 head c 時主機的牆鐘慢了，`w(c)` 太小，下一輪就提早刪。原本用 `WriterAhead`
/// （`w(e)` 太大），「現在」改成與 P0 的讀數取小之後，下一輪 P0 的牆鐘是對的，那條交錯就騙不到
/// 只比 `w` 的規則了（第二輪外部審查）。
#[test]
fn mutation_only_writer_clock_deletes_early() {
    let (w, r) = clock_fault(
        Mutations {
            only_writer_clock: true,
            ..Mutations::default()
        },
        Jump::WriterBehind,
    );
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

#[test]
fn a_backend_clock_fault_alone_does_not_delete_early() {
    let (w, r) = clock_fault(Mutations::default(), Jump::BackendBehind);
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

#[test]
fn mutation_only_backend_clock_deletes_early() {
    let (w, r) = clock_fault(
        Mutations {
            only_backend_clock: true,
            ..Mutations::default()
        },
        Jump::BackendBehind,
    );
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 兩個 prune 以同一代為基礎：P 列表之後，c2 寫了新的 pack、delta 與 snapshot；Q 在那之後才列表。
/// P 先寫出 head 1（它的 map 沒有 c2 的新 pack）；Q 輸掉柵欄。Q 若照樣執行 P9，會刪掉那個 delta，
/// c2 的新 pack 就既不在 map、也不在任何 delta 裡。
fn two_prunes_one_generation(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let _s0 = backup(&mut w, 1, 0, 0);
    let p = w.start_prune(0, 0).unwrap();
    let q = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(p, "ListShards"));
    let _s1 = backup(&mut w, 2, 1, 5);
    assert!(w.run_until(q, "ListShards"));
    assert_eq!(w.finish(p), Some(StepResult::Done));
    let r = w.finish(q);
    (w, r)
}

#[test]
fn the_fence_stops_the_losing_prune() {
    let (w, r) = two_prunes_one_generation(Mutations::default());
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("already exists")),
        "{r:?}"
    );
    assert_ok(&w);
}

#[test]
fn mutation_no_fence_loses_the_only_pointer_to_new_packs() {
    let (w, _) = two_prunes_one_generation(Mutations {
        no_fence: true,
        ..Mutations::default()
    });
    let v = w.check();
    assert!(v.iter().any(|m| m.starts_with("findability")), "{v:#?}");
}

/// c1 退役，它的 pack 被標成 condemned 並已過期；c2 接著備份同樣的內容。B2 只沿用 live 的持有者，
/// 所以 c2 自己寫一份；若沿用 condemned 的，下一輪 prune（c2 還沒 commit）就會把它們刪掉。
fn reuse_after_condemnation(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    forget(&mut w, s0);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1"));
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let r = w.finish(b);
    (w, r)
}

#[test]
fn a_backup_does_not_reuse_condemned_holders() {
    let (w, r) = reuse_after_condemnation(Mutations::default());
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert!(w.core.stats.packs_deleted > 0, "{:?}", w.core.stats);
}

#[test]
fn mutation_reuse_condemned_loses_data() {
    let (w, r) = reuse_after_condemnation(Mutations {
        reuse_condemned: true,
        ..Mutations::default()
    });
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 沿用 parent 子樹時不查子樹裡的持有者，靠的是 parent 一直存在（§9 forget、§11.2 B4）。
///
/// c3 的 snapshot 含 chunk 100、200。c1 第一次備份（variant 2）時新寫子目錄 A'，裡面的 chunk 100、
/// 200 沿用 c3 的 pack。c3 退役（強制 forget，c3 沒有進行中的 backup，合乎程序規則），prune 在 c1
/// commit 之前把 c3 的 pack 標成 condemned。c1 的下一次備份（variant 6）從 parent 整棵沿用 A'；
/// 這時 parent 被強制 forget（違反程序規則），兩輪 prune 之後 c3 的 pack 過期被刪，chunk 100、200
/// 就沒有任何副本了。`forget_before_gate` 時 B4 的 HEAD 擋得住；在 B4 之後才 forget，只有程序規則
/// 擋得住。
fn parent_forgotten_during_child(
    muts: Mutations,
    forget_before_gate: bool,
) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
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
    let b = w.start_backup(1, 0, 6, 0).unwrap();
    let phase = if forget_before_gate {
        "Gate"
    } else {
        "PutSnapR1"
    };
    assert!(w.run_until(b, phase));
    forget(&mut w, parent);
    w.advance(13 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert!(
        w.core.stats.packs_deleted > 0,
        "{:?}\n{}",
        w.core.stats,
        w.core.log.join("\n")
    );
    let r = w.finish(b);
    (w, r)
}

#[test]
fn b4_notices_a_parent_forgotten_before_the_gate() {
    let (w, r) = parent_forgotten_during_child(Mutations::default(), true);
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("parent")),
        "{r:?}"
    );
    assert_ok(&w);
}

#[test]
fn mutation_no_parent_head_commits_a_subtree_whose_chunks_are_gone() {
    let (w, r) = parent_forgotten_during_child(
        Mutations {
            no_parent_head: true,
            ..Mutations::default()
        },
        true,
    );
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 程序規則承擔安全性：parent 在 B4 之後才被強制 forget，協定本身擋不住。
#[test]
fn forcing_out_a_parent_after_the_gate_breaks_r1_so_it_needs_the_procedure_rule() {
    let (w, r) = parent_forgotten_during_child(Mutations::default(), false);
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 被入侵的主機寫一個 delta，替 c1 自己的新 pack V 宣稱它有 chunk 401（V 沒有）。c1 的下一次備份
/// 會把「自己的 pack」當成可信。兩道關：delta 只能描述同一個前綴的 pack；B4 確認沿用的 ID 在
/// `Map(g_c)` 的記錄裡（V 在 B2 與 B4 之間被 prune 收錄，所以不走 trailer 那條檢查）。
fn foreign_delta_about_own_pack(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let _s1 = backup(&mut w, 1, 0, 1);
    let v = w
        .packs_of(1)
        .into_iter()
        .filter(|id| {
            matches!(
                w.core.backend.get(&Key::Pack { client: 1, id: *id }).map(|o| &o.content),
                Some(Content::Pack(p)) if p.kind == Kind::Data
            )
        })
        .max()
        .unwrap();
    w.core.universe.tree(0, 31);
    let chunk = w.core.universe.intern(ItemDef::Chunk(401));
    assert!(w.genuine_holders(chunk).is_empty());
    w.mal_foreign_delta(1, v, Kind::Data, &[chunk]);
    let b = w.start_backup(1, 0, 31, 0).unwrap();
    assert!(w.run_until(b, "Gate"));
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let r = w.finish(b);
    (w, r)
}

#[test]
fn a_foreign_delta_cannot_vouch_for_another_clients_pack() {
    let (w, r) = foreign_delta_about_own_pack(Mutations::default());
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
}

#[test]
fn without_the_prefix_rule_b4_still_catches_the_false_claim() {
    let (w, r) = foreign_delta_about_own_pack(Mutations {
        no_delta_prefix_rule: true,
        ..Mutations::default()
    });
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("no longer claims")),
        "{r:?}"
    );
    assert_ok(&w);
}

#[test]
fn without_the_b4_entries_check_the_prefix_rule_still_holds() {
    let (w, r) = foreign_delta_about_own_pack(Mutations {
        no_b4_entries_check: true,
        ..Mutations::default()
    });
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
}

#[test]
fn mutation_without_both_layers_commits_a_chunk_nobody_has() {
    let (w, r) = foreign_delta_about_own_pack(Mutations {
        no_delta_prefix_rule: true,
        no_b4_entries_check: true,
        ..Mutations::default()
    });
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 沿用別台的副本之前先驗證：被入侵的主機宣稱持有一個還沒有人存過的 chunk。
fn claim_of_an_unstored_chunk(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let _s1 = backup(&mut w, 1, 0, 0);
    w.core.universe.tree(0, 31);
    let chunk = w.core.universe.intern(ItemDef::Chunk(401));
    w.mal_false_pack(&[chunk], Kind::Data, true);
    let b = w.start_backup(2, 0, 31, 0).unwrap();
    let r = w.finish(b);
    (w, r)
}

#[test]
fn a_backup_verifies_a_foreign_copy_before_reusing_it() {
    let (w, r) = claim_of_an_unstored_chunk(Mutations::default());
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
}

#[test]
fn mutation_trust_all_commits_a_chunk_nobody_has() {
    let (w, r) = claim_of_an_unstored_chunk(Mutations {
        trust_all: true,
        ..Mutations::default()
    });
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// grace 從 72 小時調低到 10 小時時，一個在調低之前通過 B4 的 backup 仍以 72 小時為準；drain 期間
/// 新的 condemned 記錄用舊的 grace 當 age，所以它沿用的 pack 不會在 10 小時後就被刪。
fn grace_lowered_under_a_backup(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, 0).unwrap();
    assert!(w.run_until(b, "PutSnapR1"));
    forget(&mut w, s0);
    let s = w.start_settings(1, 10 * HOUR).unwrap();
    assert_eq!(w.finish(s), Some(StepResult::Done));
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(11 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let r = w.finish(b);
    (w, r)
}

#[test]
fn lowering_grace_drains_with_the_old_age() {
    let (w, r) = grace_lowered_under_a_backup(Mutations::default());
    assert_eq!(r, Some(StepResult::Done));
    assert_ok(&w);
    assert_eq!(w.core.stats.packs_deleted, 0);
}

#[test]
fn mutation_no_drain_deletes_under_an_older_backup() {
    let (w, r) = grace_lowered_under_a_backup(Mutations {
        no_drain: true,
        ..Mutations::default()
    });
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 重建時，在遺失的世代裡被標成 tombstone 的 pack 正被一個慢的 prune 刪除。hold 期間這些 pack 是
/// held：消失時直接移除記錄，下一輪 prune 照常完成。
fn rebuild_with_a_slow_deleter(muts: Mutations) -> (World, StepResult) {
    let mut w = World::new(with(muts), 1);
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
    assert!(w.run_until(slow, "Delete"));
    assert!(w.core.stats.tombstoned > 0, "{:?}", w.core.stats);
    // 最新 head 有、前一個 head 沒有的分片全部遺失。
    let heads: Vec<Gen> = w
        .core
        .backend
        .keys()
        .filter_map(|k| if let Key::Head(g) = k { Some(*g) } else { None })
        .collect();
    let newest = *heads.iter().max().unwrap();
    let shards = |g: Gen, w: &World| -> Vec<ShardId> {
        match &w.core.backend.get(&Key::Head(g)).unwrap().content {
            Content::Head(h) => h.shards.iter().flatten().copied().collect(),
            _ => Vec::new(),
        }
    };
    let older = shards(newest - 1, &w);
    for sid in shards(newest, &w) {
        if !older.contains(&sid) {
            w.core.backend.delete(&Key::Shard(sid));
            w.core.shard_fault = true;
        }
    }
    let rb = w.start_rebuild(1).unwrap();
    assert_eq!(w.finish(rb), Some(StepResult::Done));
    assert!(!w.core.shard_fault);
    assert_eq!(w.finish(slow), Some(StepResult::Done));
    assert!(w.core.stats.packs_deleted > 0);
    w.advance(HOUR);
    let r = prune(&mut w, 0);
    (w, r)
}

#[test]
fn a_rebuild_tolerates_a_slow_deleter() {
    let (w, r) = rebuild_with_a_slow_deleter(Mutations::default());
    assert_eq!(r, StepResult::Done, "{}", w.core.log.join("\n"));
    assert_ok(&w);
}

#[test]
fn mutation_no_hold_stops_gc_after_a_rebuild() {
    let (w, r) = rebuild_with_a_slow_deleter(Mutations {
        no_hold: true,
        ..Mutations::default()
    });
    assert!(
        matches!(&r, StepResult::Abort(m) if m.starts_with("P3")),
        "{r:?}"
    );
    let v = w.check();
    assert!(
        v.iter().any(|m| m.starts_with("false data-loss alarm")),
        "{v:#?}"
    );
    assert!(!v.iter().any(|m| m.starts_with("R1")), "{v:#?}");
}

/// 解不開的 snapshot：第一次看到時以一個不刪任何東西的世代記下來；之後一個 grace 內 prune 都停；
/// 過了就不再當成起點，主體與 `.r1` 都刪掉，GC 恢復。
#[test]
fn an_undecryptable_snapshot_pauses_gc_for_one_grace() {
    let mut w = World::new(quiet(), 1);
    let _s1 = backup(&mut w, 1, 0, 0);
    w.apply(&Action::Mal(Mal::JunkSnap { r1: true }));
    let first = prune(&mut w, 0);
    assert!(matches!(&first, StepResult::Done), "{first:?}");
    w.advance(10 * HOUR);
    let second = prune(&mut w, 0);
    assert!(
        matches!(&second, StepResult::Abort(m) if m.contains("undecryptable")),
        "{second:?}"
    );
    w.advance(70 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let junk = w
        .core
        .backend
        .keys()
        .any(|k| matches!(k, Key::Snap(s) | Key::SnapR1(s) if s.client == MALICIOUS));
    assert!(!junk, "the junk snapshot is still there");
    assert_ok(&w);
}

#[test]
fn forgetting_an_undecryptable_snapshot_unblocks_gc_at_once() {
    let mut w = World::new(quiet(), 1);
    let _s1 = backup(&mut w, 1, 0, 0);
    w.apply(&Action::Mal(Mal::JunkSnap { r1: false }));
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(HOUR);
    assert!(matches!(prune(&mut w, 0), StepResult::Abort(_)));
    let junk = w
        .core
        .backend
        .keys()
        .find_map(|k| match k {
            Key::Snap(s) if s.client == MALICIOUS => Some(*s),
            _ => None,
        })
        .unwrap();
    forget(&mut w, junk);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
}

/// trailer 解得開但版本不認得的新進 pack：記為 unknown，不使用、不刪除，也不擋住 prune 與重建
/// （外部審查 F4；原本 P4 拒絕，一個這樣的 pack 就讓 GC 永遠停住）。
#[test]
fn an_unknown_trailer_version_is_kept_and_does_not_block_prune() {
    let mut w = World::new(quiet(), 1);
    let _s1 = backup(&mut w, 1, 0, 0);
    w.apply(&Action::Mal(Mal::UnknownVersionPack));
    let kept = w.packs_of(MALICIOUS);
    assert_eq!(kept.len(), 1);
    for _ in 0..5 {
        w.advance(80 * HOUR);
        assert_eq!(prune(&mut w, 0), StepResult::Done);
    }
    assert_eq!(w.packs_of(MALICIOUS), kept);
    assert_ok(&w);
}

/// 寬鬆後端（沒有條件建立）：backup 照常，prune 在 P0 就拒絕。
#[test]
fn prune_refuses_a_permissive_backend() {
    let mut w = World::new(
        Config {
            create_only: false,
            ..quiet()
        },
        1,
    );
    let _s1 = backup(&mut w, 1, 0, 0);
    let r = prune(&mut w, 0);
    assert!(
        matches!(&r, StepResult::Abort(m) if m.starts_with("P0")),
        "{r:?}"
    );
    assert_ok(&w);
}

/// 規格的假設：head 可見的延遲 + 經過時間的低估 + snapshot PUT 的延遲 < M（§11.5）。這裡讓
/// backup 量到的經過時間少算 `under`：少算 2 小時（大於 M）時，它以為還在時限內，其實沿用的 pack
/// 已經被刪了；少算 30 分鐘時 B5 照樣擋下。
fn undercounted_backup(under: Time) -> (World, Option<StepResult>) {
    let cfg = Config {
        under_max: 3 * HOUR,
        ..quiet()
    };
    let mut w = World::new(cfg, 1);
    let s0 = backup(&mut w, 1, 0, 0);
    w.advance(80 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let b = w.start_backup(2, 0, 0, under).unwrap();
    assert!(w.run_until(b, "PutSnapR1"));
    forget(&mut w, s0);
    w.advance(60);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(72 * HOUR);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.advance(60);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    assert!(w.core.stats.packs_deleted > 0, "{:?}", w.core.stats);
    let r = w.finish(b);
    (w, r)
}

#[test]
fn within_the_timing_assumption_the_late_backup_is_refused() {
    let (w, r) = undercounted_backup(30 * 60);
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("took too long")),
        "{r:?}"
    );
    assert_ok(&w);
}

#[test]
fn breaking_the_timing_assumption_loses_data() {
    let (w, r) = undercounted_backup(2 * HOUR);
    assert_eq!(r, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}

/// 某次 prune 通過 P0 之後，後端時鐘跳快 74 小時才寫 head：head 的時間在未來。後端時鐘修好之後，
/// P1 以本機時鐘 + M 代替它，GC 立刻照常；原本 P1 拒絕，GC 要等真實時間追上那個時間戳（收尾活性
/// 檢查找到的最短交錯）。
fn prune_after_a_head_from_the_future(muts: Mutations) -> (World, StepResult) {
    let actions = [
        Action::StartPrune { host: 0, under: 0 },
        Action::Step { pick: 0 },
        Action::BackendSkew(266_401),
        Action::Finish { pick: 0 },
        Action::BackendSkew(0),
    ];
    let mut w = World::new(with(muts), 1);
    for a in &actions {
        w.apply(a);
    }
    w.advance(HOUR);
    let r = prune(&mut w, 0);
    (w, r)
}

#[test]
fn a_head_time_in_the_future_does_not_stop_gc() {
    let (mut w, r) = prune_after_a_head_from_the_future(Mutations::default());
    assert_eq!(r, StepResult::Done);
    let v = w.settle();
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn mutation_refusing_a_future_head_stops_gc() {
    let (_, r) = prune_after_a_head_from_the_future(Mutations {
        refuse_future_head: true,
        ..Mutations::default()
    });
    assert!(
        matches!(&r, StepResult::Abort(m) if m.contains("ahead of the local clock")),
        "{r:?}"
    );
}

/// head 1 的主體遺失、`.r1` 還在時，一個在 head 1 出現之前就讀了 head 0 的維護命令（改 settings）
/// 再寫 head 1。主體不在，「只在不存在時建立」會成功，原本就繞過了柵欄、蓋出一個 map 不同的第 1 代。
/// 柵欄先看那一代的 `.r1`，存在就算輸（深度隨機測試找到的交錯，手工重寫）。
#[test]
fn a_lost_head_body_cannot_be_recreated_by_a_stale_writer() {
    let (w, r) = stale_writer_after_a_lost_head_body(Mutations::default());
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("already exists")),
        "{r:?}"
    );
    assert_ok(&w);
}

#[test]
fn mutation_a_fence_that_ignores_the_replica_forks_a_generation() {
    let (w, r) = stale_writer_after_a_lost_head_body(Mutations {
        fence_ignores_r1: true,
        ..Mutations::default()
    });
    assert_eq!(r, Some(StepResult::Done));
    let v = w.check();
    assert!(v.iter().any(|m| m.starts_with("findability")), "{v:#?}");
}

fn stale_writer_after_a_lost_head_body(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let s1 = backup(&mut w, 1, 1, 0);
    let s = w.start_settings(1, 2 * HOUR).unwrap();
    assert!(w.run_until(s, "settings h1 PutHead"));
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    w.apply(&Action::LoseHeadBody { pick: 1 });
    assert!(!w.core.backend.exists(&Key::Head(1)));
    assert!(w.core.backend.exists(&Key::HeadR1(1)));
    let r = w.finish(s);
    let reader = w.start_reader(s1);
    let _ = w.finish(reader);
    (w, r)
}

/// 一個 prune 寫了 head 2 的主體、還沒寫 `.r1` 就當掉；改 settings 讀了只有一份的 head 2，在上面寫
/// head 3；之後 head 2 的主體遺失，世代之間出現空洞。head 3 的分片再遺失時，重建要讀 `g_r+1..g_lost`
/// 每一代的 head，讀不到 head 2 就永遠拒絕，GC 與所有 backup 都停住（加入「寫入者當掉後主體遺失」的
/// 故障後隨機測試找到的交錯，手工重寫）。維護角色在一代上面建下一代之前先補寫它的 `.r1`。
fn a_successor_on_a_lone_head(muts: Mutations) -> (World, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    backup(&mut w, 1, 1, 0);
    assert_eq!(prune(&mut w, 0), StepResult::Done);
    let p = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(p, "PutHeadR1"));
    w.apply(&Action::CrashHeadWriter { pick: 0 });
    assert!(w.core.backend.exists(&Key::Head(2)));
    assert!(!w.core.backend.exists(&Key::HeadR1(2)));
    let s = w.start_settings(1, 96 * HOUR).unwrap();
    assert!(w.run_until(s, "settings h1 PutHead"));
    // head 2 的主體遺失：補寫過 `.r1` 就是一般的主體遺失，沒有的話那是這一代唯一的一份。
    if w.core.backend.exists(&Key::HeadR1(2)) {
        w.apply(&Action::LoseHeadBody { pick: 2 });
    } else {
        w.apply(&Action::LoseLoneHeadBody { pick: 0 });
    }
    assert!(!w.core.backend.exists(&Key::Head(2)));
    assert_eq!(w.finish(s), Some(StepResult::Done));
    w.apply(&Action::LoseShard { pick: 0 });
    let r = w.start_rebuild(2).and_then(|id| w.finish(id));
    (w, r)
}

#[test]
fn a_maintenance_writer_completes_a_lone_head_before_building_on_it() {
    let (mut w, r) = a_successor_on_a_lone_head(Mutations::default());
    assert_eq!(r, Some(StepResult::Done), "log:\n{}", w.core.log.join("\n"));
    let v = w.settle();
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn mutation_no_head_repair_leaves_a_gap_that_stops_rebuilds() {
    let (w, r) = a_successor_on_a_lone_head(Mutations {
        no_head_repair: true,
        ..Mutations::default()
    });
    assert!(
        matches!(&r, Some(StepResult::Abort(m)) if m.contains("head 2 is missing")),
        "{r:?}\nlog:\n{}",
        w.core.log.join("\n")
    );
}

/// 一個 backup 照只有一份的 head 決定沿用，之後那份遺失、另一個 prune 以舊計畫重新寫出那一代。s1 退役
/// 後它的 pack 是 condemned、age 已過；兩個 prune 以 head 3 為基礎，W2 先列出 snapshot，被入侵的主機
/// 接著寫一個引用 s1 那棵樹的 snapshot，W1 看到它而把那些 pack 復活，寫了 head 4 的主體、還沒寫 `.r1`
/// 就當掉。client 1 的 backup 以只有一份的 head 4 沿用自己寫的那些 pack、通過 B4；head 4 的主體遺失，
/// W2 的柵欄看不到任何痕跡，以沒看過偽造 snapshot 的計畫寫出 head 4 並刪掉那些 pack；backup 接著
/// commit（加入「寫入者當掉後主體遺失」的故障後，照模型推出來的交錯）。B1 只從兩份都在的 head 決定沿用。
fn a_backup_on_a_lone_head(muts: Mutations) -> (World, bool, Option<StepResult>) {
    let mut w = World::new(with(muts), 1);
    let s1 = backup(&mut w, 1, 0, 0);
    let root = match &w.core.backend.get(&Key::Snap(s1)).unwrap().content {
        Content::Snap(b) => b.root,
        _ => unreachable!(),
    };
    forget(&mut w, s1);
    age_and_prune(&mut w, 3, 80);
    w.advance(80 * HOUR);
    let w2 = w.start_prune(1, 0).unwrap();
    assert!(w.run_until(w2, "ListDeltas"));
    w.apply(&Action::Mal(Mal::ForgedSnap {
        item: u16::try_from(root.0).unwrap(),
    }));
    let w1 = w.start_prune(0, 0).unwrap();
    assert!(w.run_until(w1, "PutHeadR1"));
    w.apply(&Action::CrashHeadWriter { pick: 0 });
    let b = w.start_backup(1, 0, 0, 0).unwrap();
    let past_b4 = w.run_until(b, "PutSnapR1");
    w.apply(&Action::LoseLoneHeadBody { pick: 0 });
    assert!(!w.core.backend.exists(&Key::Head(4)));
    assert_eq!(w.finish(w2), Some(StepResult::Done));
    let rb = w.finish(b);
    (w, past_b4, rb)
}

#[test]
fn a_backup_plans_only_on_a_head_with_both_copies() {
    let (mut w, past_b4, _) = a_backup_on_a_lone_head(Mutations::default());
    assert!(!past_b4);
    assert!(
        w.core
            .log
            .iter()
            .any(|l| l.contains("B1: head 4 has no .r1 yet")),
        "log:\n{}",
        w.core.log.join("\n")
    );
    assert_ok(&w);
    let v = w.settle();
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn mutation_a_backup_on_a_lone_head_loses_data() {
    let (w, past_b4, rb) = a_backup_on_a_lone_head(Mutations {
        backup_trusts_lone_head: true,
        ..Mutations::default()
    });
    assert!(past_b4);
    assert_eq!(rb, Some(StepResult::Done));
    assert!(violates_r1(&w), "{:#?}", w.check());
}
