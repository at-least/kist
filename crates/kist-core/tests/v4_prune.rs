//! v4 的 prune（format-v4.md §11.3 的 P0–P9）：每一條規則一個測試，時間走虛擬時鐘（[`VClock`]）。
//!
//! 一輪 prune 的「現在」是基礎世代（上一輪寫的 head）的時間與 P0 的讀數取小（§11.1），所以「過了一個
//! grace」要隔著一輪：先前進、prune 一次（寫出時間夠晚的 head），再 prune 一次才看得到。

mod v4_common;

use std::collections::BTreeMap;

use kist_core::v4::SettingsChange;
use kist_core::CoreError;
use kist_format::v4::ids::{MetaId, ObjectId};
use kist_format::v4::index::{status, Record, Status};
use kist_format::v4::keys::{Key, SnapTs};
use kist_format::v4::meta::node_type;
use kist_format::v4::pack::Kind;
use kist_format::v4::snapshot::Root;
use v4_common::harness::*;
use v4_common::*;

const HOUR: u64 = 3600;

#[tokio::test]
async fn a_first_prune_records_every_pack_as_live_and_absorbs_the_deltas() {
    let t = setup().await;
    t.write("a", "f", &data("a", 200 * 1024));
    t.backup(1, "a").await;
    assert!(!t.hooked.keys("deltas").await.is_empty());
    let r = t.prune().await;
    assert_eq!((r.base, r.generation), (0, 1));
    let recs = t.records().await;
    let mut packs = t.pack_ids("packs").await;
    packs.extend(t.pack_ids("mpacks").await);
    assert_eq!(recs.len(), packs.len());
    assert_eq!(r.newcomers as usize, packs.len());
    for id in &packs {
        let rec = &recs[id];
        assert_eq!(rec.status.s, status::LIVE);
        assert_eq!(rec.since, 1);
        assert!(!rec.by_prune);
        assert!(!rec.entries.is_empty());
    }
    assert!(
        t.hooked.keys("deltas").await.is_empty(),
        "every pack of the delta is in Map(1)"
    );
    assert_eq!(r.snapshots, 1);
    assert!(r.items > 0 && r.chunks > 0);
}

/// 沒有被引用的 pack：過了一個 grace 標成 condemned，再過一個 age 標成 tombstone 並刪除，再過一個 grace
/// 記錄才移除。被引用的 pack 一路都是 live。
#[tokio::test]
async fn an_unreferenced_pack_goes_live_condemned_tombstone_deleted_then_unrecorded() {
    let t = setup().await;
    t.write("keep", "k", &data("keep", 100 * 1024));
    t.write("gone", "g", &data("gone", 100 * 1024));
    t.backup(1, "keep").await;
    let before = t.all_packs().await;
    let gone = t.backup(1, "gone").await;
    let theirs: Vec<ObjectId> = t
        .all_packs()
        .await
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    assert!(!theirs.is_empty());
    t.forget(&gone).await;

    t.prune().await; // g1：全部收錄成 live
    t.prune_after(73).await; // g2：基礎世代 g1 還太新，不標
    for id in &theirs {
        assert_eq!(status_of(&t.records().await, id), Some(status::LIVE));
    }
    let r = t.prune_after(73).await; // g3：condemned
    assert!(r.condemned >= theirs.len() as u64);
    let recs = t.records().await;
    for id in &theirs {
        let rec = &recs[id];
        assert_eq!(rec.status.s, status::CONDEMNED, "{id}");
        assert_eq!((rec.status.at, rec.status.age), (3, 72 * 3600));
    }
    for id in &before {
        assert_eq!(status_of(&recs, id), Some(status::LIVE), "referenced");
    }
    t.prune_after(73).await; // g4：condemned 在 g3，基礎世代 g3 還不到 age
    for id in &theirs {
        assert_eq!(status_of(&t.records().await, id), Some(status::CONDEMNED));
    }
    let r = t.prune_after(73).await; // g5：tombstone，P9 刪除
    assert_eq!(r.tombstoned, theirs.len() as u64);
    assert_eq!(r.deleted_packs, theirs.len() as u64);
    let recs = t.records().await;
    let listed = t.all_packs().await;
    for id in &theirs {
        assert_eq!(status_of(&recs, id), Some(status::TOMBSTONE));
        assert!(!listed.contains(id), "{id} deleted");
    }
    for id in &before {
        assert!(listed.contains(id), "{id} kept");
    }
    t.prune_after(73).await; // g6：tombstone 不在列表了，基礎世代 g5 還不到 grace
    for id in &theirs {
        assert_eq!(status_of(&t.records().await, id), Some(status::TOMBSTONE));
    }
    let r = t.prune_after(73).await; // g7：記錄移除
    assert!(r.removed_records >= theirs.len() as u64);
    let recs = t.records().await;
    for id in &theirs {
        assert!(!recs.contains_key(id));
    }
}

/// P7a：condemned 的 pack 又被需要（開始得早的 backup 晚到的 commit 引用它）就復活。
#[tokio::test]
async fn p7a_a_condemned_pack_needed_again_is_revived() {
    let t = setup().await;
    t.write("keep", "k", &data("keep", 100 * 1024));
    t.write("late", "l", &data("late", 100 * 1024));
    t.backup(1, "keep").await;
    let before = t.all_packs().await;
    let late = t.backup(1, "late").await;
    let theirs: Vec<ObjectId> = t
        .all_packs()
        .await
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    let body = t.repo.backend().get(&late).await.unwrap();
    t.forget(&late).await;
    condemn(&t, &theirs).await;
    t.repo.backend().put_if_absent(&late, body).await.unwrap();
    let r = t.prune().await;
    assert_eq!(r.revived, theirs.len() as u64);
    let recs = t.records().await;
    for id in &theirs {
        assert_eq!(status_of(&recs, id), Some(status::LIVE));
        assert!(!recs[id].held);
    }
}

/// drain 期間新的 condemned 記錄用舊的 grace 當 age（§11.4）；drain 在兩個時鐘上都過了之後的一輪拿掉它。
#[tokio::test]
async fn new_condemned_records_age_by_the_old_grace_during_a_drain() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    t.prune().await; // g1
    t.repo
        .change_settings(
            SettingsChange {
                grace: Some(2 * HOUR),
                ..Default::default()
            },
            t.clock.maint(),
        )
        .await
        .unwrap(); // g2：drain from 2
    t.prune_after(3).await; // g3：基礎世代 g2 的 T 還不夠晚
    t.prune_after(3).await; // g4：condemned
    let recs = t.records().await;
    for id in &theirs {
        assert_eq!(recs[id].status.s, status::CONDEMNED);
        assert_eq!(recs[id].status.age, 72 * HOUR, "the old grace, not 2h");
    }
    let g = t.newest().await;
    assert!(t.repo.read_head(g).await.unwrap().head.drain.is_some());
    t.prune_after(73).await;
    t.prune_after(1).await;
    let g = t.newest().await;
    assert_eq!(
        t.repo.read_head(g).await.unwrap().head.drain,
        None,
        "the drain ended"
    );
}

