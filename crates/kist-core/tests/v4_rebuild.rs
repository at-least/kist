//! v4 的重建（format-v4.md §11.4）：沿用 `Map(g_r)` 或把每個 pack 記成 held 的 condemned、hold 與合併、沿用的
//! 條件只看 `w`、柵欄、P1 的補寫。時間走虛擬時鐘。

mod v4_common;

use std::collections::{BTreeMap, HashSet};

use kist_core::v4::{MaintOptions, RebuildReport};
use kist_core::CoreError;
use kist_format::v4::ids::ObjectId;
use kist_format::v4::index::{status, Drain, Hold, Record, Status};
use kist_format::v4::keys::{Gen, Key};
use v4_common::harness::*;
use v4_common::*;

const HOUR: u64 = 3600;

async fn rebuild(t: &T) -> RebuildReport {
    try_rebuild(t).await.unwrap()
}

async fn try_rebuild(t: &T) -> kist_core::Result<RebuildReport> {
    t.repo
        .rebuild(MaintOptions {
            clock: t.clock.clock(),
        })
        .await
}

/// 刪掉 head `g` 引用、而更舊的 head 都沒引用的分片（主體與 `.r1`）：那一代的 map 讀不全。
async fn lose_map(t: &T, g: Gen) {
    let mut older = HashSet::new();
    for o in 0..g {
        older.extend(
            t.repo
                .read_head(o)
                .await
                .unwrap()
                .head
                .shards
                .into_iter()
                .flatten(),
        );
    }
    let mut lost = 0;
    for id in t
        .repo
        .read_head(g)
        .await
        .unwrap()
        .head
        .shards
        .into_iter()
        .flatten()
    {
        if older.contains(&id) {
            continue;
        }
        for replica in [false, true] {
            t.repo
                .backend()
                .delete(&Key::Shard { id, replica }.path())
                .await
                .unwrap();
        }
        lost += 1;
    }
    assert!(lost > 0, "generation {g} has no shard of its own");
}

/// 刪掉全部的分片：只有 head 0（沒有分片）讀得全。
async fn lose_all_maps(t: &T) {
    for k in t.hooked.keys("maint/map").await {
        t.repo.backend().delete(&k).await.unwrap();
    }
}

async fn map_of(t: &T, g: Gen) -> BTreeMap<ObjectId, Record> {
    let gen = t.repo.read_head(g).await.unwrap();
    t.repo
        .load_map(&gen)
        .await
        .unwrap()
        .records()
        .iter()
        .map(|r| (r.id, r.clone()))
        .collect()
}

#[tokio::test]
async fn an_intact_map_needs_no_rebuild() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await;
    let r = rebuild(&t).await;
    assert_eq!(r.generation, None);
    assert_eq!(t.newest().await, 1);
}

/// 遺失的只有最新的一代（`w` 的跨度 0）→ 沿用 `Map(g_r)`，condemned 的標成 held，新進者照 P4 收錄；head 記下
/// hold。之後的讀取端與 prune 都讀得了新的一代。
#[tokio::test]
async fn a_short_loss_keeps_the_last_readable_map() {
    let t = setup().await;
    let (keep, theirs, _) = keep_and_gone(&t).await;
    condemn(&t, &theirs).await; // g3：theirs condemned
    t.write("b", "f", &data("b", 50 * 1024));
    let b = t.backup(1, "b").await;
    let fresh: Vec<ObjectId> = t
        .all_packs()
        .await
        .into_iter()
        .filter(|id| !keep.contains(id) && !theirs.contains(id))
        .collect();
    t.prune().await; // g4：收錄 b 的 pack，寫出新的分片
    lose_map(&t, 4).await;
    let r = rebuild(&t).await;
    assert_eq!(
        (r.generation, r.lost, r.readable, r.kept_map),
        (Some(5), 4, 3, true)
    );
    let head = t.repo.read_head(5).await.unwrap().head;
    assert_eq!(
        head.hold,
        Some(Hold {
            from: 4,
            lost: 4,
            grace: 72 * HOUR
        })
    );
    let map = map_of(&t, 5).await;
    for id in &theirs {
        assert_eq!(map[id].status.s, status::CONDEMNED);
        assert!(map[id].held, "condemned records of Map(g_r) are held");
    }
    for id in &keep {
        assert_eq!(map[id].status.s, status::LIVE);
        assert!(!map[id].held);
    }
    for id in &fresh {
        assert_eq!((map[id].status.s, map[id].since), (status::LIVE, 5));
    }
    assert!(
        !read_tree(&t.repo, &b).await.is_empty(),
        "readers load generation 5"
    );
    t.prune().await;
}

