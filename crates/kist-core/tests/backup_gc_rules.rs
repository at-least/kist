//! backup 面對 GC 的規則（M3）：
//! - 開始時被 `gc/` 標記的 pack 不拿來去重（chunk 重傳）；
//! - commit 前驗證引用到的每個 pack 都還在；不在就重新載入 index 找別的位置，找不到就安全失敗（不寫 snapshot）；
//! - 標記已超過 grace 的 pack 視同不存在；
//! - snapshot 時間可注入。

mod common;

use std::collections::HashSet;

use common::*;
use kist_core::{BackupOptions, CheckOptions, CoreError, ReloadableIndex, Repository};
use kist_format::{keys, ObjectId};

fn client(id: u8) -> BackupOptions {
    BackupOptions {
        client_id: [id; 16],
        hostname: format!("host{id}"),
        username: "tester".to_owned(),
        now: None,
        gc_grace: std::time::Duration::from_secs(72 * 3600),
    }
}

/// repo 裡的所有 pack id。
fn pack_ids(t: &TestRepo) -> Vec<ObjectId> {
    let dir = t.repo_path().join(keys::PACKS_PREFIX);
    let mut out: Vec<ObjectId> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| keys::object_id_from_key(&e.unwrap().file_name().to_string_lossy()).unwrap())
        .collect();
    out.sort();
    out
}

/// 直接在本機 repo 放一個標記（內容格式由 prune 定義；backup 只看 key 與 mtime）。
fn mark(t: &TestRepo, id: &ObjectId, age: std::time::Duration) {
    let path = t.repo_path().join(keys::gc(id));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"KISTGC1\n").unwrap();
    let when = std::time::SystemTime::now() - age;
    filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(when)).unwrap();
}

async fn chunks_in_pack(repo: &Repository, pack: &ObjectId) -> HashSet<kist_format::ChunkId> {
    repo.load_index()
        .await
        .unwrap()
        .chunks()
        .into_iter()
        .filter(|(_, loc)| &loc.pack == pack)
        .map(|(id, _)| id)
        .collect()
}

#[tokio::test]
async fn marked_pack_is_not_used_for_dedup() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    let packs = pack_ids(&t);
    assert!(packs.len() >= 2, "測試資料應該產生多個 pack");
    let victim = packs[0];
    let victim_chunks = chunks_in_pack(&repo, &victim).await;
    assert!(!victim_chunks.is_empty());

    mark(&t, &victim, std::time::Duration::from_secs(60));
    // 內容沒變：快速路徑本來會整份沿用；被標記的 pack 裡的 chunk 必須重傳
    let s = repo
        .backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    assert_eq!(
        s.stats.chunks_new,
        victim_chunks.len() as u64,
        "{:?}",
        s.stats
    );
    assert!(s.stats.packs_new >= 1);
    // 重傳的 chunk 現在在新 pack 裡：新 blob 的 entries 涵蓋 victim 的每個 chunk
    let mut errors = Vec::new();
    let blobs = t.open().await.load_index_blobs(&mut errors).await.unwrap();
    assert!(errors.is_empty());
    let mut covered = HashSet::new();
    for (_, blob) in &blobs.effective {
        for p in &blob.packs {
            if p.pack != victim {
                covered.extend(p.entries.iter().map(|e| e.id));
            }
        }
    }
    assert!(victim_chunks.is_subset(&covered));
    // 沒被標記的 pack 照常去重：第三次 backup 什麼都不寫
    let s = repo
        .backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    assert_eq!(s.stats.chunks_new, 0, "{:?}", s.stats);
}

#[tokio::test]
async fn commit_fails_safely_when_a_referenced_pack_vanished() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    let victim = pack_ids(&t)[0];

    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    // prune 在這時候把 pack 刪了（沒有別的副本）
    std::fs::remove_file(t.repo_path().join(keys::pack(&victim))).unwrap();
    let err = prepared.commit().await.unwrap_err();
    assert!(matches!(err, CoreError::PackMissing { .. }), "{err}");
    assert_eq!(t.count("snapshots"), 1, "失敗時不能寫 snapshot");
}