/// P3：map 裡 live 的 pack 不在列表裡是資料遺失，拒絕、什麼都不寫。用沒有被引用的 pack：P5 不會換個理由
/// 擋下來，這裡驗的是 P3 本身。
#[tokio::test]
async fn p3_a_live_pack_missing_from_the_listing_is_refused() {
    let t = setup().await;
    let plain = data("x", 3000);
    let k = t.repo.keys().chunk_id(&plain);
    let (x, _) = t.hand_pack(1, Kind::Data, &[(k.into(), plain)]).await;
    t.prune().await;
    assert_eq!(status_of(&t.records().await, &x), Some(status::LIVE));
    t.repo
        .backend()
        .delete(
            &Key::Pack {
                client: client(1),
                id: x,
            }
            .path(),
        )
        .await
        .unwrap();
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("in the map but missing")),
        "{err}"
    );
    assert_eq!(t.newest().await, 1);
}

/// P3 的拒絕之前重新列出世代：已有更新的 head（另一個 prune 贏了、刪了它）就只是輸了。
#[tokio::test]
async fn p3_a_newer_generation_turns_the_refusal_into_superseded() {
    let t = setup().await;
    t.write("a", "f", &data("a", 100 * 1024));
    t.backup(1, "a").await;
    t.prune().await;
    let gone = t.pack_ids("packs").await[0];
    t.repo
        .backend()
        .delete(
            &Key::Pack {
                client: client(1),
                id: gone,
            }
            .path(),
        )
        .await
        .unwrap();
    t.hooked.store.after_get.lock().unwrap().push((
        Key::Head {
            g: 1,
            replica: false,
        }
        .path(),
        Key::Head {
            g: 2,
            replica: true,
        }
        .path(),
        b"x".to_vec(),
    ));
    let err = t.try_prune().await.unwrap_err();
    assert!(matches!(err, CoreError::Superseded(_)), "{err}");
}

/// P3：held 的記錄消失時直接移除（它可能在重建之前就被當時的 prune 刪掉了，§11.4）。
#[tokio::test]
async fn p3_a_missing_held_record_is_dropped() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    t.prune().await;
    let id = theirs[0];
    t.hand_generation(|recs, _| {
        let r = recs.get_mut(&id).unwrap();
        r.status = Status {
            s: status::CONDEMNED,
            at: 2,
            age: 72 * HOUR,
        };
        r.held = true;
    })
    .await;
    let rec = t.records().await[&id].clone();
    t.repo
        .backend()
        .delete(&kist_core::v4::state::pack_key(rec.kind, rec.client, id).path())
        .await
        .unwrap();
    let r = t.prune().await;
    assert_eq!(r.removed_records, 1);
    assert!(!t.records().await.contains_key(&id));
}

/// P4：trailer 解得開、版本不認得 → unknown：不使用、不刪除，一直留著（ADR 026 負責人決定 10）。
#[tokio::test]
async fn p4_a_pack_with_an_unknown_trailer_version_is_kept_and_never_deleted() {
    let t = setup().await;
    let id = t.odd_pack(1, Some(5)).await;
    let r = t.prune().await;
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("unknown trailer version")),
        "{:?}",
        r.warnings
    );
    let rec = t.records().await[&id].clone();
    assert_eq!(rec.status.s, status::UNKNOWN);
    assert!(rec.entries.is_empty());
    for _ in 0..5 {
        t.prune_after(73).await;
    }
    assert_eq!(status_of(&t.records().await, &id), Some(status::UNKNOWN));
    assert!(t.pack_ids("packs").await.contains(&id));
}

/// P4：trailer 解不開 → unreadable；P7e：進 map 之後過了一個 grace → condemned，之後照常刪除。
#[tokio::test]
async fn p4_an_unreadable_pack_is_condemned_a_grace_after_it_arrived_then_deleted() {
    let t = setup().await;
    let id = t.odd_pack(1, None).await;
    t.prune().await; // g1：unreadable，since 1
    assert_eq!(status_of(&t.records().await, &id), Some(status::UNREADABLE));
    t.prune_after(73).await; // g2：T(1) + grace > T(g1)
    assert_eq!(status_of(&t.records().await, &id), Some(status::UNREADABLE));
    t.prune_after(73).await; // g3
    let rec = t.records().await[&id].clone();
    assert_eq!((rec.status.s, rec.status.at), (status::CONDEMNED, 3));
    t.prune_after(73).await;
    t.prune_after(73).await;
    assert_eq!(status_of(&t.records().await, &id), Some(status::TOMBSTONE));
    assert!(!t.pack_ids("packs").await.contains(&id), "deleted");
}

/// P5：被引用的 item 沒有任何持有者 → 拒絕，什麼都不寫。
#[tokio::test]
async fn p5_an_item_without_any_holder_is_refused() {
    let t = setup().await;
    let at = hours_from_now(-1);
    hand_snapshot(
        &t.repo,
        client(1),
        at,
        vec![Root {
            path: b"/x".to_vec(),
            t: node_type::DIR,
            tree: Some(MetaId::from_bytes([9; 32])),
            entry: None,
        }],
        vec![],
    )
    .await;
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("no readable holder")),
        "{err}"
    );
    assert_eq!(t.newest().await, 0);
}

/// P5：解得開但版本不認得的 snapshot → 拒絕（較新的 kist 寫的），不記進 `bad`、不寫 head。
#[tokio::test]
async fn p5_a_snapshot_with_an_unknown_version_is_refused() {
    let t = setup().await;
    #[derive(serde::Serialize)]
    struct Snap {
        v: u32,
        roots: MetaId,
        time: i64,
        host: String,
        client: kist_format::v4::ids::ClientId,
    }
    let at = hours_from_now(-1);
    let ts = SnapTs::from_nanos(at).unwrap();
    let plain = kist_format::cbor::encode(&Snap {
        v: 5,
        roots: MetaId::from_bytes([1; 32]),
        time: at,
        host: "future".into(),
        client: client(1),
    })
    .unwrap();
    let sealed = t
        .repo
        .keys()
        .seal_snapshot(&client(1), &ts, &plain)
        .unwrap();
    let key = Key::Snapshot {
        client: client(1),
        ts,
        replica: false,
    };
    t.repo
        .backend()
        .put_if_absent(&key.path(), sealed)
        .await
        .unwrap();
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("newer kist")),
        "{err}"
    );
    assert_eq!(t.newest().await, 0);
}

/// P5：只有 `.r1` 的 snapshot 照樣當成起點（commit 前中斷，或主體遺失），並回報。
#[tokio::test]
async fn p5_an_r1_only_snapshot_is_walked_and_reported() {
    let t = setup().await;
    t.write("a", "f", &data("a", 100 * 1024));
    let key = t.backup(1, "a").await;
    let packs = t.all_packs().await;
    t.repo.backend().delete(&key).await.unwrap();
    let r = t.prune().await;
    assert_eq!(r.snapshots, 1);
    assert!(
        r.warnings.iter().any(|w| w.contains("only its .r1")),
        "{:?}",
        r.warnings
    );
    t.prune_after(73).await;
    t.prune_after(73).await;
    let recs = t.records().await;
    for id in &packs {
        assert_eq!(
            status_of(&recs, id),
            Some(status::LIVE),
            "{id} is referenced"
        );
    }
}

