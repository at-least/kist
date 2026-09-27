//! prune：兩階段 GC（標記 → grace → 刪）、repack、活躍 client 規則、拒絕不完整的引用。
//! 時間全部注入：物件與標記的「修改時間」是真實時鐘 R，snapshot 與 prune 的「現在」是 R 之後幾天。

mod common;

use std::collections::HashSet;
use std::path::Path;

use common::*;
use kist_core::{BackupOptions, CheckOptions, CoreError, PruneOptions, Repository};
use kist_format::{keys, ObjectId};
use time::{Duration, OffsetDateTime};

const H: std::time::Duration = std::time::Duration::from_secs(3600);

/// 後端的修改時間是整秒：標記不能跟它標的物件同一秒（否則 prune 會當作「標記後被重寫過」而不刪）。
/// 真實世界 grace 是幾天，不會發生；測試裡物件剛寫出就標記，要先等過這一秒。
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
}

fn client(id: u8, now: OffsetDateTime) -> BackupOptions {
    BackupOptions {
        client_id: [id; 16],
        hostname: format!("host{id}"),
        username: "tester".to_owned(),
        now: Some(now),
        gc_grace: 72 * H,
        parity: 0,
        progress: None,
        source: kist_core::SourceSpec::default(),
    }
}

fn prune_opts(now: OffsetDateTime) -> PruneOptions {
    PruneOptions {
        // 測試用合成時鐘追趕真實 mtime：沒有時鐘差可言，skew 歸零。
        clock_skew: std::time::Duration::ZERO,
        grace: 72 * H,
        inactive_after: 30 * 24 * H,
        repack_below_percent: 50,
        dry_run: false,
        now: Some(now),
    }
}

fn ids_under(t: &TestRepo, prefix: &str) -> HashSet<ObjectId> {
    let dir = t.repo_path().join(prefix);
    if !dir.is_dir() {
        return HashSet::new();
    }
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| keys::object_id_from_key(&e.unwrap().file_name().to_string_lossy()).unwrap())
        .collect()
}

/// 來源：幾個小檔 + 一個排在最後、佔好幾個 pack 的大檔（之後刪掉它就有整包死掉的 pack）。
fn make_src(root: &Path) {
    make_source(root);
    std::fs::write(root.join("zz-big.bin"), random_bytes(77, 1200 * 1024)).unwrap();
}

async fn restore_matches(repo: &Repository, key: &str, src: &Path, out: &Path) {
    let _ = std::fs::remove_dir_all(out);
    let r = repo.restore(key, out, Default::default()).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_same_tree(src, &out.join(src.strip_prefix("/").unwrap()));
}