/// 遺失的世代在 `w` 上跨過一個 age → 每個列出來的 pack 記成 held 的 condemned（`at = r`）；hold 期間不復活、
/// 不標 tombstone；hold 在兩個時鐘上都過了之後，被引用的復活、沒被引用的照常走下去。資料一路讀得到。
#[tokio::test]
async fn a_long_loss_holds_every_pack_condemned_until_the_hold_passes() {
    let t = setup().await;
    let (keep, theirs, _) = keep_and_gone(&t).await;
    let a = {
        t.write("c", "f", &data("c", 50 * 1024));
        t.backup(1, "c").await
    };
    t.prune().await; // g1
    t.prune_after(73).await; // g2
    t.prune_after(73).await; // g3：theirs condemned
    assert!(!theirs.is_empty());
    lose_all_maps(&t).await;
    let r = rebuild(&t).await;
    assert_eq!(
        (r.generation, r.lost, r.readable, r.kept_map),
        (Some(4), 3, 0, false)
    );
    let head = t.repo.read_head(4).await.unwrap().head;
    assert_eq!(
        head.hold,
        Some(Hold {
            from: 1,
            lost: 3,
            grace: 72 * HOUR
        })
    );
    let map = map_of(&t, 4).await;
    for id in t.all_packs().await {
        let rec = &map[&id];
        assert_eq!(
            (rec.status.s, rec.status.at, rec.status.age, rec.held),
            (status::CONDEMNED, 4, 72 * HOUR, true),
            "{id}"
        );
    }
    assert!(!read_tree(&t.repo, &a).await.is_empty());
    // hold 期間：不復活、不標 tombstone
    let p = t.prune().await;
    assert_eq!((p.revived, p.tombstoned), (0, 0));
    for id in &keep {
        assert_eq!(status_of(&t.records().await, id), Some(status::CONDEMNED));
    }
    // hold 的時間窗：g1..g3 裡最晚的 T 與 w（g3，146 小時）加 72 小時
    t.prune_after(73).await;
    let p = t.prune_after(1).await;
    assert!(p.revived > 0);
    let recs = t.records().await;
    for id in &keep {
        assert_eq!(recs[id].status.s, status::LIVE, "referenced: revived");
        assert!(!recs[id].held);
    }
    assert_eq!(
        t.repo.read_head(t.newest().await).await.unwrap().head.hold,
        None
    );
    assert!(!read_tree(&t.repo, &a).await.is_empty());
}

/// 沿用的條件只看 `w`（第二輪外部審查 D1）：遺失的兩代在後端時鐘上幾乎同時寫成（`T` 的跨度 0），在 `w` 上相隔
/// 73 小時 → 不沿用。
#[tokio::test]
async fn the_keep_rule_looks_at_w_only() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await; // g1，時間 0
    t.clock.advance_hours(73);
    t.clock.set_backend_skew(-73 * 3600);
    t.hand_generation(|_, _| {}).await; // g2：T 是 0、w 是 73 小時
    t.clock.set_backend_skew(0);
    lose_map(&t, 2).await;
    lose_map(&t, 1).await;
    let r = rebuild(&t).await;
    assert_eq!((r.readable, r.kept_map), (0, false));
}

/// `Map(g_r)` 的 tombstone 在不沿用時照樣保留（那個 pack 可能正被刪）；版本不認得與讀不了的 pack 照 P4 記錄，
/// 不記成 condemned。
#[tokio::test]
async fn a_long_loss_keeps_tombstones_and_records_unknown_and_unreadable_packs() {
    let t = setup().await;
    let (_, theirs, _) = keep_and_gone(&t).await;
    t.prune().await; // g1
    let x = theirs[0];
    t.hand_generation(|recs, h| {
        recs.get_mut(&x).unwrap().status = Status {
            s: status::TOMBSTONE,
            at: h.g,
            age: 0,
        };
    })
    .await; // g2：g_r
    let unknown = t.odd_pack(1, Some(5)).await;
    let unreadable = t.odd_pack(1, None).await;
    t.clock.advance_hours(73);
    t.hand_generation(|_, _| {}).await; // g3
    t.clock.advance_hours(73);
    t.hand_generation(|_, _| {}).await; // g4
    lose_map(&t, 4).await;
    lose_map(&t, 3).await;
    let r = rebuild(&t).await;
    assert_eq!((r.readable, r.kept_map), (2, false));
    let map = map_of(&t, 5).await;
    assert_eq!(map[&x].status.s, status::TOMBSTONE);
    assert_eq!(map[&unknown].status.s, status::UNKNOWN);
    assert_eq!(map[&unreadable].status.s, status::UNREADABLE);
    assert!(r
        .warnings
        .iter()
        .any(|w| w.contains("unknown trailer version")));
}

