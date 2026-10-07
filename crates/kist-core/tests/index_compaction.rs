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

/// 觸發看的是**有效** blob（§10）。合併後被取代的舊 blob 要等 grace 才刪；
/// 這段期間再跑 prune（沒有任何變動）不得把 index 再重寫一次——否則
/// grace 內每次 prune 都多寫一顆整份 index。
#[tokio::test]
async fn superseded_blobs_waiting_for_grace_do_not_retrigger_compaction() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..(kist_core::MAX_EFFECTIVE_BLOBS + 1) as u64 {
        std::fs::write(src.join(format!("f{i:03}.bin")), random_bytes(i, 1024)).unwrap();
        repo.backup(std::slice::from_ref(&src), backup_options())
            .await
            .unwrap();
    }
    // 預設 grace：被取代的 blob 只被標記，不刪。
    repo.prune(PruneOptions::default()).await.unwrap();
    let mut errors = Vec::new();
    let after_first = repo.load_index_blobs(&mut errors).await.unwrap();
    assert_eq!(
        after_first.effective.len(),
        1,
        "前置條件：第一次 prune 已合併"
    );
    assert!(
        after_first.superseded.len() > kist_core::MAX_EFFECTIVE_BLOBS,
        "前置條件：被取代的 blob 還在等 grace（{} 顆）",
        after_first.superseded.len()
    );
    let effective_id = after_first.effective[0].0;

    repo.prune(PruneOptions::default()).await.unwrap();
    let after_second = repo.load_index_blobs(&mut errors).await.unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        after_second
            .effective
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![effective_id],
        "只有 1 顆有效 blob、沒有任何變動：第二次 prune 不該重寫 index"
    );
}