/// 完整時間線：b1(大檔) → 刪大檔 b2 → forget b1 → prune 標記 + repack → b3 → prune 刪除。
#[tokio::test]
async fn mark_then_delete_after_grace_when_active_clients_moved_on() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    let out = t.dir.path().join("out");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();

    let b1 = repo
        .backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    std::fs::remove_file(src.join("zz-big.bin")).unwrap();
    let b2 = repo
        .backup(
            std::slice::from_ref(&src),
            client(1, r + Duration::hours(1)),
        )
        .await
        .unwrap();
    let packs_before = ids_under(&t, "packs");
    let trees_before = ids_under(&t, "trees");
    repo.forget(kist_core::ForgetOptions {
        snapshots: vec![b1.snapshot_key.clone()],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();

    // 第一次 prune（物件已經比 grace 老）：只標記、不刪；部分死掉的 pack 被 repack
    settle().await;
    settle().await;
    let p1 = repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    assert!(p1.marked >= 3, "{p1:?}");
    assert_eq!(p1.deleted, 0, "{p1:?}");
    assert!(p1.repacked_packs >= 1, "{p1:?}");
    let marked = ids_under(&t, "gc");
    assert!(marked.len() as u64 >= p1.marked);
    assert!(
        packs_before.is_subset(&ids_under(&t, "packs")),
        "第一階段不能刪 pack"
    );
    assert!(
        trees_before.is_subset(&ids_under(&t, "trees")),
        "第一階段不能刪 tree"
    );
    // b2 的根 tree 活著、不能被標記
    for root in &b2.roots {
        assert!(!marked.contains(&ObjectId::from_bytes(*root.tree.as_bytes())));
    }
    // repo 一致（index 已重寫，不含被 repack 的 pack）
    let report = t
        .open()
        .await
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    restore_matches(&t.open().await, &b2.snapshot_key, &src, &out).await;

    // 標記後 client 1 又備份了一次（內容同 b2）：不寫新資料，也不會碰被標記的 pack
    let b3 = repo
        .backup(std::slice::from_ref(&src), client(1, r + Duration::days(5)))
        .await
        .unwrap();
    assert_eq!(b3.report.chunks_new, 0, "{:?}", b3.report);

    // 第二次 prune：標記已超過 grace，且唯一的活躍 client 在標記後有新 snapshot → 刪
    settle().await;
    settle().await;
    let p2 = repo.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    assert!(p2.deleted >= 3, "{p2:?}");
    assert_eq!(p2.blocked, 0, "{p2:?}");
    let packs_after = ids_under(&t, "packs");
    for id in &marked {
        assert!(!packs_after.contains(id), "被標記的 pack {id} 還在");
        assert!(
            !ids_under(&t, "trees").contains(id),
            "被標記的 tree {id} 還在"
        );
    }
    for id in &marked {
        assert!(
            !ids_under(&t, "gc").contains(id),
            "刪掉的物件 {id} 的標記也要清掉"
        );
    }
    // 這一輪會新標記在 p1 變成垃圾的東西（被 repack 的舊 pack、被取代的舊 index blob）
    assert!(p2.marked > 0, "{p2:?}");
    let fresh = t.open().await;
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // （被 repack 的舊 pack 這時還是孤兒、剛被標記，check 會警告它；最後收斂時再驗沒有警告）
    restore_matches(&fresh, &b2.snapshot_key, &src, &out).await;
    restore_matches(&fresh, &b3.snapshot_key, &src, &out).await;

    // 之後每 4 天跑一次：被 repack 的舊 pack、被取代的舊 index blob 都會被清掉，最後什麼都不剩；
    // 每一步 repo 都要一致
    let mut quiet = 0;
    for day in [12, 16, 20, 24, 28] {
        settle().await;
        let p = repo
            .prune(prune_opts(r + Duration::days(day)))
            .await
            .unwrap();
        assert!(p.skipped.is_empty(), "{p:?}");
        let report = t
            .open()
            .await
            .check(CheckOptions {
                read_data: true,
                repair: false,
            })
            .await
            .unwrap();
        assert!(report.errors.is_empty(), "day {day}: {:?}", report.errors);
        if p.marked + p.deleted + p.repacked_packs + p.revived + p.stale_marks == 0 {
            quiet += 1;
        }
    }
    assert!(quiet >= 2, "GC 沒有收斂");
    assert!(ids_under(&t, "gc").is_empty());
    let report = t
        .open()
        .await
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    restore_matches(&t.open().await, &b3.snapshot_key, &src, &out).await;
}

#[tokio::test]
async fn young_objects_are_not_marked_and_dry_run_changes_nothing() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    let b1 = repo
        .backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    repo.forget(kist_core::ForgetOptions {
        snapshots: vec![b1.snapshot_key],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();
    // 剛寫的物件（可能是進行中的 backup）：不標
    settle().await;
    let p = repo
        .prune(prune_opts(r + Duration::hours(1)))
        .await
        .unwrap();
    assert_eq!(p.marked, 0, "{p:?}");
    assert!(ids_under(&t, "gc").is_empty());

    let mut opts = prune_opts(r + Duration::days(4));
    opts.dry_run = true;
    settle().await;
    settle().await;
    let p = repo.prune(opts).await.unwrap();
    assert!(p.marked > 0, "{p:?}");
    assert!(ids_under(&t, "gc").is_empty(), "dry-run 不能寫標記");
    assert_eq!(t.count("indexes"), 1, "dry-run 不能重寫 index");
}

#[tokio::test]
async fn refuses_to_prune_when_references_are_incomplete() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    let b1 = repo
        .backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    // 弄壞根 tree：引用不完整，prune 必須拒絕，什麼都不寫
    let path = t.repo_path().join(keys::tree(&b1.roots[0].tree));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[40] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    let err = repo
        .prune(prune_opts(r + Duration::days(4)))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Unsafe(_)), "{err}");
    assert!(ids_under(&t, "gc").is_empty());
    assert_eq!(t.count("indexes"), 1);
}

