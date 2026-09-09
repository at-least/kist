//! backup 進度 callback：每個目錄項目處理完各叫一次（phase "files"），
//! 結尾的 flush / verify / commit 各叫一次，且順序固定。

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use kist_core::{BackupOptions, BackupProgress, ProgressCallback};

/// 每次回報記下：(phase, stats.files, current 是否為 Some)
type Entry = (&'static str, u64, bool);

#[tokio::test]
async fn progress_callback_reports_entries_then_final_phases() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;

    let log: Arc<Mutex<Vec<Entry>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let cb = ProgressCallback(Arc::new(move |p: &BackupProgress| {
        sink.lock()
            .unwrap()
            .push((p.phase, p.stats.files, p.current.is_some()));
    }));
    let opts = BackupOptions {
        progress: Some(cb),
        ..backup_options()
    };
    let summary = repo.backup(std::slice::from_ref(&src), opts).await.unwrap();

    let log = log.lock().unwrap().clone();
    assert!(!log.is_empty(), "callback 從未被呼叫");

    // files 計數在整個過程中不能倒退
    for w in log.windows(2) {
        assert!(w[0].1 <= w[1].1, "files 倒退：{:?} -> {:?}", w[0], w[1]);
    }

    // 最後一筆 "files" 的計數 = 最終 summary 的 files
    let last_files = log
        .iter()
        .rposition(|(phase, _, _)| *phase == "files")
        .expect("沒有任何 files 階段的回報");
    assert_eq!(log[last_files].1, summary.stats.files, "{log:?}");
    assert!(summary.stats.files > 0);

    // 所有 files 回報都帶目前路徑；結尾三個階段不帶
    for (phase, _, has_current) in &log[..=last_files] {
        assert_eq!(*phase, "files", "files 之間夾了別的階段：{log:?}");
        assert!(*has_current, "files 階段應有 current：{log:?}");
    }
    let tail: Vec<(&str, bool)> = log[last_files + 1..]
        .iter()
        .map(|(phase, _, has_current)| (*phase, *has_current))
        .collect();
    assert_eq!(
        tail,
        vec![("flush", false), ("verify", false), ("commit", false)],
        "{log:?}"
    );
    // 結尾階段看到的 files 也是最終值
    for entry in &log[last_files + 1..] {
        assert_eq!(entry.1, summary.stats.files, "{entry:?}");
    }
}
