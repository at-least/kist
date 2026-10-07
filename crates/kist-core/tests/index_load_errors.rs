//! index 讀取失敗的錯誤分類：後端 I/O 錯誤（網路斷、權限、暫時性 5xx）不是
//! repo 損壞——不該被報成 `Corrupt`，更不該建議 `rebuild-index`（那會讓人以為
//! 健康的 repo 壞了）。對照組：blob 內容驗不過（hash 不符）才是損壞。

mod common;

use common::*;
use kist_core::CoreError;
#[cfg(unix)]
use kist_core::Repository;

#[cfg(unix)]
async fn open_cached(t: &TestRepo) -> Repository {
    Repository::open_with_cache(
        t.backend.clone(),
        PASSWORD.as_bytes(),
        Some(t.dir.path().join("cache")),
    )
    .await
    .unwrap()
}

fn index_blobs(t: &TestRepo) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = walk_files(&t.repo_path().join("indexes"))
        .into_iter()
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

/// 快取已知第一顆 blob 之後，新 blob 讀取遇到後端 I/O 錯誤（chmod 000 模擬）：
/// 錯誤要照原樣回（`Backend`），不能變成 `Corrupt`＋`rebuild-index` 建議。
#[cfg(unix)]
#[tokio::test]
async fn backend_io_error_on_index_blob_is_not_corruption() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipping: running as root (chmod 000 擋不住 root)");
        return;
    }
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = open_cached(&t).await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // 讓快取 manifest 記下第一顆 blob
    let repo = open_cached(&t).await;
    repo.load_index().await.unwrap();
    let first = index_blobs(&t);

    // 第二次備份 → 新 blob；快取還不認得它
    let repo = open_cached(&t).await;
    let src2 = t.dir.path().join("src2");
    make_source(&src2);
    // 內容與第一次不同，才會有新 chunk、新 index blob
    std::fs::write(src2.join("unique.bin"), random_bytes(7, 100 * 1024)).unwrap();
    repo.backup(std::slice::from_ref(&src2), backup_options())
        .await
        .unwrap();
    let new_blobs: Vec<_> = index_blobs(&t)
        .into_iter()
        .filter(|p| !first.contains(p))
        .collect();
    assert_eq!(new_blobs.len(), 1, "第二次備份應該正好多一顆 index blob");

    // 新 blob 讀不到（EACCES）：後端 I/O 錯誤，不是損壞
    std::fs::set_permissions(
        &new_blobs[0],
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();
    let repo = open_cached(&t).await;
    let err = repo.load_index().await.unwrap_err();
    assert!(
        matches!(err, CoreError::Backend(_)),
        "後端 I/O 錯誤要照原樣回，不是 {err:?}"
    );
    let msg = err.to_string();
    assert!(!msg.contains("rebuild-index"), "誤導的建議：{msg}");
}

/// 對照組：blob 拉得下來但內容驗不過（hash 不符）——這才是損壞，
/// `Corrupt` 與 `rebuild-index` 建議都對。
#[tokio::test]
async fn hash_mismatch_on_index_blob_is_corruption() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    // 弄壞 blob 內容（長度不變，hash 一定不符）
    let blobs = index_blobs(&t);
    assert!(!blobs.is_empty());
    let mut data = std::fs::read(&blobs[0]).unwrap();
    data[0] ^= 0xFF;
    std::fs::write(&blobs[0], data).unwrap();

    let err = repo.load_index().await.unwrap_err();
    assert!(
        matches!(err, CoreError::Corrupt { .. }),
        "hash 不符是損壞，不是 {err:?}"
    );
    assert!(err.to_string().contains("rebuild-index"));
}