/// 開始得比標記早的 backup 在標記後 commit：它引用到被標記的 pack，第二次 prune 要撤銷標記。
#[tokio::test]
async fn marked_objects_referenced_by_a_new_snapshot_are_revived() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    let b1 = repo
        .backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    // client 2 開始備份同樣的內容（沿用 b1 的 chunk），還沒 commit
    let prepared = t
        .open()
        .await
        .backup_prepare(std::slice::from_ref(&src), client(2, r + Duration::days(4)))
        .await
        .unwrap();
    // b1 被 forget，prune 把 b1 的東西全標起來
    repo.forget(kist_core::ForgetOptions {
        snapshots: vec![b1.snapshot_key],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();
    settle().await;
    settle().await;
    let p1 = repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    assert!(p1.marked > 0, "{p1:?}");
    let marked = ids_under(&t, "gc");
    // client 2 commit（標記很年輕，允許）
    let b2 = prepared.commit().await.unwrap();
    // 第二次 prune：那些 pack / tree 又被引用了 → 撤銷標記，不刪
    settle().await;
    settle().await;
    let p2 = repo.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    assert!(p2.revived > 0, "{p2:?}");
    assert_eq!(p2.deleted, 0, "{p2:?}");
    assert!(ids_under(&t, "gc").is_empty(), "全部復活");
    for id in &marked {
        assert!(
            ids_under(&t, "packs").contains(id) || ids_under(&t, "trees").contains(id),
            "{id} 不見了"
        );
    }
    let fresh = t.open().await;
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let out = t.dir.path().join("out");
    restore_matches(&fresh, &b2.snapshot_key, &src, &out).await;
}

#[tokio::test]
async fn active_client_without_a_newer_snapshot_blocks_deletion_but_inactive_does_not() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    let b1 = repo
        .backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    // client 2 也有一個 snapshot（另一組路徑，內容小），之後不再備份
    let other = t.dir.path().join("other");
    make_source(&other);
    repo.backup(std::slice::from_ref(&other), client(2, r))
        .await
        .unwrap();
    repo.forget(kist_core::ForgetOptions {
        snapshots: vec![b1.snapshot_key],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();
    settle().await;
    repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    let marked = ids_under(&t, "gc");
    assert!(!marked.is_empty());
    // client 1 在標記後又備份了（不含大檔，所以不會復活那些垃圾）；client 2 沒有 → client 2（活躍）擋住刪除
    std::fs::remove_file(src.join("zz-big.bin")).unwrap();
    repo.backup(std::slice::from_ref(&src), client(1, r + Duration::days(5)))
        .await
        .unwrap();
    settle().await;
    settle().await;
    let p = repo.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    assert_eq!(p.deleted, 0, "{p:?}");
    assert!(p.blocked > 0, "{p:?}");
    // （client 1 重備份時內容相同的子目錄 tree 會被重新引用而復活，所以不能要求標記原封不動）
    assert!(!ids_under(&t, "gc").is_empty(), "{p:?}");
    // 30 天後 client 2 變成 inactive：不再阻擋
    settle().await;
    let p = repo
        .prune(prune_opts(r + Duration::days(40)))
        .await
        .unwrap();
    assert!(p.deleted > 0, "{p:?}");
    assert_eq!(p.blocked, 0, "{p:?}");
    let report = t
        .open()
        .await
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[tokio::test]
async fn stale_marker_for_a_missing_object_is_removed() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    repo.backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    let bogus = ObjectId::from_bytes([0xAB; 32]);
    let path = t.repo_path().join(keys::gc(&bogus));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, keys::GC_MARK_MAGIC).unwrap();
    settle().await;
    let p = repo
        .prune(prune_opts(r + Duration::hours(1)))
        .await
        .unwrap();
    assert_eq!(p.stale_marks, 1, "{p:?}");
    assert!(!path.exists());
}

/// reviewer 的 1-A：prune 走訪之後、寫新 index 之前，另一個 backup commit 了一個引用「走訪時沒人引用」
/// 的 chunk 的 snapshot。舊 pack 必須留在 index（帶標記），下一輪復活、再 repack，資料不能不見。
#[tokio::test]
async fn snapshot_committed_inside_a_prune_keeps_its_chunks() {
    let t = TestRepo::new().await;
    let src_a = t.dir.path().join("a");
    let src_b = t.dir.path().join("b");
    for s in [&src_a, &src_b] {
        std::fs::create_dir_all(s).unwrap();
        std::fs::write(s.join("shared.bin"), random_bytes(93, 60 * 1024)).unwrap();
    }
    std::fs::write(src_a.join("zz-big.bin"), random_bytes(94, 900 * 1024)).unwrap();
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    // a 被備份又被 forget：大檔的 chunk 沒人引用，pack 因 shared 還活著
    let b1 = repo
        .backup(std::slice::from_ref(&src_a), client(1, r))
        .await
        .unwrap();
    repo.forget(kist_core::ForgetOptions {
        snapshots: vec![b1.snapshot_key],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();
    t.open()
        .await
        .backup(
            std::slice::from_ref(&src_b),
            client(2, r + Duration::hours(1)),
        )
        .await
        .unwrap();
    // client 1 再備份 a，大檔全部去重到舊 pack；停在 commit 前
    let prepared = repo
        .backup_prepare(
            std::slice::from_ref(&src_a),
            client(1, r + Duration::hours(2)),
        )
        .await
        .unwrap();
    // prune 走訪（大檔的 chunk 不算被引用 → 舊 pack 會被 repack）
    settle().await;
    let plan = repo
        .prune_plan(prune_opts(r + Duration::days(4)))
        .await
        .unwrap();
    assert!(plan.report().repacked_packs >= 1, "{:?}", plan.report());
    // 走訪之後 commit
    let s = prepared.commit().await.unwrap();
    // 然後 prune 才寫新 index、標記舊 pack
    let p1 = plan.execute().await.unwrap();
    assert!(p1.marked >= 1, "{p1:?}");
    let out = t.dir.path().join("out");
    let fresh = t.open().await;
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    restore_matches(&fresh, &s.snapshot_key, &src_a, &out).await;

    // 之後的每一輪：舊 pack 先復活（它是大檔 chunk 的正本）、再被 repack、最後刪掉；資料一直都在
    for day in [8, 12, 16, 20] {
        repo.backup(
            std::slice::from_ref(&src_a),
            client(1, r + Duration::days(day) - Duration::hours(1)),
        )
        .await
        .unwrap();
        settle().await;
        let p = repo
            .prune(prune_opts(r + Duration::days(day)))
            .await
            .unwrap();
        assert!(p.skipped.is_empty(), "{p:?}");
        let fresh = t.open().await;
        let report = fresh
            .check(CheckOptions {
                read_data: true,
                repair: false,
            })
            .await
            .unwrap();
        assert!(report.errors.is_empty(), "day {day}: {:?}", report.errors);
        restore_matches(&fresh, &s.snapshot_key, &src_a, &out).await;
    }
    assert!(ids_under(&t, "gc").is_empty());
}

/// 同一個 chunk 在兩個 pack 都有副本（兩台 client 同時寫）：只有正本那個 pack 是需要的，
/// 另一個走兩階段刪掉；資料一直讀得到。
#[tokio::test]
async fn duplicate_copies_are_reclaimed() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    repo.backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    let first = ids_under(&t, "packs");
    // 假裝所有 pack 被標記，讓 client 2 把同樣的資料再寫一份
    for id in &first {
        let path = t.repo_path().join(keys::gc(id));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, keys::GC_MARK_MAGIC).unwrap();
    }
    let s2 = t
        .open()
        .await
        .backup(
            std::slice::from_ref(&src),
            client(2, r + Duration::hours(1)),
        )
        .await
        .unwrap();
    assert!(s2.report.chunks_new > 0);
    for id in &first {
        std::fs::remove_file(t.repo_path().join(keys::gc(id))).unwrap();
    }
    assert!(ids_under(&t, "packs").len() > first.len());

    settle().await;

    settle().await;

    let p1 = repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    assert!(p1.marked > 0, "{p1:?}");
    assert_eq!(p1.revived, 0, "{p1:?}");
    // 每個 pack 要嘛是正本、要嘛被標記；兩份都有的 chunk 只留一份
    let marked = ids_under(&t, "gc");
    assert_eq!(
        p1.live_packs as usize + marked.len(),
        ids_under(&t, "packs").len()
    );
    repo.backup(std::slice::from_ref(&src), client(1, r + Duration::days(5)))
        .await
        .unwrap();
    repo.backup(std::slice::from_ref(&src), client(2, r + Duration::days(5)))
        .await
        .unwrap();
    settle().await;
    settle().await;
    let p2 = repo.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    assert_eq!(p2.deleted as usize, marked.len(), "{p2:?}");
    let fresh = t.open().await;
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let out = t.dir.path().join("out");
    restore_matches(&fresh, &s2.snapshot_key, &src, &out).await;
}

/// reviewer 的第二個發現：兩個 prune 重疊（或 prune 途中 rebuild-index）會讓被刪掉的 pack 留在 index 裡
/// （幽靈）。幽靈不能參加正本的選擇（否則真正的持有者會被當垃圾），下次重寫 index 時要丟掉。
#[tokio::test]
async fn phantom_packs_do_not_steal_canonical_from_real_holders() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    repo.backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    let first = ids_under(&t, "packs");
    // client 2 把同樣的資料再寫一份（假裝第一批被標記）
    for id in &first {
        let path = t.repo_path().join(keys::gc(id));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, keys::GC_MARK_MAGIC).unwrap();
    }
    let s2 = t
        .open()
        .await
        .backup(
            std::slice::from_ref(&src),
            client(2, r + Duration::hours(1)),
        )
        .await
        .unwrap();
    for id in &first {
        std::fs::remove_file(t.repo_path().join(keys::gc(id))).unwrap();
    }
    // 製造幽靈：第一批 pack 從儲存消失，但 index 裡還在（= 另一個 prune 刪了它們、我們的 blob 沒被取代）
    for id in &first {
        std::fs::remove_file(t.repo_path().join(keys::pack(id))).unwrap();
    }
    let second: HashSet<ObjectId> = ids_under(&t, "packs");
    assert!(second.is_disjoint(&first));

    settle().await;

    settle().await;

    let p1 = repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    // 真正的持有者一個都不能被標記
    let marked = ids_under(&t, "gc");
    assert!(marked.is_disjoint(&second), "{p1:?} marked={marked:?}");
    assert_eq!(p1.live_packs as usize, second.len(), "{p1:?}");
    // index 已重寫，幽靈不在裡面
    let fresh = t.open().await;
    let index = fresh.load_index().await.unwrap();
    for id in &first {
        assert!(
            !index.packs().any(|(p, _)| p == id),
            "幽靈 {id} 還在 index 裡"
        );
    }
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let out = t.dir.path().join("out");
    restore_matches(&fresh, &s2.snapshot_key, &src, &out).await;
    // 之後收斂
    repo.backup(std::slice::from_ref(&src), client(2, r + Duration::days(5)))
        .await
        .unwrap();
    repo.backup(std::slice::from_ref(&src), client(1, r + Duration::days(5)))
        .await
        .unwrap();
    settle().await;
    settle().await;
    let p2 = repo.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    assert!(p2.skipped.is_empty(), "{p2:?}");
    let report = t
        .open()
        .await
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

