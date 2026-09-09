//! `KIST_RCLONE_BIN` 覆蓋與缺 binary 的錯誤訊息。放在獨立的測試執行檔：同一個
//! 測試執行檔裡的測試共用 process env，動 `KIST_RCLONE_BIN` 會污染其他測試。

use kist_backend::BackendError;

fn rclone_enabled() -> bool {
    std::env::var("KIST_TEST_RCLONE").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn missing_binary_is_named_clearly() {
    if !rclone_enabled() {
        eprintln!("rclone 測試跳過（tests/rclone-setup.sh 未跑）");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("KIST_RCLONE_BIN", dir.path().join("no-such-rclone"));
    let err = kist_backend::Backend::from_url(&format!("rclone://{}", dir.path().display())).await;
    match err {
        Err(BackendError::Rclone(msg)) => {
            assert!(
                msg.contains("KIST_RCLONE_BIN") && msg.contains("cannot spawn"),
                "error should name the binary and the override: {msg}"
            );
        }
        other => panic!("expected Rclone error, got {other:?}"),
    }
}
