//! forget：保留政策（純函式）與實際刪 snapshot 的行為。

mod common;

use common::*;
use kist_core::forget::{apply_policy, Candidate, ForgetOptions, RetentionPolicy};
use kist_core::{BackupOptions, CheckOptions, CoreError};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

/// 每小時一個 snapshot，共 `n` 個，最新的在 `end`；回傳依時間**新 → 舊**排序的候選清單。
fn hourly(end: OffsetDateTime, n: i64) -> Vec<Candidate> {
    (0..n)
        .map(|i| Candidate {
            key: format!("snapshots/aa/{i:03}"),
            time: end - Duration::hours(i),
        })
        .collect()
}

fn kept<'a>(decisions: &'a [(String, Vec<&'static str>)]) -> Vec<&'a str> {
    decisions
        .iter()
        .filter(|(_, reasons)| !reasons.is_empty())
        .map(|(k, _)| k.as_str())
        .collect()
}

#[test]
fn keep_last_keeps_the_newest_n() {
    let end = datetime!(2026-09-05 10:30:00 UTC);
    let policy = RetentionPolicy {
        keep_last: Some(3),
        ..Default::default()
    };
    let d = apply_policy(&hourly(end, 10), &policy);
    assert_eq!(
        kept(&d),
        ["snapshots/aa/000", "snapshots/aa/001", "snapshots/aa/002"]
    );
    assert_eq!(d[0].1, ["last"]);
}

#[test]
fn keep_daily_keeps_newest_of_each_day() {
    // 10:30 往回 48 小時：跨 09-05、09-04、09-03 三天
    let end = datetime!(2026-09-05 10:30:00 UTC);
    let policy = RetentionPolicy {
        keep_daily: Some(2),
        ..Default::default()
    };
    let d = apply_policy(&hourly(end, 48), &policy);
    // 09-05 最新的是 000（10:30）；09-04 最新的是 11 小時前（23:30）= 011
    assert_eq!(kept(&d), ["snapshots/aa/000", "snapshots/aa/011"]);
}

#[test]
fn keep_hourly_weekly_monthly_yearly_use_distinct_buckets() {
    let end = datetime!(2026-01-01 00:10:00 UTC);
    let policy = RetentionPolicy {
        keep_hourly: Some(2),
        keep_weekly: Some(2),
        keep_monthly: Some(2),
        keep_yearly: Some(2),
        ..Default::default()
    };
    // 每小時一個、往回 40 天
    let d = apply_policy(&hourly(end, 24 * 40), &policy);
    let k = kept(&d);
    // hourly：000（00:10）、001（23:10 前一天）
    assert!(k.contains(&"snapshots/aa/000") && k.contains(&"snapshots/aa/001"));
    // yearly：2026 最新 = 000；2025 最新 = 001（2025-12-31 23:10）
    // monthly：2026-01 = 000；2025-12 = 001
    // weekly：ISO 週；2026-01-01 是週四，前一週最新的是 2025-12-28（週日）23:10 = 3 天又 1 小時前 = 073
    assert!(k.contains(&"snapshots/aa/073"), "{k:?}");
    assert_eq!(k.len(), 3, "{k:?}");
    let reasons_000 = &d[0].1;
    assert!(reasons_000.contains(&"hourly") && reasons_000.contains(&"yearly"));
}

#[test]
fn keep_within_is_relative_to_the_newest_snapshot() {
    // 最新的 snapshot 是 10 天前：距「現在」都超過 2 小時，但 restic 語意是距最新的那個
    let end = datetime!(2026-08-26 10:30:00 UTC);
    let policy = RetentionPolicy {
        keep_within: Some(std::time::Duration::from_secs(2 * 3600)),
        ..Default::default()
    };
    let d = apply_policy(&hourly(end, 10), &policy);
    assert_eq!(
        kept(&d),
        ["snapshots/aa/000", "snapshots/aa/001", "snapshots/aa/002"]
    );
}

#[test]
fn zero_counts_are_rejected() {
    let policy = RetentionPolicy {
        keep_last: Some(0),
        ..Default::default()
    };
    assert!(matches!(policy.validate(), Err(CoreError::Usage(_))));
    let policy = RetentionPolicy {
        keep_within: Some(std::time::Duration::ZERO),
        ..Default::default()
    };
    assert!(matches!(policy.validate(), Err(CoreError::Usage(_))));
}

#[test]
fn empty_policy_keeps_nothing() {
    let end = datetime!(2026-09-05 10:30:00 UTC);
    let d = apply_policy(&hourly(end, 3), &RetentionPolicy::default());
    assert!(kept(&d).is_empty());
}

fn client(id: u8) -> BackupOptions {
    BackupOptions {
        client_id: [id; 16],
        hostname: format!("host{id}"),
        username: "tester".to_owned(),
        now: None,
        gc_grace: kist_core::DEFAULT_GC_GRACE,
        parity: 0,
        progress: None,
    }
}

#[tokio::test]
async fn forget_applies_policy_per_client_and_path_group() {
    let t = TestRepo::new().await;
    let src_a = t.dir.path().join("a");
    let src_b = t.dir.path().join("b");
    make_source(&src_a);
    make_source(&src_b);
    let repo = t.open().await;
    let mut keys_a = Vec::new();
    for _ in 0..3 {
        let s = repo
            .backup(std::slice::from_ref(&src_a), client(0xA))
            .await
            .unwrap();
        keys_a.push(s.snapshot_key);
    }
    // 同一台 client、另一組路徑：自成一組
    let other_paths = repo
        .backup(std::slice::from_ref(&src_b), client(0xA))
        .await
        .unwrap()
        .snapshot_key;
    let mut keys_b = Vec::new();
    for _ in 0..2 {
        let s = repo
            .backup(std::slice::from_ref(&src_b), client(0xB))
            .await
            .unwrap();
        keys_b.push(s.snapshot_key);
    }
    assert_eq!(t.count("snapshots"), 6);

    // dry-run：算出來要刪的一樣，但什麼都不動
    let policy = RetentionPolicy {
        keep_last: Some(1),
        ..Default::default()
    };
    let dry = repo
        .forget(ForgetOptions {
            snapshots: vec![],
            policy: policy.clone(),
            dry_run: true,
        })
        .await
        .unwrap();
    assert_eq!(dry.removed.len(), 3);
    assert_eq!(t.count("snapshots"), 6);

    let summary = repo
        .forget(ForgetOptions {
            snapshots: vec![],
            policy,
            dry_run: false,
        })
        .await
        .unwrap();
    let mut removed = summary.removed.clone();
    removed.sort();
    let mut expected = vec![keys_a[0].clone(), keys_a[1].clone(), keys_b[0].clone()];
    expected.sort();
    assert_eq!(removed, expected);
    assert_eq!(t.count("snapshots"), 3);
    let remaining = repo.list_snapshot_keys().await.unwrap();
    assert!(remaining.contains(&keys_a[2]));
    assert!(remaining.contains(&keys_b[1]));
    assert!(remaining.contains(&other_paths));
    // 資料還在，repo 一致
    let report = repo
        .check(CheckOptions {
            read_data: true,
            repair: false,
        })
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[tokio::test]
async fn forget_explicit_snapshots_and_refuses_empty_request() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let first = repo
        .backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap()
        .snapshot_key;
    let second = repo
        .backup(std::slice::from_ref(&src), client(1))
        .await
        .unwrap()
        .snapshot_key;

    let err = repo
        .forget(ForgetOptions {
            snapshots: vec![],
            policy: RetentionPolicy::default(),
            dry_run: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Usage(_)), "{err}");
    assert_eq!(t.count("snapshots"), 2);

    let summary = repo
        .forget(ForgetOptions {
            snapshots: vec![first.clone()],
            policy: RetentionPolicy::default(),
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(summary.removed, vec![first.clone()]);
    assert_eq!(repo.list_snapshot_keys().await.unwrap(), vec![second]);

    // 不存在的 snapshot：錯誤
    let err = repo
        .forget(ForgetOptions {
            snapshots: vec![first],
            policy: RetentionPolicy::default(),
            dry_run: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::SnapshotNotFound(_)), "{err}");
}
