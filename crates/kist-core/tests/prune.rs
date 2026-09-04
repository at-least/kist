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

fn client(id: u8, now: OffsetDateTime) -> BackupOptions {
    BackupOptions {
        client_id: [id; 16],
        hostname: format!("host{id}"),
        username: "tester".to_owned(),
        now: Some(now),
        gc_grace: 72 * H,
    }
}

fn prune_opts(now: OffsetDateTime) -> PruneOptions {
    PruneOptions {
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
        now: None,
    })
    .await
    .unwrap();

    // 第一次 prune（物件已經比 grace 老）：只標記、不刪；部分死掉的 pack 被 repack
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
    assert!(!marked.contains(&b2.root));
    // repo 一致（index 已重寫，不含被 repack 的 pack）
    let report = t
        .open()
        .await
        .check(CheckOptions { read_data: true })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    restore_matches(&t.open().await, &b2.snapshot_key, &src, &out).await;

    // 標記後 client 1 又備份了一次（內容同 b2）：不寫新資料，也不會碰被標記的 pack
    let b3 = repo
        .backup(std::slice::from_ref(&src), client(1, r + Duration::days(5)))
        .await
        .unwrap();
    assert_eq!(b3.stats.chunks_new, 0, "{:?}", b3.stats);

    // 第二次 prune：標記已超過 grace，且唯一的活躍 client 在標記後有新 snapshot → 刪
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
    let report = fresh.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // （被 repack 的舊 pack 這時還是孤兒、剛被標記，check 會警告它；最後收斂時再驗沒有警告）
    restore_matches(&fresh, &b2.snapshot_key, &src, &out).await;
    restore_matches(&fresh, &b3.snapshot_key, &src, &out).await;

    // 之後每 4 天跑一次：被 repack 的舊 pack、被取代的舊 index blob 都會被清掉，最後什麼都不剩；
    // 每一步 repo 都要一致
    let mut quiet = 0;
    for day in [12, 16, 20, 24, 28] {
        let p = repo
            .prune(prune_opts(r + Duration::days(day)))
            .await
            .unwrap();
        assert!(p.skipped.is_empty(), "{p:?}");
        let report = t
            .open()
            .await
            .check(CheckOptions { read_data: true })
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
        .check(CheckOptions { read_data: true })
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
        now: None,
    })
    .await
    .unwrap();
    // 剛寫的物件（可能是進行中的 backup）：不標
    let p = repo
        .prune(prune_opts(r + Duration::hours(1)))
        .await
        .unwrap();
    assert_eq!(p.marked, 0, "{p:?}");
    assert!(ids_under(&t, "gc").is_empty());

    let mut opts = prune_opts(r + Duration::days(4));
    opts.dry_run = true;
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
    let path = t.repo_path().join(keys::tree(&b1.root));
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
        now: None,
    })
    .await
    .unwrap();
    let p1 = repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    assert!(p1.marked > 0, "{p1:?}");
    let marked = ids_under(&t, "gc");
    // client 2 commit（標記很年輕，允許）
    let b2 = prepared.commit().await.unwrap();
    // 第二次 prune：那些 pack / tree 又被引用了 → 撤銷標記，不刪
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
    let report = fresh.check(CheckOptions { read_data: true }).await.unwrap();
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
        now: None,
    })
    .await
    .unwrap();
    repo.prune(prune_opts(r + Duration::days(4))).await.unwrap();
    let marked = ids_under(&t, "gc");
    assert!(!marked.is_empty());
    // client 1 在標記後又備份了（不含大檔，所以不會復活那些垃圾）；client 2 沒有 → client 2（活躍）擋住刪除
    std::fs::remove_file(src.join("zz-big.bin")).unwrap();
    repo.backup(std::slice::from_ref(&src), client(1, r + Duration::days(5)))
        .await
        .unwrap();
    let p = repo.prune(prune_opts(r + Duration::days(8))).await.unwrap();
    assert_eq!(p.deleted, 0, "{p:?}");
    assert!(p.blocked > 0, "{p:?}");
    // （client 1 重備份時內容相同的子目錄 tree 會被重新引用而復活，所以不能要求標記原封不動）
    assert!(!ids_under(&t, "gc").is_empty(), "{p:?}");
    // 30 天後 client 2 變成 inactive：不再阻擋
    let p = repo
        .prune(prune_opts(r + Duration::days(40)))
        .await
        .unwrap();
    assert!(p.deleted > 0, "{p:?}");
    assert_eq!(p.blocked, 0, "{p:?}");
    let report = t
        .open()
        .await
        .check(CheckOptions { read_data: true })
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
    std::fs::write(&path, b"KISTGC1\n").unwrap();
    let p = repo
        .prune(prune_opts(r + Duration::hours(1)))
        .await
        .unwrap();
    assert_eq!(p.stale_marks, 1, "{p:?}");
    assert!(!path.exists());
}
