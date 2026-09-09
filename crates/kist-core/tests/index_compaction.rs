//! index 壓縮觸發（v3 §10 的 MUST）：有效（未被 supersede）blob 超過
//! `MAX_EFFECTIVE_BLOBS` 時，prune 必須合併重寫為一顆——與刪除無關，
//! 只增不刪的 repo 不能讓 blob 無限增長。

use std::time::Duration;

use kist_core::PruneOptions;

mod common;
use common::{backup_options, TestRepo};

#[tokio::test]
async fn prune_compacts_the_index_past_the_threshold() {
    let t = TestRepo::new().await;
    let repo = t.open().await;

    // 65 次「各新增一個 1 KiB 小檔」的備份：每次寫一顆新 index blob。
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..(kist_core::MAX_EFFECTIVE_BLOBS + 1) as u64 {
        std::fs::write(src.join(format!("f{i:03}.bin")), random_bytes(i, 1024)).unwrap();
        repo.backup(std::slice::from_ref(&src), backup_options())
            .await
            .unwrap();
    }

    let mut errors = Vec::new();
    let blobs = repo.load_index_blobs(&mut errors).await.unwrap();
    assert!(
        blobs.effective.len() > kist_core::MAX_EFFECTIVE_BLOBS,
        "前置條件：只有 {} 顆有效 blob，要多於 {}",
        blobs.effective.len(),
        kist_core::MAX_EFFECTIVE_BLOBS
    );

    // 沒有任何 forget：唯一觸發就是 blob 數本身。prune 必須合併為一顆，
    // 且不刪任何資料物件。
    let report = repo
        .prune(PruneOptions {
            grace: Duration::from_secs(0),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(report.deleted, 0, "沒有遺忘的 snapshot，不得刪除");

    let mut errors = Vec::new();
    let blobs = repo.load_index_blobs(&mut errors).await.unwrap();
    assert_eq!(
        blobs.effective.len(),
        1,
        "prune 必須把有效 blob 合併為一顆（壓縮是強制的）"
    );
}

fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    use rand::{RngExt, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut v = vec![0u8; len];
    rng.fill(&mut v[..]);
    v
}