/// P5：排在前面的持有者的副本是假的（位元損壞）→ 從它的記錄移除、回報，改用下一個持有者。
#[tokio::test]
async fn p5_a_false_metadata_copy_is_removed_and_the_next_holder_used() {
    let t = setup().await;
    let mut link = entry(
        b"x",
        node_type::SYMLINK,
        kist_format::v4::meta::meta_kind::GENERIC,
    );
    link.target = b"t".to_vec();
    let seg = kist_format::v4::meta::Item::Seg(kist_format::v4::meta::Seg::new(vec![link]));
    let cbor = seg.encode().unwrap();
    let id = t.repo.keys().meta_id(&cbor);
    let (a, ea) = t
        .hand_pack(1, Kind::Meta, &[(id.into(), cbor.clone())])
        .await;
    let (b, eb) = t
        .hand_pack(2, Kind::Meta, &[(id.into(), cbor.clone())])
        .await;
    // 排序：同一個 since，名稱小的先
    let (first, fc, fe) = if a < b { (a, 1, ea[0]) } else { (b, 2, eb[0]) };
    let second = if a < b { b } else { a };
    damage(
        &t,
        &Key::Mpack {
            client: client(fc),
            id: first,
            replica: false,
        },
        &fe,
    )
    .await;
    hand_snapshot(
        &t.repo,
        client(3),
        hours_from_now(-1),
        vec![Root {
            path: b"/d".to_vec(),
            t: node_type::DIR,
            tree: Some(id),
            entry: None,
        }],
        vec![],
    )
    .await;
    let r = t.prune().await;
    assert_eq!(r.false_entries, 1);
    assert!(
        r.warnings.iter().any(|w| w.contains("copy is false")),
        "{:?}",
        r.warnings
    );
    let recs = t.records().await;
    assert!(recs[&first].entries.iter().all(|e| e.i != id.into()));
    assert!(recs[&second].entries.iter().any(|e| e.i == id.into()));
}

/// P7d 的刪前驗證：condemned 的 X 過了 age，但它持有的 chunk 的正本 C（不是 by_prune）的副本是假的 →
/// 從 C 的記錄移除、X 這一輪留在 condemned；X 成了正本、復活。不驗證的話 X 被刪，chunk 只剩假的副本。
#[tokio::test]
async fn p7d_a_false_canonical_copy_keeps_the_condemned_holder() {
    let t = setup().await;
    let plain = data("chunk", 3000);
    let k = t.repo.keys().chunk_id(&plain);
    let (a, ea) = t
        .hand_pack(1, Kind::Data, &[(k.into(), plain.clone())])
        .await;
    let (b, eb) = t
        .hand_pack(2, Kind::Data, &[(k.into(), plain.clone())])
        .await;
    hand_snapshot(
        &t.repo,
        client(3),
        hours_from_now(-1),
        vec![file_root("f", k, plain.len() as u64)],
        vec![],
    )
    .await;
    // 名稱小的是正本 C，另一個 X 沒有被當成正本，過了 grace 標成 condemned
    let (c, cc, ce, x) = if a < b {
        (a, 1, ea[0], b)
    } else {
        (b, 2, eb[0], a)
    };
    condemn(&t, &[x]).await;
    t.prune_after(73).await; // g4：還不到 age
    damage(
        &t,
        &Key::Pack {
            client: client(cc),
            id: c,
        },
        &ce,
    )
    .await;
    let r = t.prune_after(73).await; // g5：X 過了 age，刪前驗證 C 的副本
    assert_eq!(r.false_entries, 1);
    assert_eq!(r.tombstoned, 0);
    let recs = t.records().await;
    assert!(
        recs[&c].entries.iter().all(|e| e.i != k.into()),
        "removed from C"
    );
    assert_eq!(
        recs[&x].status.s,
        status::LIVE,
        "X is the canonical holder now"
    );
    assert!(t.pack_ids("packs").await.contains(&x));
}

/// P7d 的兩個時鐘：X 在後端時鐘慢了 100 小時時標成 condemned（`T(c)` 早了）：只看 `T` 的話一輪之後就過了
/// age，`w` 擋住。
#[tokio::test]
async fn p7d_a_slow_backend_clock_at_condemnation_does_not_shorten_the_age() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    t.prune().await;
    t.clock.set_backend_skew(-100 * 3600);
    let c = t
        .hand_generation(|recs, h| {
            for id in &theirs {
                recs.get_mut(id).unwrap().status = Status {
                    s: status::CONDEMNED,
                    at: h.g,
                    age: 72 * HOUR,
                };
            }
        })
        .await;
    t.clock.set_backend_skew(0);
    t.prune_after(2).await;
    t.prune_after(1).await;
    let recs = t.records().await;
    for id in &theirs {
        assert_eq!(recs[id].status.s, status::CONDEMNED, "condemned at {c}");
    }
}

/// 同上，寫入者的時鐘慢了 100 小時（`w(c)` 早了）：`T` 擋住。
#[tokio::test]
async fn p7d_a_slow_writer_clock_at_condemnation_does_not_shorten_the_age() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    t.prune().await;
    t.hand_generation(|recs, h| {
        h.w -= 100 * 3600 * 1_000_000_000;
        for id in &theirs {
            recs.get_mut(id).unwrap().status = Status {
                s: status::CONDEMNED,
                at: h.g,
                age: 72 * HOUR,
            };
        }
    })
    .await;
    t.prune_after(2).await;
    t.prune_after(1).await;
    let recs = t.records().await;
    for id in &theirs {
        assert_eq!(recs[id].status.s, status::CONDEMNED);
    }
}

/// P9：Object Lock 擋下刪除 → 回報、記錄留在 tombstone；下一輪刷新（P7f）、再試一次。
#[tokio::test]
async fn p9_a_refused_deletion_keeps_the_tombstone_and_is_retried() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    condemn(&t, &theirs).await;
    t.prune_after(73).await;
    let recs = t.records().await;
    let victim = theirs
        .iter()
        .find(|id| recs[*id].kind == Kind::Data)
        .copied()
        .unwrap();
    let key = Key::Pack {
        client: client(1),
        id: victim,
    }
    .path();
    t.hooked.store.fail_delete.lock().unwrap().push(key.clone());
    let r = t.prune_after(73).await; // g5：tombstone，刪除被擋
    assert_eq!(r.undeleted.len(), 1, "{:?}", r.undeleted);
    assert!(t.pack_ids("packs").await.contains(&victim));
    let rec = t.records().await[&victim].clone();
    assert_eq!((rec.status.s, rec.status.at), (status::TOMBSTONE, 5));
    let r = t.prune().await; // g6：刷新、再刪
    assert!(r.undeleted.is_empty());
    assert!(!t.pack_ids("packs").await.contains(&victim));
    let rec = t.records().await[&victim].clone();
    assert_eq!((rec.status.s, rec.status.at), (status::TOMBSTONE, 6));
}