/// 被引用的 chunk 只有幽靈持有（index 指到的 pack 不存在）：引用不完整，prune 必須拒絕。
#[tokio::test]
async fn refuses_when_a_referenced_chunk_is_only_in_a_missing_pack() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    repo.backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    let victim = ids_under(&t, "packs").into_iter().next().unwrap();
    std::fs::remove_file(t.repo_path().join(keys::pack(&victim))).unwrap();
    let err = repo
        .prune(prune_opts(r + Duration::days(4)))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Unsafe(_)), "{err}");
    assert!(ids_under(&t, "gc").is_empty());
    assert_eq!(t.count("indexes"), 1);
}

/// pack 與根 tree 已刪、它們的標記還在、index blob 沒動（Go 的「清掃做到一半
/// 當機」狀態；Rust 的 execute 先寫 index 再刪，單一 prune 當機造不出來，
/// 物件被外力刪掉才會）。改寫自已移除的 Go 參考實作
/// （`TestPruneRewritesTheIndexAfterACrashedSweep`）：2026-09 的稽核裡，這是唯一
/// 「沒有 Go 就抓不到」的案例（51be5ed，見 ADR 018）。backup 放在第二輪 prune
/// 之前：Rust 的標記隨物件一起刪，照 Go 的順序走不到 touch 分支。Go 的第三個
/// 斷言（清掉孤兒標記後的 backup 必須重傳）不在這裡；「有幽靈 pack 就重寫
/// index」由 `phantom_packs_do_not_steal_canonical_from_real_holders` 釘住。
/// 釘住三件事：
/// 1. 同內容再備份時，根 tree 以主體身分重寫、卻帶著開始時就在的標記 → 必須補
///    touch，commit 一次成功（否則 commit gate 會安全失敗一次）；
/// 2. 下一輪 prune 把孤兒標記清掉，並重寫 index 丟掉幽靈 pack；
/// 3. 之後 repo 一致、可還原。
#[tokio::test]
async fn backup_and_prune_recover_from_a_crashed_sweep() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    let out = t.dir.path().join("out");
    make_src(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    let b1 = repo
        .backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();
    repo.forget(kist_core::ForgetOptions {
        snapshots: vec![b1.snapshot_key.clone()],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();
    settle().await;
    settle().await;
    let p1 = repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    assert!(p1.marked > 0, "{p1:?}");
    let root = b1.roots[0].tree;
    let root_obj = ObjectId::from_bytes(*root.as_bytes());
    let marks = ids_under(&t, "gc");
    assert!(marks.contains(&root_obj), "根 tree 應該被標記：{p1:?}");
    let crashed_pack = *marks
        .intersection(&ids_under(&t, "packs"))
        .next()
        .expect("至少一個 pack 被標記");

    // 當機狀態：清掃刪了 pack 與根 tree，還沒刪到它們的標記；index blob 原封不動。
    std::fs::remove_file(t.repo_path().join(keys::pack(&crashed_pack))).unwrap();
    std::fs::remove_file(t.repo_path().join(keys::tree(&root))).unwrap();
    settle().await;

    let b2 = t
        .open()
        .await
        .backup(
            std::slice::from_ref(&src),
            client(1, r + Duration::days(4) + Duration::hours(1)),
        )
        .await
        .unwrap();
    assert_eq!(b2.roots[0].tree, root, "內容沒變，根 tree 應該同 ID");
    assert!(
        t.repo_path().join(keys::touch(&root)).exists(),
        "重寫的主體 tree 帶著開始時的標記，要補 touch"
    );
    assert!(b2.report.packs_new > 0, "{:?}", b2.report);

    settle().await;
    let p2 = repo
        .prune(prune_opts(r + Duration::days(4) + Duration::hours(2)))
        .await
        .unwrap();
    assert!(p2.stale_marks >= 1, "{p2:?}");
    assert!(p2.skipped.is_empty(), "{p2:?}");
    let marks = ids_under(&t, "gc");
    assert!(!marks.contains(&crashed_pack), "孤兒標記沒清掉：{p2:?}");
    assert!(
        !marks.contains(&root_obj),
        "b2 引用的根 tree 應該復活：{p2:?}"
    );
    let fresh = t.open().await;
    let index = fresh.load_index().await.unwrap();
    assert!(
        !index.packs().any(|(p, _)| *p == crashed_pack),
        "新開的 repo 仍在 index 裡看到已消失的 pack {crashed_pack}"
    );
    let report = fresh
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    restore_matches(&fresh, &b2.snapshot_key, &src, &out).await;
}

/// ADR 019 A8：第 13 步（parity 孤兒清掃）刪 sidecar 前會再看一眼 pack 在不在；
/// 那一眼的錯誤不是 NotFound（暫時性錯誤）時，不得當成「pack 不在」而刪掉
/// sidecar，要讓 prune 回錯（同第 14 步的分法）。
/// 注入：`packs/<id>` 放一個 Unix socket。本機後端的 list 只列一般檔案，所以它
/// 不在清單裡（像並發 backup 剛 Put、還沒列到的 pack）；head 的 open(2) 對
/// socket 回 ENXIO，不是 NotFound。
#[cfg(unix)]
#[tokio::test]
async fn parity_sweep_keeps_the_sidecar_when_the_pack_head_fails() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let r = OffsetDateTime::now_utc();
    repo.backup(std::slice::from_ref(&src), client(1, r))
        .await
        .unwrap();

    let id = ObjectId::from_bytes([0xCD; 32]);
    let sidecar = t.repo_path().join(kist_format::parity::key(&id));
    std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    std::fs::write(&sidecar, b"parity").unwrap();
    // socket 路徑有長度上限（sun_path 108 bytes）：先在短路徑建，再搬到 pack 的位置。
    let short = t.dir.path().join("s");
    drop(std::os::unix::net::UnixListener::bind(&short).unwrap());
    std::fs::rename(&short, t.repo_path().join(keys::pack(&id))).unwrap();

    let res = repo.prune(prune_opts(r + Duration::hours(1))).await;
    assert!(
        sidecar.exists(),
        "pack 的 HEAD 失敗不等於 pack 不在：sidecar 不得刪：{res:?}"
    );
    assert!(res.is_err(), "HEAD 的錯誤要讓 prune 回錯：{res:?}");
}