#[tokio::test]
async fn commit_finds_moved_chunks_after_reloading_the_index() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    let victim = pack_ids(&t)[0];

    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    // 另一台 client 在 victim 被標記後備份同樣的資料：chunk 被重傳到新 pack
    mark(&t, &victim, std::time::Duration::from_secs(60));
    t.open()
        .await
        .backup(std::slice::from_ref(&src), client(2))
        .await
        .unwrap();
    // 然後 victim 被刪掉（prune 會先寫一個不含它的 index；這裡用 rebuild-index 模擬）
    std::fs::remove_file(t.repo_path().join(keys::pack(&victim))).unwrap();
    std::fs::remove_file(t.repo_path().join(keys::gc(&victim))).unwrap();
    t.open().await.rebuild_index().await.unwrap();
    // 準備好的 backup 引用的是舊位置；commit 重新載入 index 後應該找得到新位置
    let summary = prepared.commit().await.unwrap();
    assert_eq!(t.count("snapshots"), 3);
    // 新 snapshot 的資料全部讀得到
    let fresh = t.open().await;
    let report = fresh.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let out = t.dir.path().join("out");
    let r = fresh
        .restore(&summary.snapshot_key, &out, Default::default())
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
}

#[tokio::test]
async fn commit_treats_an_expired_marker_as_missing() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    let victim = pack_ids(&t)[0];

    // 年輕的標記（prune 第二階段會看到新 snapshot 而復活它）：可以 commit
    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    mark(&t, &victim, std::time::Duration::from_secs(3600));
    prepared.commit().await.unwrap();

    // 超過 grace 的標記：這個 pack 隨時會被刪，不能 commit
    std::fs::remove_file(t.repo_path().join(keys::gc(&victim))).unwrap();
    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    mark(&t, &victim, std::time::Duration::from_secs(4 * 24 * 3600));
    let err = prepared.commit().await.unwrap_err();
    assert!(matches!(err, CoreError::PackMissing { .. }), "{err}");
    assert_eq!(t.count("snapshots"), 2);
}

#[tokio::test]
async fn snapshot_time_can_be_injected() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let mut opts = client(1);
    opts.now = Some(time::macros::datetime!(2030-01-02 03:04:05.000000006 UTC));
    let s = repo.backup(std::slice::from_ref(&src), opts).await.unwrap();
    assert!(
        s.snapshot_key.ends_with("/20300102T030405000000006Z"),
        "{}",
        s.snapshot_key
    );
    let snap = repo.read_snapshot_by_key(&s.snapshot_key).await.unwrap();
    assert_eq!(snap.time, "2030-01-02T03:04:05.000000006Z");
}

/// 開始得早的 restore 手上是舊 index：chunk 被 repack 搬走後，重載 index 就讀得到。
#[tokio::test]
async fn reading_a_moved_chunk_reloads_the_index() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    let victim = pack_ids(&t)[0];
    let victim_chunks = chunks_in_pack(&repo, &victim).await;
    let stale = ReloadableIndex::new(repo.load_index().await.unwrap());
    let chunk = *victim_chunks.iter().next().unwrap();
    let before = repo.read_chunk_reloading(&chunk, &stale).await.unwrap();

    // 「repack」：chunk 被重寫到新 pack、index 重寫、舊 pack 刪掉
    mark(&t, &victim, std::time::Duration::from_secs(60));
    t.open()
        .await
        .backup(std::slice::from_ref(&src), client(2))
        .await
        .unwrap();
    std::fs::remove_file(t.repo_path().join(keys::pack(&victim))).unwrap();
    t.open().await.rebuild_index().await.unwrap();

    assert_eq!(
        stale.get().await.get(&chunk).unwrap().pack,
        victim,
        "還是舊 index"
    );
    let after = repo.read_chunk_reloading(&chunk, &stale).await.unwrap();
    assert_eq!(before, after);
    assert_ne!(
        stale.get().await.get(&chunk).unwrap().pack,
        victim,
        "重載後指到新 pack"
    );

    // 真的不存在的 chunk：重載後仍然是錯，不會無限重試
    let bogus = kist_format::ChunkId::from_bytes([0xEE; 32]);
    let err = repo.read_chunk_reloading(&bogus, &stale).await.unwrap_err();
    assert!(matches!(err, CoreError::ChunkMissing(_)), "{err}");
}