/// P1：最新的 head 的後端時間比本機時鐘晚超過 M → 回報，以本機時鐘 + M 當作現在；只看 `T` 的規則（P7b）
/// 不會因此把剛寫的 pack 標掉。
#[tokio::test]
async fn p1_a_head_from_the_future_does_not_condemn_fresh_packs() {
    let t = setup().await;
    t.clock.set_backend_skew(100 * 3600);
    t.hand_generation(|_, _| {}).await;
    t.clock.set_backend_skew(0);
    let (_, theirs, _) = keep_and_gone(&t).await;
    let r = t.prune().await;
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("ahead of the local clock")),
        "{:?}",
        r.warnings
    );
    let recs = t.records().await;
    for id in &theirs {
        assert_eq!(
            recs[id].status.s,
            status::LIVE,
            "fresh and unreferenced: not condemned yet"
        );
    }
}

/// P5：兩份都解不開的 snapshot。第一次看到 → 以柵欄寫 head 記進 `bad`、停止；一個 grace 之內 → 停止；過了
/// （兩個時鐘，比 P0 的讀數）→ 不再當成起點、回報、P9 刪掉它的主體與 `.r1`。
#[tokio::test]
async fn p5_an_undecryptable_snapshot_is_recorded_waited_on_then_dropped() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    let bad = format!("snapshots/{}/20200101T000000000000000Z", client(1));
    for k in [bad.clone(), format!("{bad}.r1")] {
        t.repo
            .backend()
            .put_if_absent(&k, b"garbage".to_vec())
            .await
            .unwrap();
    }
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("recorded at generation 1")),
        "{err}"
    );
    let h = t.repo.read_head(1).await.unwrap().head;
    assert_eq!(h.bad.len(), 1);
    assert_eq!((h.bad[0].key.as_str(), h.bad[0].first), (bad.as_str(), 1));
    assert_eq!(
        h.shards,
        t.repo.read_head(0).await.unwrap().head.shards,
        "nothing else changed"
    );
    t.clock.advance_hours(71);
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("refuses until")),
        "{err}"
    );
    assert_eq!(t.newest().await, 1);
    let r = t.prune_after(2).await;
    assert_eq!(r.deleted_snapshots, 1);
    assert!(r.warnings.iter().any(|w| w.contains("ignored and deleted")));
    assert!(!t
        .hooked
        .keys("snapshots")
        .await
        .iter()
        .any(|k| k.starts_with(&bad)));
}

/// 壞 snapshot 記進 `bad` 的那一代寫成時後端時鐘慢了 100 小時：只看 `T` 的話一小時後就過了 grace，`w` 擋住。
#[tokio::test]
async fn p5_a_slow_backend_clock_at_the_bad_record_does_not_shorten_the_wait() {
    let t = setup().await;
    let bad = format!("snapshots/{}/20200101T000000000000000Z", client(1));
    for k in [bad.clone(), format!("{bad}.r1")] {
        t.repo
            .backend()
            .put_if_absent(&k, b"garbage".to_vec())
            .await
            .unwrap();
    }
    t.clock.set_backend_skew(-100 * 3600);
    let b = bad.clone();
    t.hand_generation(|_, h| {
        h.bad
            .push(kist_format::v4::index::Bad { key: b, first: h.g });
    })
    .await;
    t.clock.set_backend_skew(0);
    t.clock.advance_hours(1);
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("refuses until")),
        "{err}"
    );
}

/// 同上，寫入者的時鐘慢了 100 小時：`T` 擋住。
#[tokio::test]
async fn p5_a_slow_writer_clock_at_the_bad_record_does_not_shorten_the_wait() {
    let t = setup().await;
    let bad = format!("snapshots/{}/20200101T000000000000000Z", client(1));
    for k in [bad.clone(), format!("{bad}.r1")] {
        t.repo
            .backend()
            .put_if_absent(&k, b"garbage".to_vec())
            .await
            .unwrap();
    }
    let b = bad.clone();
    t.hand_generation(|_, h| {
        h.w -= 100 * 3600 * 1_000_000_000;
        h.bad
            .push(kist_format::v4::index::Bad { key: b, first: h.g });
    })
    .await;
    t.clock.advance_hours(1);
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("refuses until")),
        "{err}"
    );
}

/// 第二輪外部審查 C1–C3 在 P7d：基礎世代由兩個時鐘都快了 45 分鐘的主機寫成（不到 `M`，P1 不調整）。「現在」
/// 只用它的值的話 X 在真實的 71.6 小時就過了 age；與 P0 的讀數取小之後沒有。
#[tokio::test]
async fn p7d_a_base_head_written_with_fast_clocks_does_not_end_the_age_early() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    t.prune().await;
    let c = t
        .hand_generation(|recs, h| {
            for id in &theirs {
                recs.get_mut(id).unwrap().status = Status {
                    s: status::CONDEMNED,
                    at: h.g,
                    age: 72 * HOUR,
                };
            }
        })
        .await;
    t.clock.advance(71 * 3600 + 30 * 60);
    t.clock.set_backend_skew(45 * 60);
    t.hand_generation(|_, h| h.w += 45 * 60 * 1_000_000_000)
        .await;
    t.clock.set_backend_skew(0);
    t.clock.advance(5 * 60);
    let r = t.prune().await;
    assert_eq!(r.tombstoned, 0, "condemned at {c}");
}

/// P4：讀新進者的 trailer 時後端出錯（逾時）→ 這一輪停止，不把好的 pack 記成 unreadable。用沒有被引用的
/// pack：P5 不會換個理由擋下來。
#[tokio::test]
async fn p4_a_read_error_stops_the_prune_instead_of_marking_the_pack_unreadable() {
    let t = setup().await;
    let plain = data("x", 3000);
    let k = t.repo.keys().chunk_id(&plain);
    let (id, _) = t.hand_pack(1, Kind::Data, &[(k.into(), plain)]).await;
    t.hooked.store.fail_get.lock().unwrap().push(
        Key::Pack {
            client: client(1),
            id,
        }
        .path(),
    );
    assert!(t.try_prune().await.is_err());
    assert_eq!(t.newest().await, 0, "nothing written");
    t.prune().await;
    assert_eq!(status_of(&t.records().await, &id), Some(status::LIVE));
}

/// P7g：tombstone 不在列表了、過了 grace，但當下還有 delta 提到它（例如很慢的 backup 晚寫的）→ 記錄留著（讀取端
/// 不會把它當成 delta 裡的新 pack）；同一輪的 P9 收錄、刪掉那份 delta，下一輪才移除記錄。
#[tokio::test]
async fn p7g_a_tombstone_named_by_a_delta_keeps_its_record() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    condemn(&t, &theirs).await;
    t.prune_after(73).await;
    t.prune_after(73).await; // g5：tombstone、刪除
    t.prune_after(73).await; // g6：不在列表了，基礎世代 g5 還不到 grace
    let x = theirs[0];
    let rec = t.records().await[&x].clone();
    assert_eq!(rec.status.s, status::TOMBSTONE);
    let delta = kist_format::v4::index::Delta::new(
        rec.client,
        vec![kist_format::v4::index::DeltaPack {
            id: x,
            kind: rec.kind,
            size: rec.size,
            entries: rec.entries.clone(),
        }],
    )
    .encode()
    .unwrap();
    let sealed = t.repo.keys().seal_delta(&rec.client, &delta).unwrap();
    t.repo
        .backend()
        .put_if_absent(
            &Key::Delta {
                client: rec.client,
                id: ObjectId::of(&sealed),
            }
            .path(),
            sealed,
        )
        .await
        .unwrap();
    t.prune_after(73).await; // g7：過了 grace，但 delta 提到它
    assert!(t.records().await.contains_key(&x));
    assert!(
        t.hooked.keys("deltas").await.is_empty(),
        "the delta is absorbed and deleted"
    );
    t.prune().await; // g8：delta 不在了
    assert!(!t.records().await.contains_key(&x));
}

