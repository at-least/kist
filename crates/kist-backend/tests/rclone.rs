//! rclone 橋接（`rclone://`）的專屬測試：stdio 往返、寬鬆 `put_if_absent` 語意、
//! 啟動失敗的錯誤訊息。需要 PATH 上有 rclone 且 `KIST_TEST_RCLONE=1`
//! （`eval "$(sh tests/rclone-setup.sh)"`）；沒設就跳過。
//!
//! 這些測試同時是「rclone 宣稱支援的擴充真的會動」的驗證：`put` 會踩到
//! O_EXCL fallback + posix-rename、`put_if_absent` 會踩到 stat + posix-rename。

use kist_backend::BackendError;

fn rclone_enabled() -> bool {
    std::env::var("KIST_TEST_RCLONE").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn stdio_bridge_round_trip() {
    if !rclone_enabled() {
        eprintln!("rclone 測試跳過（tests/rclone-setup.sh 未跑）");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // rclone:///<絕對路徑>：remote 留空 = 本機目錄
    let url = format!("rclone://{}", dir.path().display());
    let b = kist_backend::Backend::from_url(&url).await.unwrap();

    // put（覆蓋）：O_EXCL fallback → posix-rename publish
    b.put("packs/aa", b"payload".to_vec()).await.unwrap();
    assert_eq!(b.get("packs/aa").await.unwrap(), b"payload");
    b.put("packs/aa", b"payload-2".to_vec()).await.unwrap();
    assert_eq!(b.get("packs/aa").await.unwrap(), b"payload-2");

    // put_if_absent（寬鬆）：已存在要 AlreadyExists，而且內容不變
    match b.put_if_absent("packs/aa", b"other".to_vec()).await {
        Err(BackendError::AlreadyExists(_)) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert_eq!(b.get("packs/aa").await.unwrap(), b"payload-2");
    b.put_if_absent("packs/bb", b"first".to_vec())
        .await
        .unwrap();
    assert_eq!(b.get("packs/bb").await.unwrap(), b"first");

    // range 讀
    assert_eq!(b.get_range("packs/aa", 7..9).await.unwrap(), b"-2".to_vec());

    // list（含子目錄遞迴）與 head
    b.put("trees/tt", b"tree".to_vec()).await.unwrap();
    let mut keys: Vec<String> = b
        .list("packs")
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.key)
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["packs/aa".to_string(), "packs/bb".to_string()]);
    assert_eq!(b.size("packs/aa").await.unwrap(), 9);

    // 刪除 → NotFound
    b.delete("packs/aa").await.unwrap();
    assert!(matches!(
        b.get("packs/aa").await,
        Err(BackendError::NotFound(_))
    ));
    // 刪不存在的物件視為已刪（與 S3 一致）
    b.delete("packs/aa").await.unwrap();
}

#[tokio::test]
async fn stdio_bridge_survives_repeated_ops() {
    if !rclone_enabled() {
        eprintln!("rclone 測試跳過（tests/rclone-setup.sh 未跑）");
        return;
    }
    // 同一個連線上連續幾輪寫讀：stdio 子程序的 pipe 不會因為並發請求卡死
    let dir = tempfile::tempdir().unwrap();
    let url = format!("rclone://{}", dir.path().display());
    let b = kist_backend::Backend::from_url(&url).await.unwrap();
    for i in 0..10u8 {
        let key = format!("packs/p{i}");
        b.put(&key, vec![i; 4096]).await.unwrap();
    }
    for i in 0..10u8 {
        let key = format!("packs/p{i}");
        assert_eq!(b.get(&key).await.unwrap(), vec![i; 4096]);
    }
}

#[tokio::test]
async fn bogus_remote_reports_rclones_own_error() {
    if !rclone_enabled() {
        eprintln!("rclone 測試跳過（tests/rclone-setup.sh 未跑）");
        return;
    }
    // 不存在的 remote：rclone 會在版本交換前就退出——錯誤要附上 rclone 說的話，
    // 而不是光溜溜的 EOF。
    let err = match kist_backend::Backend::from_url("rclone://kist-no-such-remote/x").await {
        Err(BackendError::Rclone(msg)) => msg,
        other => panic!("expected Rclone error, got {other:?}"),
    };
    assert!(
        err.contains("kist-no-such-remote") || err.contains("didn't find section"),
        "error should surface rclone's own message: {err}"
    );
}

// 注意：動 `KIST_RCLONE_BIN` 的測試放在 tests/rclone_bin.rs（同一個測試執行檔裡
// 的測試共用 process env，會互相污染 spawn 的路徑）。
