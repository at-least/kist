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