/// P8：輸掉柵欄（下一代的 `.r1` 在這一輪 P1 之後出現）→ 什麼都不刪。
#[tokio::test]
async fn p8_a_lost_fence_deletes_nothing() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    condemn(&t, &theirs).await;
    t.prune_after(73).await; // g4
    t.clock.advance_hours(73);
    t.hooked.store.after_get.lock().unwrap().push((
        Key::Head {
            g: 4,
            replica: false,
        }
        .path(),
        Key::Head {
            g: 5,
            replica: true,
        }
        .path(),
        b"x".to_vec(),
    ));
    let err = t.try_prune().await.unwrap_err();
    assert!(matches!(err, CoreError::Superseded(_)), "{err}");
    let listed = t.all_packs().await;
    for id in &theirs {
        assert!(listed.contains(id), "{id} would have been deleted");
    }
}

/// P8：寫 head 之前超過整輪時限（grace − M）→ 不寫 head、什麼都不刪。
#[tokio::test]
async fn p8_out_of_time_writes_no_head() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.hooked.store.advance_after_get.lock().unwrap().push((
        Key::Head {
            g: 0,
            replica: false,
        }
        .path(),
        71 * 3600,
    ));
    let err = t.try_prune().await.unwrap_err();
    assert!(
        err.to_string().contains("not writing the next generation"),
        "{err}"
    );
    assert_eq!(t.newest().await, 0);
}

/// P8：head 的主體寫成、`.r1` 寫入失敗 → 什麼都不刪（第二輪外部審查 G1）。
#[tokio::test]
async fn p8_a_failed_head_r1_deletes_nothing() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    condemn(&t, &theirs).await;
    t.prune_after(73).await; // g4
    t.hooked.store.fail.lock().unwrap().push(
        Key::Head {
            g: 5,
            replica: true,
        }
        .path(),
    );
    t.clock.advance_hours(73);
    let err = t.try_prune().await.unwrap_err();
    assert!(err.to_string().contains(".r1 is not"), "{err}");
    let listed = t.all_packs().await;
    for id in &theirs {
        assert!(listed.contains(id));
    }
}

/// P9：寫完 head 之後超過整輪時限 → 停止刪除、回報沒做完；下一輪照常刪。
#[tokio::test]
async fn p9_out_of_time_stops_deleting_and_the_next_prune_finishes() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    condemn(&t, &theirs).await;
    t.prune_after(73).await; // g4
    t.hooked.store.advance_after_put.lock().unwrap().push((
        Key::Head {
            g: 5,
            replica: true,
        }
        .path(),
        71 * 3600,
    ));
    let r = t.prune_after(73).await; // g5
    assert!(r.unfinished.is_some());
    assert_eq!(r.deleted_packs, 0);
    assert!(t.all_packs().await.contains(&theirs[0]));
    let r = t.prune().await;
    assert_eq!(r.deleted_packs, theirs.len() as u64);
}

/// P9 的刪除順序：`.r1`、parity、主體（中途停下時，留下的主體仍是完整的 pack）。
#[tokio::test]
async fn p9_deletes_the_r1_and_parity_before_the_body() {
    let t = setup().await;
    t.write("keep", "k", &data("keep", 50 * 1024));
    t.write("gone", "g", &data("gone", 50 * 1024));
    t.backup(1, "keep").await;
    let before = t.all_packs().await;
    let mut o = opts(1);
    o.clock = t.clock.clock();
    o.parity = 2;
    let gone = t
        .repo
        .backup(&[t.src.path().join("gone")], o)
        .await
        .unwrap()
        .snapshot;
    let mpack = t
        .pack_ids("mpacks")
        .await
        .into_iter()
        .find(|id| !before.contains(id))
        .unwrap();
    t.forget(&gone).await;
    condemn(&t, &[mpack]).await;
    t.prune_after(73).await;
    t.prune_after(73).await;
    let body = Key::Mpack {
        client: client(1),
        id: mpack,
        replica: false,
    }
    .path();
    let r1 = Key::Mpack {
        client: client(1),
        id: mpack,
        replica: true,
    }
    .path();
    let parity = Key::Parity {
        client: client(1),
        id: mpack,
    }
    .path();
    let order: Vec<String> = t
        .hooked
        .store
        .deleted()
        .into_iter()
        .filter(|k| [&body, &r1, &parity].contains(&k))
        .collect();
    assert_eq!(order, vec![r1, parity, body]);
}

/// P9 的清理：解不開的 delta、兩代都沒引用的舊分片、pack 已不在的 parity，都要過了 grace 才刪。分片只有記錄
/// 改變的那幾片會換新：forget 掉唯一的 snapshot，讓它的 pack 標成 condemned。
#[tokio::test]
async fn p9_cleans_up_bad_deltas_old_shards_and_orphan_parity_after_a_grace() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.write("p", "f", &data("p", 50 * 1024));
    let a = t.backup(1, "a").await;
    let mut o = opts(1);
    o.clock = t.clock.clock();
    o.parity = 1;
    t.repo.backup(&[t.src.path().join("p")], o).await.unwrap();
    let live_parity = t.hooked.keys("parity").await;
    assert!(!live_parity.is_empty());
    let young = Key::Shard {
        id: ObjectId::from_bytes([6; 32]),
        replica: false,
    }
    .path();
    t.repo
        .backend()
        .put_if_absent(&young, b"s".to_vec())
        .await
        .unwrap();
    let garbage = b"not a delta".to_vec();
    let bad_delta = Key::Delta {
        client: client(1),
        id: ObjectId::of(&garbage),
    }
    .path();
    t.repo
        .backend()
        .put_if_absent(&bad_delta, garbage)
        .await
        .unwrap();
    let orphan = Key::Parity {
        client: client(1),
        id: ObjectId::from_bytes([5; 32]),
    }
    .path();
    t.repo
        .backend()
        .put_if_absent(&orphan, b"p".to_vec())
        .await
        .unwrap();
    let r = t.prune().await; // g1：寫出分片 S1
    assert!(r
        .warnings
        .iter()
        .any(|w| w.contains("delta cannot be read")));
    let s1 = t.hooked.keys("maint/map").await;
    assert!(!s1.is_empty());
    t.forget(&a).await;
    let r = t.prune_after(73).await; // g2：n_t = T(g1)，都還不到 grace
    assert_eq!(
        (r.deleted_deltas, r.deleted_parity, r.deleted_shards),
        (0, 0, 0)
    );
    assert!(
        t.hooked.keys("maint/map").await.contains(&young),
        "an orphan shard waits a grace"
    );
    let r = t.prune_after(73).await; // g3：condemned，分片換新；delta、parity、孤兒分片過了 grace
    assert_eq!((r.deleted_deltas, r.deleted_parity), (1, 1));
    assert_eq!(
        r.deleted_shards, 1,
        "only the orphan: S1 is still referenced by the base generation g2"
    );
    assert!(!t.hooked.keys("maint/map").await.contains(&young));
    assert!(!t.hooked.keys("deltas").await.contains(&bad_delta));
    assert!(!t.hooked.keys("parity").await.contains(&orphan));
    let r = t.prune_after(73).await; // g4：S1 兩代都沒引用
    assert!(r.deleted_shards >= 1);
    // S1 裡被換掉的那幾片刪了，還被最新的 head 引用的留著
    let shards = t.hooked.keys("maint/map").await;
    let g = t.newest().await;
    let referenced: Vec<ObjectId> = t
        .repo
        .read_head(g)
        .await
        .unwrap()
        .head
        .shards
        .into_iter()
        .flatten()
        .collect();
    let mut replaced = 0;
    for k in &s1 {
        let Ok(Key::Shard { id, .. }) = Key::parse(k) else {
            unreachable!()
        };
        assert_eq!(shards.contains(k), referenced.contains(&id), "{k}");
        replaced += usize::from(!referenced.contains(&id));
    }
    assert!(replaced > 0);
    let parity = t.hooked.keys("parity").await;
    for k in &live_parity {
        assert!(parity.contains(k), "{k}: its pack is still there");
    }
}

