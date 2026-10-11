//! 保留政策（`kist_core::retention`，forget 用的純函式）：每一種 keep 規則、零值拒絕、空的政策。搬自 v3 的
//! `forget.rs`（政策在 v3 與 v4 共用；v4 的 forget 怎麼分組與刪除見 v4_forget.rs）。

use kist_core::retention::{apply_policy, Candidate, RetentionPolicy};
use kist_core::CoreError;
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