/// 寫過的 tree 有過期標記：我們的 put 比標記新 → 可以 commit；put 比標記舊（backup 跑超過 grace）→ 不行。
#[tokio::test]
async fn commit_checks_expired_markers_on_written_trees() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let first = repo
        .backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    let four_days = std::time::Duration::from_secs(4 * 24 * 3600);

    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    // 標記比我們的 put 舊：prune 刪前會看到 tree 被重寫過而撤銷標記 → 允許
    mark(&t, &first.root, four_days);
    prepared.commit().await.unwrap();
    assert_eq!(t.count("snapshots"), 2);

    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    // 把 tree 的修改時間改到標記之前：等於 put 發生在標記前、backup 跑了超過 grace
    let tree_path = t.repo_path().join(keys::tree(&first.root));
    let older = std::time::SystemTime::now() - five_days();
    filetime::set_file_mtime(&tree_path, filetime::FileTime::from_system_time(older)).unwrap();
    let err = prepared.commit().await.unwrap_err();
    assert!(matches!(err, CoreError::TreeMarked(_)), "{err}");
    assert_eq!(t.count("snapshots"), 2);
}

fn five_days() -> std::time::Duration {
    std::time::Duration::from_secs(5 * 24 * 3600)
}

/// proptest 找到的第一個 bug（repack 丟掉進行中 backup 去重到的 chunk）的回歸測試。
/// 現在 repack 之後舊 pack 留在 index 裡（帶標記），commit 照常成功，資料一直讀得到。
#[tokio::test]
async fn commit_after_a_repack_keeps_its_chunks_reachable() {
    let t = TestRepo::new().await;
    let src_a = t.dir.path().join("a");
    let src_b = t.dir.path().join("b");
    for s in [&src_a, &src_b] {
        std::fs::create_dir_all(s).unwrap();
        std::fs::write(s.join("shared.bin"), random_bytes(91, 60 * 1024)).unwrap();
    }
    std::fs::write(src_a.join("zz-big.bin"), random_bytes(92, 900 * 1024)).unwrap();
    let repo = t.open().await;
    let b1 = repo
        .backup(std::slice::from_ref(&src_a), client(1))
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
        .backup(std::slice::from_ref(&src_b), client(2))
        .await
        .unwrap();
    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src_a), client(1))
        .await
        .unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(5 * 24 * 3600);
    for id in pack_ids(&t) {
        let p = t.repo_path().join(keys::pack(&id));
        filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(old)).unwrap();
    }
    let report = t
        .open()
        .await
        .prune(kist_core::PruneOptions::default())
        .await
        .unwrap();
    assert!(report.repacked_packs >= 1, "{report:?}");

    let s = prepared.commit().await.unwrap();
    assert_eq!(t.count("snapshots"), 2);
    let fresh = t.open().await;
    let r = fresh.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let out = t.dir.path().join("out");
    let rs = fresh
        .restore(&s.snapshot_key, &out, Default::default())
        .await
        .unwrap();
    assert!(rs.errors.is_empty(), "{:?}", rs.errors);
}

/// reviewer 的 1-B：tree 在 backup 開始時已被標記（還年輕）；backup 重 put 它；prune 在 HEAD 與 DELETE
/// 之間沒看到而把它刪掉、連標記一起清了。commit 必須發現 tree 不見了。
#[tokio::test]
async fn commit_checks_trees_that_were_marked_when_the_backup_started() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let first = repo
        .backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    mark(&t, &first.root, std::time::Duration::from_secs(3600));
    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    // prune 刪掉 tree 並清掉標記
    std::fs::remove_file(t.repo_path().join(keys::tree(&first.root))).unwrap();
    std::fs::remove_file(t.repo_path().join(keys::gc(&first.root))).unwrap();
    let err = prepared.commit().await.unwrap_err();
    assert!(matches!(err, CoreError::TreeMarked(_)), "{err}");
    assert_eq!(t.count("snapshots"), 1);

    // 沒被刪（我們的 put 比標記新）：可以 commit
    mark(&t, &first.root, std::time::Duration::from_secs(3600));
    let prepared = repo
        .backup_prepare(std::slice::from_ref(&src), client(1))
        .await
        .unwrap();
    prepared.commit().await.unwrap();
    assert_eq!(t.count("snapshots"), 2);
}