/// 重建後的 hold（§11.4）：期間 held 的記錄不復活、不標 tombstone；在兩個時鐘上都過了遺失世代的最晚時間
/// 加一個 grace 才結束，head 拿掉它。
#[tokio::test]
async fn a_hold_blocks_tombstones_and_held_revival_until_it_passes() {
    let t = setup().await;
    let (keep, theirs, _) = keep_and_gone(&t).await;
    t.prune().await; // g1
    let held_live = keep[0];
    t.hand_generation(|recs, h| {
        for id in &theirs {
            recs.get_mut(id).unwrap().status = Status {
                s: status::CONDEMNED,
                at: 1,
                age: 72 * HOUR,
            };
        }
        let r = recs.get_mut(&held_live).unwrap();
        r.status = Status {
            s: status::CONDEMNED,
            at: 1,
            age: 72 * HOUR,
        };
        r.held = true;
        h.hold = Some(kist_format::v4::index::Hold {
            from: 1,
            lost: 1,
            grace: 72 * HOUR,
        });
    })
    .await; // g2
    t.prune_after(73).await; // g3：hold 還在（基礎世代 g2 的時間 = T(1)）
    t.prune_after(73).await; // g4：基礎世代 g3 在 T(1) + 146h：hold 結束這一輪才看得到
    let recs = t.records().await;
    let g = t.newest().await;
    let head = t.repo.read_head(g).await.unwrap().head;
    assert_eq!(head.hold, None, "the hold ended");
    assert_eq!(
        recs[&held_live].status.s,
        status::LIVE,
        "revived once the hold ended"
    );
    assert!(!recs[&held_live].held);
    // hold 期間沒有 tombstone：結束的這一輪才標
    for id in &theirs {
        assert_eq!(recs[id].status.s, status::TOMBSTONE);
    }
}

/// hold 期間：held 的 canonical 記錄不復活、aged 的 condemned 不標 tombstone。
#[tokio::test]
async fn during_a_hold_nothing_is_tombstoned_and_held_records_stay_condemned() {
    let t = setup().await;
    let (keep, theirs, _) = keep_and_gone(&t).await;
    t.prune().await; // g1
    let held_live = keep[0];
    t.clock.advance_hours(100);
    t.hand_generation(|recs, h| {
        for id in &theirs {
            recs.get_mut(id).unwrap().status = Status {
                s: status::CONDEMNED,
                at: 1,
                age: 72 * HOUR,
            };
        }
        let r = recs.get_mut(&held_live).unwrap();
        r.status = Status {
            s: status::CONDEMNED,
            at: 1,
            age: 72 * HOUR,
        };
        r.held = true;
        h.hold = Some(kist_format::v4::index::Hold {
            from: 2,
            lost: 2,
            grace: 72 * HOUR,
        });
    })
    .await; // g2：hold 涵蓋 g2 自己
    let r = t.prune_after(1).await;
    assert_eq!(r.tombstoned, 0);
    assert_eq!(r.revived, 0);
    let recs = t.records().await;
    assert_eq!(recs[&held_live].status.s, status::CONDEMNED);
    assert!(recs[&held_live].held);
    for id in &theirs {
        assert_eq!(recs[id].status.s, status::CONDEMNED);
    }
}

/// hold 的兩個時鐘：condemned 在 g1（時間 0）；遺失的世代 g2 在真實的 50 小時寫成，`skew` 是那時後端時鐘
/// （`T`）或寫入者時鐘（`w`）的偏差。在 81 小時的那一輪 condemned 已過 age（兩個時鐘），hold 只在偏差的那個
/// 時鐘上結束了（50 − 100 + 72 = 22 小時），另一個時鐘要到 122 小時。
async fn hold_with_one_skewed_clock(backend_skew: bool) -> u64 {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    t.prune().await; // g1
    t.clock.advance_hours(50);
    if backend_skew {
        t.clock.set_backend_skew(-100 * 3600);
    }
    t.hand_generation(|_, h| {
        if !backend_skew {
            h.w -= 100 * 3600 * 1_000_000_000;
        }
    })
    .await; // g2：遺失的世代
    t.clock.set_backend_skew(0);
    t.hand_generation(|recs, h| {
        for id in &theirs {
            recs.get_mut(id).unwrap().status = Status {
                s: status::CONDEMNED,
                at: 1,
                age: 72 * HOUR,
            };
        }
        h.hold = Some(kist_format::v4::index::Hold {
            from: 2,
            lost: 2,
            grace: 72 * HOUR,
        });
    })
    .await; // g3
    t.prune_after(30).await; // g4（80 小時）：基礎世代 g3 在 50 小時，還不到 age
    t.prune_after(1).await.tombstoned // g5：基礎世代 g4 在 80 小時
}

#[tokio::test]
async fn a_slow_backend_clock_in_the_lost_generations_does_not_end_the_hold_early() {
    assert_eq!(hold_with_one_skewed_clock(true).await, 0);
}

#[tokio::test]
async fn a_slow_writer_clock_in_the_lost_generations_does_not_end_the_hold_early() {
    assert_eq!(hold_with_one_skewed_clock(false).await, 0);
}

