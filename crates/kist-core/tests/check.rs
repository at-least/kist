//! check 的行為：正常 repo 無錯；人為破壞要被抓到。

mod common;

use common::*;
use kist_core::CheckOptions;

async fn repo_with_data() -> (TestRepo, kist_core::Repository) {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    repo.backup(&[src], backup_options()).await.unwrap();
    (t, repo)
}

fn some_pack(t: &TestRepo) -> std::path::PathBuf {
    let mut packs = walk_files(&t.repo_path().join("packs"));
    packs.sort();
    packs.remove(0)
}

#[tokio::test]
async fn clean_repo_passes_both_modes() {
    let (_t, repo) = repo_with_data().await;
    let quick = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(quick.errors.is_empty(), "{:?}", quick.errors);
    assert!(quick.packs > 0 && quick.snapshots == 1 && quick.chunks > 0);
    let full = repo.check(CheckOptions { read_data: true }).await.unwrap();
    assert!(full.errors.is_empty(), "{:?}", full.errors);
}

#[tokio::test]
async fn flipped_byte_in_pack_is_detected_by_read_data() {
    let (t, repo) = repo_with_data().await;
    let pack = some_pack(&t);
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[100] ^= 0x01;
    std::fs::write(&pack, bytes).unwrap();

    let report = repo.check(CheckOptions { read_data: true }).await.unwrap();
    let name = pack.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "錯誤訊息應指名被破壞的 pack：{:?}",
        report.errors
    );
}

#[tokio::test]
async fn truncated_pack_is_detected_without_reading_data() {
    let (t, repo) = repo_with_data().await;
    let pack = some_pack(&t);
    let bytes = std::fs::read(&pack).unwrap();
    std::fs::write(&pack, &bytes[..bytes.len() - 10]).unwrap();

    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    let name = pack.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "{:?}",
        report.errors
    );
}

#[tokio::test]
async fn missing_pack_is_detected() {
    let (t, repo) = repo_with_data().await;
    let pack = some_pack(&t);
    std::fs::remove_file(&pack).unwrap();
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    let name = pack.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "{:?}",
        report.errors
    );
}

#[tokio::test]
async fn missing_tree_is_detected() {
    let (t, repo) = repo_with_data().await;
    let mut trees = walk_files(&t.repo_path().join("trees"));
    trees.sort();
    let tree = trees.remove(0);
    std::fs::remove_file(&tree).unwrap();
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    let name = tree.file_name().unwrap().to_str().unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(name)),
        "{:?}",
        report.errors
    );
}

/// 把物件 A 的檔案複製到物件 B 的名稱上：解密會成功（同 key、同種類），
/// 只有「名稱 = 密文 hash」的驗證能抓到。
fn swap_objects(dir: &std::path::Path) -> (String, String) {
    let mut files = walk_files(dir);
    files.retain(|p| p.is_file());
    files.sort();
    let (a, b) = (files[0].clone(), files[1].clone());
    std::fs::copy(&a, &b).unwrap();
    (
        a.file_name().unwrap().to_str().unwrap().to_owned(),
        b.file_name().unwrap().to_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn tree_copied_over_another_tree_is_detected() {
    let (t, repo) = repo_with_data().await;
    let (_a, b) = swap_objects(&t.repo_path().join("trees"));
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains(&b) && e.contains("name")),
        "{:?}",
        report.errors
    );
    let key = repo.resolve_snapshot("latest").await.unwrap();
    let err = repo
        .restore(
            &key,
            &t.dir.path().join("out"),
            kist_core::RestoreOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, kist_core::CoreError::Corrupt { .. }), "{err}");
}

#[tokio::test]
async fn index_copied_over_another_index_is_detected() {
    let (t, repo) = repo_with_data().await;
    // 第二次 backup 有新 chunk 才會有第二個 index blob
    let src = t.dir.path().join("src");
    std::fs::write(src.join("extra.bin"), random_bytes(99, 50 * 1024)).unwrap();
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    assert_eq!(t.count("indexes"), 2);
    let (_a, b) = swap_objects(&t.repo_path().join("indexes"));
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(&b)),
        "{:?}",
        report.errors
    );
    assert!(
        repo.load_index().await.is_err(),
        "load_index 不該接受名稱不符的 index"
    );
}

#[tokio::test]
async fn snapshot_copied_over_another_snapshot_is_detected() {
    let (t, repo) = repo_with_data().await;
    let src = t.dir.path().join("src");
    repo.backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let client_dir = std::fs::read_dir(t.repo_path().join("snapshots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let (_a, b) = swap_objects(&client_dir);
    let report = repo.check(CheckOptions { read_data: false }).await.unwrap();
    assert!(
        report.errors.iter().any(|e| e.contains(&b)),
        "{:?}",
        report.errors
    );
    let key = repo.resolve_snapshot(&b).await.unwrap();
    assert!(matches!(
        repo.read_snapshot_by_key(&key).await,
        Err(kist_core::CoreError::Corrupt { .. })
    ));
}
