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