/// P5：被引用的 chunk 只剩 tombstone 持有 → 拒絕（P9 正要刪它）。
#[tokio::test]
async fn p5_a_chunk_held_only_by_a_tombstone_is_refused() {
    let t = setup().await;
    let plain = data("chunk", 3000);
    let k = t.repo.keys().chunk_id(&plain);
    let (x, _) = t
        .hand_pack(1, Kind::Data, &[(k.into(), plain.clone())])
        .await;
    t.prune().await; // g1：x live
    t.hand_generation(|recs, h| {
        recs.get_mut(&x).unwrap().status = Status {
            s: status::TOMBSTONE,
            at: h.g,
            age: 0,
        };
    })
    .await;
    hand_snapshot(
        &t.repo,
        client(3),
        hours_from_now(-1),
        vec![file_root("f", k, plain.len() as u64)],
        vec![],
    )
    .await;
    let err = t.try_prune().await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Unsafe(m) if m.contains("chunk") && m.contains("no readable holder")),
        "{err}"
    );
}

/// 同一個目錄先有小的 a 與大的 b、再只剩 a：forget 第一份之後，a 與 b 同一個資料 pack，當正本的只有 a（約兩成）。
/// 回傳 (那個 pack, 第二份 snapshot)。
async fn sparse_pack(t: &T) -> (ObjectId, String) {
    t.write("d", "a", &data("a", 10 * 1024));
    t.write("d", "b", &data("b", 40 * 1024));
    let first = t.backup(1, "d").await;
    let packs = t.pack_ids("packs").await;
    assert_eq!(packs.len(), 1, "a and b share one data pack");
    std::fs::remove_file(t.src.path().join("d/b")).unwrap();
    let second = t.backup(1, "d").await;
    assert_eq!(t.pack_ids("packs").await, packs, "a is reused, nothing new");
    t.forget(&first).await;
    (packs[0], second)
}

/// 還原 `snapshot` 到暫存目錄，回傳 d 底下的檔案。
async fn restored(t: &T, snapshot: &str) -> BTreeMap<String, Node> {
    let target = tempfile::tempdir().unwrap();
    let r = t
        .repo
        .restore(
            &Key::parse(snapshot).unwrap(),
            target.path(),
            kist_core::RestoreOptions::default(),
        )
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{r:?}");
    let segs = kist_format::v4::snapshot::split_locator(
        t.src.path().join("d").to_str().unwrap().as_bytes(),
    )
    .unwrap();
    let mut dir = target.path().to_path_buf();
    for s in segs {
        dir.push(String::from_utf8(s).unwrap());
    }
    source_tree(&dir)
}

fn only_a() -> BTreeMap<String, Node> {
    [("a".to_owned(), Node::File(data("a", 10 * 1024)))].into()
}

const MAINT: kist_format::v4::ids::ClientId = kist_format::v4::ids::ClientId::MAINT;

/// P7c：live、是正本、比 grace 老、有效比例低的 pack 被 repack：當正本的項目寫進全零 client 前綴下的新 pack
/// （by_prune、讀回驗證過），舊的 condemned、保留全部項目，之後照常刪除；snapshot 一路還原得出來。
#[tokio::test]
async fn p7c_a_sparse_old_pack_is_repacked_and_the_snapshot_still_restores() {
    let t = setup().await;
    let (p, second) = sparse_pack(&t).await;
    t.prune().await; // g1
    let entries = t.records().await[&p].entries.len();
    let r = t.prune_after(73).await; // g2：還不夠老
    assert_eq!(r.repacked, 0);
    let r = t.prune_after(73).await; // g3：repack
    assert_eq!(r.repacked, 1);
    let recs = t.records().await;
    let old = &recs[&p];
    assert_eq!((old.status.s, old.status.at), (status::CONDEMNED, 3));
    assert_eq!(old.entries.len(), entries, "the old pack keeps every entry");
    let new: Vec<&Record> = recs.values().filter(|r| r.by_prune).collect();
    assert_eq!(new.len(), 1);
    let n = new[0];
    assert_eq!((n.client, n.status.s, n.since), (MAINT, status::LIVE, 3));
    assert!(n.entries.len() < entries);
    assert!(t.hooked.keys(&format!("packs/{MAINT}")).await.contains(
        &Key::Pack {
            client: MAINT,
            id: n.id
        }
        .path()
    ));
    assert_eq!(restored(&t, &second).await, only_a());
    t.prune_after(73).await;
    t.prune_after(73).await; // 舊的 tombstone、刪除
    assert!(!t.pack_ids("packs").await.contains(&p));
    assert_eq!(
        restored(&t, &second).await,
        only_a(),
        "read from the new pack"
    );
}

/// P7c 的比例：當正本的項目不低於 `repack_below` 就不 repack；0 不依比例 repack。
#[tokio::test]
async fn p7c_a_dense_pack_or_repack_below_zero_is_not_repacked() {
    let t = setup().await;
    let (p, _) = sparse_pack(&t).await;
    t.prune().await;
    t.prune_after(73).await;
    t.clock.advance_hours(73);
    let r = t.prune_with(0).await.unwrap();
    assert_eq!(r.repacked, 0, "0 disables ratio repacks");
    let r = t.prune_with(15).await.unwrap();
    assert_eq!(r.repacked, 0, "about a fifth is live, not under 15%");
    let r = t.prune_with(50).await.unwrap();
    assert_eq!(r.repacked, 1);
    assert_eq!(t.records().await[&p].status.s, status::CONDEMNED);
}

/// P7c：新 pack 讀回驗證失敗 → 新的記為 unreadable、舊的維持 live、回報。
#[tokio::test]
async fn p7c_a_new_pack_that_does_not_read_back_leaves_the_old_one_live() {
    let t = setup().await;
    let (p, second) = sparse_pack(&t).await;
    t.prune().await;
    t.prune_after(73).await;
    *t.hooked.store.corrupt_puts_under.lock().unwrap() = Some(format!("packs/{MAINT}"));
    let r = t.prune_after(73).await;
    assert_eq!(r.repacked, 0);
    assert!(
        r.warnings.iter().any(|w| w.contains("does not read back")),
        "{:?}",
        r.warnings
    );
    let recs = t.records().await;
    assert_eq!(recs[&p].status.s, status::LIVE);
    let bad: Vec<&Record> = recs.values().filter(|r| r.client == MAINT).collect();
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].status.s, status::UNREADABLE);
    assert!(!bad[0].by_prune);
    *t.hooked.store.corrupt_puts_under.lock().unwrap() = None;
    assert_eq!(restored(&t, &second).await, only_a());
}

/// hold 期間不 repack（§11.4）。
#[tokio::test]
async fn p7c_no_repack_during_a_hold() {
    let t = setup().await;
    sparse_pack(&t).await;
    t.prune().await;
    t.prune_after(73).await;
    t.clock.advance_hours(73);
    t.hand_generation(|_, h| {
        h.hold = Some(kist_format::v4::index::Hold {
            from: h.g,
            lost: h.g,
            grace: 72 * HOUR,
        });
    })
    .await;
    let r = t.prune().await;
    assert_eq!(r.repacked, 0);
}

