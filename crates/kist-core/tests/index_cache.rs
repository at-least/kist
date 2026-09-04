//! 本地 index 快取：查詢結果必須與每次從 repo 重讀完全一樣，而且只能省時間，不能省正確性。

mod common;

use std::path::Path;

use common::*;
use kist_core::index::{ChunkLocation, DiskTable, TableRecord};
use kist_core::{BackupOptions, CheckOptions, Repository};
use kist_format::{ChunkId, ObjectId};

fn cache_root(t: &TestRepo) -> std::path::PathBuf {
    t.dir.path().join("cache")
}

async fn open_cached(t: &TestRepo) -> Repository {
    Repository::open_with_cache(t.backend.clone(), PASSWORD.as_bytes(), Some(cache_root(t)))
        .await
        .unwrap()
}

fn cache_files(root: &Path) -> Vec<std::path::PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    walk_files(root)
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}

#[tokio::test]
async fn cached_lookups_match_fresh_index() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = open_cached(&t).await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // 第二次 open：把 blob 併進快取
    let repo = open_cached(&t).await;
    let cached = repo.load_index().await.unwrap();
    assert!(cached.is_cached(), "應該來自快取");
    let files = cache_files(&cache_root(&t));
    assert!(files.iter().any(|p| p.ends_with("index.tbl")), "{files:?}");

    let fresh = t.open().await.load_index().await.unwrap();
    assert!(!fresh.is_cached());
    assert_eq!(cached.len(), fresh.len());
    assert_eq!(cached.pack_count(), fresh.pack_count());
    for (id, loc) in fresh.chunks() {
        assert_eq!(cached.get(&id), Some(loc), "chunk {id}");
        assert!(cached.contains(&id));
    }
    // 不存在的 ID
    assert!(!cached.contains(&ChunkId::from_bytes([0xAB; 32])));
    assert!(cached.get(&ChunkId::from_bytes([0; 32])).is_none());
    assert!(cached.get(&ChunkId::from_bytes([0xFF; 32])).is_none());
}

#[tokio::test]
async fn new_index_blob_from_another_client_is_merged() {
    let t = TestRepo::new().await;
    let src_a = t.dir.path().join("a");
    let src_b = t.dir.path().join("b");
    make_source(&src_a);
    std::fs::create_dir_all(&src_b).unwrap();
    std::fs::write(src_b.join("only-b.bin"), random_bytes(31, 200 * 1024)).unwrap();

    let a = open_cached(&t).await;
    a.backup(std::slice::from_ref(&src_a), backup_options())
        .await
        .unwrap();
    let a = open_cached(&t).await; // 快取建好
    assert!(a.load_index().await.unwrap().is_cached());

    // client B 直接對 repo 寫（不用快取）
    let b = t.open().await;
    let b_opts = BackupOptions {
        client_id: [0x22; 16],
        ..backup_options()
    };
    let sb = b
        .backup(std::slice::from_ref(&src_b), b_opts)
        .await
        .unwrap();
    assert!(sb.stats.chunks_new > 0);

    // A 重新 open：只多讀 B 的新 blob，然後 B 的資料對 A 而言已存在
    let a = open_cached(&t).await;
    let sa = a
        .backup(std::slice::from_ref(&src_b), backup_options())
        .await
        .unwrap();
    assert_eq!(
        sa.stats.chunks_new, 0,
        "B 上傳過的 chunk 不該再上傳：{:?}",
        sa.stats
    );
    let cached = a.load_index().await.unwrap();
    let fresh = t.open().await.load_index().await.unwrap();
    assert_eq!(cached.len(), fresh.len());
}

#[tokio::test]
async fn deleted_index_blob_triggers_rebuild() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = open_cached(&t).await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    std::fs::write(src.join("more.bin"), random_bytes(32, 100 * 1024)).unwrap();
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let repo = open_cached(&t).await;
    let before = repo.load_index().await.unwrap();
    assert!(before.is_cached());

    let mut blobs = walk_files(&t.repo_path().join("indexes"));
    blobs.retain(|p| p.is_file());
    assert_eq!(blobs.len(), 2);
    std::fs::remove_file(&blobs[0]).unwrap();

    let repo = open_cached(&t).await;
    let after = repo.load_index().await.unwrap();
    let fresh = t.open().await.load_index().await.unwrap();
    assert_eq!(after.len(), fresh.len(), "快取必須反映 blob 被刪");
    assert!(after.len() < before.len());
    for (id, loc) in fresh.chunks() {
        assert_eq!(after.get(&id), Some(loc));
    }
}