/// 已有 hold 時合併：`from` 取小、`lost` 與 `grace` 取大。
#[tokio::test]
async fn an_existing_hold_is_merged() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await; // g1
    t.hand_generation(|_, h| {
        h.hold = Some(Hold {
            from: 1,
            lost: 1,
            grace: 100 * HOUR,
        });
    })
    .await; // g2
    t.hand_generation(|_, _| {}).await; // g3
    lose_map(&t, 3).await;
    let r = rebuild(&t).await;
    assert_eq!(r.readable, 2);
    assert_eq!(
        t.repo.read_head(4).await.unwrap().head.hold,
        Some(Hold {
            from: 1,
            lost: 3,
            grace: 100 * HOUR
        })
    );
}

/// 不沿用時新的 condemned 的 age：drain 還沒結束就是舊的 grace。
#[tokio::test]
async fn a_long_loss_during_a_drain_ages_by_the_old_grace() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await; // g1
    t.hand_generation(|_, h| {
        h.settings.grace = 10 * HOUR;
        h.drain = Some(Drain {
            from: h.g,
            old_grace: 100 * HOUR,
        });
    })
    .await; // g2：調低
    t.clock.advance_hours(11);
    t.hand_generation(|_, _| {}).await; // g3：w 跨度 11 小時 ≥ 最小的 grace 10 小時
    lose_map(&t, 3).await;
    lose_map(&t, 2).await;
    let r = rebuild(&t).await;
    assert!(!r.kept_map);
    let map = map_of(&t, 4).await;
    for rec in map.values() {
        assert_eq!(
            rec.status.age,
            100 * HOUR,
            "the old grace while the drain runs"
        );
    }
}

/// 範圍內的 head 讀不了 → 拒絕（不在世代之間有空洞的狀態下猜）。
#[tokio::test]
async fn an_unreadable_head_in_the_range_is_refused() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await;
    t.prune().await;
    lose_all_maps(&t).await;
    for replica in [false, true] {
        t.repo
            .backend()
            .put(&Key::Head { g: 1, replica }.path(), b"garbage".to_vec())
            .await
            .unwrap();
    }
    let err = try_rebuild(&t).await.unwrap_err();
    assert!(matches!(err, CoreError::Generation(_)), "{err}");
    assert_eq!(t.newest().await, 2);
}

/// 柵欄：下一代的 `.r1` 在這一輪 P1 之後出現 → 輸了，不寫。
#[tokio::test]
async fn a_lost_fence_writes_nothing() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await;
    lose_map(&t, 1).await;
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
    let err = try_rebuild(&t).await.unwrap_err();
    assert!(matches!(err, CoreError::Superseded(_)), "{err}");
    assert!(!t
        .repo
        .backend()
        .exists(
            &Key::Head {
                g: 2,
                replica: false
            }
            .path()
        )
        .await
        .unwrap());
}

/// P1：最大世代只有主體 → 先補寫 `.r1` 再寫重建的那一代。
#[tokio::test]
async fn the_lone_newest_head_gets_its_r1_first() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await;
    t.repo
        .backend()
        .delete(
            &Key::Head {
                g: 1,
                replica: true,
            }
            .path(),
        )
        .await
        .unwrap();
    lose_map(&t, 1).await;
    rebuild(&t).await;
    assert!(t
        .repo
        .backend()
        .exists(
            &Key::Head {
                g: 1,
                replica: true
            }
            .path()
        )
        .await
        .unwrap());
    assert_eq!(t.newest().await, 2);
}

/// 寫 head 之前超過整輪時限（grace − M）→ 不寫。
#[tokio::test]
async fn out_of_time_writes_nothing() {
    let t = setup().await;
    t.write("a", "f", &data("a", 50 * 1024));
    t.backup(1, "a").await;
    t.prune().await;
    lose_all_maps(&t).await;
    t.hooked.store.advance_after_get.lock().unwrap().push((
        Key::Head {
            g: 1,
            replica: false,
        }
        .path(),
        71 * 3600,
    ));
    let err = try_rebuild(&t).await.unwrap_err();
    assert!(
        err.to_string().contains("not writing generation 2"),
        "{err}"
    );
    assert_eq!(t.newest().await, 1);
}