/// 規格錯誤第 17 項的修法 D：唯一持有 metadata item 的 mpack 位元損壞，有 parity → P5 在記憶體裡修復、照樣走訪；
/// P7c 把它當正本的項目寫進 by_prune 的新 mpack（不論新舊、比例），損壞的那個 condemned、之後照常刪除。沒有 D
/// 的話這一項從記錄移除、沒有持有者，prune 一直拒絕到有人 forget。
#[tokio::test]
async fn d_a_damaged_sole_metadata_holder_with_parity_is_repaired_and_repacked() {
    let t = setup_with(0).await;
    t.write("d", "a", &data("a", 10 * 1024));
    let mut o = opts(1);
    o.clock = t.clock.clock();
    o.parity = 2;
    let snap = t
        .repo
        .backup(&[t.src.path().join("d")], o)
        .await
        .unwrap()
        .snapshot;
    t.prune().await; // g1
    let mpacks = t.pack_ids("mpacks").await;
    assert_eq!(mpacks.len(), 1);
    let m = mpacks[0];
    let rec = t.records().await[&m].clone();
    damage(
        &t,
        &Key::Mpack {
            client: client(1),
            id: m,
            replica: false,
        },
        &rec.entries[0],
    )
    .await;
    let r = t.prune().await; // g2
    assert_eq!((r.repaired, r.repacked, r.false_entries), (1, 1, 0));
    let recs = t.records().await;
    assert_eq!(recs[&m].status.s, status::CONDEMNED);
    let n: Vec<&Record> = recs.values().filter(|r| r.by_prune).collect();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].kind, Kind::Meta);
    assert_eq!(restored(&t, &snap).await, only_a());
    t.prune_after(73).await;
    t.prune_after(73).await;
    assert!(
        !t.pack_ids("mpacks").await.contains(&m),
        "the damaged mpack is deleted"
    );
    assert_eq!(restored(&t, &snap).await, only_a());
}

/// 修法 D 在 P7d：正本 C 的副本位元損壞、有 parity → 不移除；這一輪不憑修好的副本把 condemned 的 X 標成
/// tombstone，C 由 repack 寫進 by_prune 的新 pack；下一輪以那一份驗證，X 才 tombstone。
#[tokio::test]
async fn d_a_damaged_canonical_copy_is_repaired_and_the_condemned_holder_waits_a_round() {
    let t = setup().await;
    let plain = data("chunk", 3000);
    let k = t.repo.keys().chunk_id(&plain);
    let (a, ea) = t
        .hand_pack_with_parity(1, Kind::Data, &[(k.into(), plain.clone())], 2)
        .await;
    let (b, eb) = t
        .hand_pack_with_parity(2, Kind::Data, &[(k.into(), plain.clone())], 2)
        .await;
    hand_snapshot(
        &t.repo,
        client(3),
        hours_from_now(-1),
        vec![file_root("f", k, plain.len() as u64)],
        vec![],
    )
    .await;
    let (c, cc, ce, x) = if a < b {
        (a, 1, ea[0], b)
    } else {
        (b, 2, eb[0], a)
    };
    condemn(&t, &[x]).await;
    t.prune_after(73).await; // g4
    damage(
        &t,
        &Key::Pack {
            client: client(cc),
            id: c,
        },
        &ce,
    )
    .await;
    let r = t.prune_after(73).await; // g5
    assert_eq!(
        (r.repaired, r.repacked, r.false_entries, r.tombstoned),
        (1, 1, 0, 0)
    );
    let recs = t.records().await;
    assert_eq!(
        recs[&x].status.s,
        status::CONDEMNED,
        "X waits for the new copy"
    );
    assert_eq!(recs[&c].status.s, status::CONDEMNED, "C is repacked");
    assert!(
        recs[&c].entries.iter().any(|e| e.i == k.into()),
        "not removed"
    );
    let n: Vec<&Record> = recs.values().filter(|r| r.by_prune).collect();
    assert_eq!(n.len(), 1);
    assert!(n[0].entries.iter().any(|e| e.i == k.into()));
    let r = t.prune().await; // g6：正本是 by_prune 的新 pack
    assert!(r.tombstoned >= 1);
    assert_eq!(t.records().await[&x].status.s, status::TOMBSTONE);
}

/// 移除版本不認得的 pack（負責人決定 10）：以柵欄寫一代把 unknown 改成 condemned，之後由 prune 照兩階段刪除。
#[tokio::test]
async fn condemned_unknown_packs_are_deleted_by_later_prunes() {
    let t = setup().await;
    let id = t.odd_pack(1, Some(5)).await;
    t.prune().await; // g1：unknown
    let r = t.repo.condemn_unknown(t.clock.maint()).await.unwrap();
    assert_eq!(r.generation, Some(2));
    assert_eq!(
        r.packs,
        vec![Key::Pack {
            client: client(1),
            id
        }
        .path()]
    );
    let rec = t.records().await[&id].clone();
    assert_eq!(
        (rec.status.s, rec.status.at, rec.status.age),
        (status::CONDEMNED, 2, 72 * 3600)
    );
    t.prune_after(73).await; // g3：基礎世代 g2 還不到 age
    assert!(t.pack_ids("packs").await.contains(&id));
    let p = t.prune_after(73).await; // g4
    assert_eq!(p.deleted_packs, 1);
    assert!(!t.pack_ids("packs").await.contains(&id));
}

#[tokio::test]
async fn without_unknown_packs_nothing_is_written() {
    let t = setup().await;
    t.write("a", "f", &data("a", 10 * 1024));
    t.backup(1, "a").await;
    t.prune().await;
    let r = t.repo.condemn_unknown(t.clock.maint()).await.unwrap();
    assert_eq!(r.generation, None);
    assert!(r.packs.is_empty());
    assert_eq!(t.newest().await, 1);
}

/// 較新的 kist 寫的 snapshot 還在時，prune 在 P5 就停住：condemned 的 unknown pack 不會被刪。
#[tokio::test]
async fn a_newer_snapshot_keeps_condemned_unknown_packs_from_being_deleted() {
    let t = setup().await;
    let id = t.odd_pack(1, Some(5)).await;
    t.prune().await;
    t.repo.condemn_unknown(t.clock.maint()).await.unwrap();
    #[derive(serde::Serialize)]
    struct Snap {
        v: u32,
        roots: MetaId,
        time: i64,
        host: String,
        client: kist_format::v4::ids::ClientId,
    }
    let at = hours_from_now(-1);
    let ts = SnapTs::from_nanos(at).unwrap();
    let plain = kist_format::cbor::encode(&Snap {
        v: 5,
        roots: MetaId::from_bytes([1; 32]),
        time: at,
        host: "future".into(),
        client: client(2),
    })
    .unwrap();
    let sealed = t
        .repo
        .keys()
        .seal_snapshot(&client(2), &ts, &plain)
        .unwrap();
    t.repo
        .backend()
        .put_if_absent(
            &Key::Snapshot {
                client: client(2),
                ts,
                replica: false,
            }
            .path(),
            sealed,
        )
        .await
        .unwrap();
    for _ in 0..3 {
        t.clock.advance_hours(73);
        assert!(t.try_prune().await.is_err());
    }
    assert!(t.pack_ids("packs").await.contains(&id));
}