#[tokio::test]
async fn check_never_uses_the_cache() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = open_cached(&t).await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let repo = open_cached(&t).await;
    assert!(repo.load_index().await.unwrap().is_cached());
    // 破壞 repo 裡的 index blob：快取還是好的，check 必須用 repo 的、報錯
    for blob in walk_files(&t.repo_path().join("indexes")) {
        if blob.is_file() {
            let mut bytes = std::fs::read(&blob).unwrap();
            bytes[40] ^= 0xff;
            std::fs::write(&blob, bytes).unwrap();
        }
    }
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains("index")),
        "{:?}",
        report.errors
    );
}

#[tokio::test]
async fn cache_directory_depends_on_repo_key_and_location() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = open_cached(&t).await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    open_cached(&t).await.load_index().await.unwrap();
    let dirs_before: Vec<_> = std::fs::read_dir(cache_root(&t))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs_before.len(), 1);

    // 同一個 repo 從另一個路徑（symlink）打開：不同 URL → 不同快取目錄
    #[cfg(unix)]
    {
        let alias = t.dir.path().join("repo-alias");
        std::os::unix::fs::symlink(t.repo_path(), &alias).unwrap();
        let backend = kist_backend::Backend::from_url(alias.to_str().unwrap()).unwrap();
        let repo = Repository::open_with_cache(backend, PASSWORD.as_bytes(), Some(cache_root(&t)))
            .await
            .unwrap();
        repo.load_index().await.unwrap();
        let dirs_after: Vec<_> = std::fs::read_dir(cache_root(&t))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(dirs_after.len(), 2, "{dirs_after:?}");
    }
}

fn rec(id: u8, pack: u8, offset: u64) -> TableRecord {
    TableRecord {
        id: ChunkId::from_bytes([id; 32]),
        location: ChunkLocation {
            pack: ObjectId::from_bytes([pack; 32]),
            offset,
            length: 100,
            raw_len: 90,
            flags: 1,
        },
    }
}

#[test]
fn disk_table_build_lookup_iterate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.tbl");
    // 亂序、含重複 id（後者保留先出現的）
    let records = vec![
        rec(9, 1, 0),
        rec(3, 1, 100),
        rec(5, 2, 0),
        rec(3, 2, 999),
        rec(1, 3, 0),
    ];
    let table = DiskTable::build(&path, records).unwrap();
    assert_eq!(table.len(), 4);
    let table = DiskTable::open(&path).unwrap();
    assert_eq!(table.len(), 4);
    assert_eq!(
        table
            .get(&ChunkId::from_bytes([3; 32]))
            .unwrap()
            .unwrap()
            .offset,
        100
    );
    assert_eq!(
        table
            .get(&ChunkId::from_bytes([9; 32]))
            .unwrap()
            .unwrap()
            .pack,
        ObjectId::from_bytes([1; 32])
    );
    assert!(table.get(&ChunkId::from_bytes([4; 32])).unwrap().is_none());
    assert!(table.get(&ChunkId::from_bytes([0; 32])).unwrap().is_none());
    assert!(table
        .get(&ChunkId::from_bytes([255; 32]))
        .unwrap()
        .is_none());
    let ids: Vec<u8> = table
        .iter()
        .unwrap()
        .map(|r| r.unwrap().id.as_bytes()[0])
        .collect();
    assert_eq!(ids, vec![1, 3, 5, 9], "依 id 排序");

    let empty = DiskTable::build(&dir.path().join("empty.tbl"), Vec::new()).unwrap();
    assert_eq!(empty.len(), 0);
    assert!(empty.get(&ChunkId::from_bytes([1; 32])).unwrap().is_none());
}

#[test]
fn disk_table_rejects_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.tbl");
    std::fs::write(&path, b"not a table").unwrap();
    assert!(DiskTable::open(&path).is_err());
    let path2 = dir.path().join("trunc.tbl");
    DiskTable::build(&path2, vec![rec(1, 1, 0), rec(2, 1, 0)]).unwrap();
    let bytes = std::fs::read(&path2).unwrap();
    std::fs::write(&path2, &bytes[..bytes.len() - 10]).unwrap();
    assert!(
        DiskTable::open(&path2).is_err(),
        "紀錄數與檔案長度不符要拒絕"
    );
}
