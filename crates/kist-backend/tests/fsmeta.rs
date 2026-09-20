//! POSIX metadata 擷取：mtime 要保留全 i64 範圍（含 1970 年以前的負值）。
//! 夾成 0 的話，restore 會把這些檔案的時間靜靜設成 1970-01-01，而且快速
//! 路徑再也分不清 1969 裡動過的檔案。

use std::time::{Duration, UNIX_EPOCH};

#[test]
fn capture_preserves_pre_epoch_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.txt");
    std::fs::write(&path, b"x").unwrap();
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    let before = UNIX_EPOCH - Duration::from_secs(86_400); // 1969-12-31
    if let Err(e) = f.set_times(std::fs::FileTimes::new().set_modified(before)) {
        // 檔案系統不支援 1970 以前的 mtime：這個環境測不了，明說，不假裝通過。
        panic!("此檔案系統無法設定 1970 年以前的 mtime（{e}）；測試環境不支援");
    }
    drop(f);

    let meta = std::fs::symlink_metadata(&path).unwrap();
    let captured = kist_backend::fsmeta::capture(&meta);
    assert!(
        captured.mtime_ns < 0,
        "pre-epoch mtime 被夾成 {}，應保留負的奈秒值",
        captured.mtime_ns
    );

    // 對照組：epoch 之後的值照常保留（同精度）。
    let after = UNIX_EPOCH + Duration::from_nanos(1_234_567_890);
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(after))
        .unwrap();
    drop(f);
    let captured = kist_backend::fsmeta::capture(&std::fs::symlink_metadata(&path).unwrap());
    assert_eq!(captured.mtime_ns, 1_234_567_890);
}
