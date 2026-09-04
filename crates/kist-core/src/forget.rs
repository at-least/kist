//! forget：依保留政策或明確指定刪掉 snapshot。
//!
//! 只刪 snapshot 物件本身（立即刪，不走兩階段）：snapshot 是 commit point，
//! 拿掉之後它獨占的 pack / tree 就成了垃圾，由 `prune` 兩階段回收。
//! 政策以 (client id, paths) 分組套用，與 backup 選 parent 的分組一致；
//! 明確指定的 snapshot 不管政策一律刪。
//!
//! 政策的語意沿用 restic（`keep_within` 見下）：由新到舊逐一看，每個「桶」（小時、日、ISO 週、月、年）
//! 只保留該桶裡最新的一個，各類桶各自有數量上限；`keep_last` 保留最新 N 個；
//! `keep_within` 保留距**該組最新 snapshot** 一段時間內的全部（restic 語意；不是距現在，
//! 否則一台停機的機器回來時 forget 會把它的 snapshot 全刪光）。任何一條理由成立就保留。
//! 任何 `keep_*` 設成 0 一律拒絕（restic 也是）：那會把整組刪光，幾乎不會是本意。

use std::collections::BTreeMap;

use kist_format::keys;
use time::OffsetDateTime;

use crate::repo::Repository;
use crate::{CoreError, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub keep_last: Option<u32>,
    pub keep_hourly: Option<u32>,
    pub keep_daily: Option<u32>,
    pub keep_weekly: Option<u32>,
    pub keep_monthly: Option<u32>,
    pub keep_yearly: Option<u32>,
    /// 用 std 的 Duration，CLI 不必依賴 `time` crate。
    pub keep_within: Option<std::time::Duration>,
}

impl RetentionPolicy {
    /// 沒有任何規則。
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// 0 不是合法的保留數量。
    pub fn validate(&self) -> Result<()> {
        let counts = [
            ("--keep-last", self.keep_last),
            ("--keep-hourly", self.keep_hourly),
            ("--keep-daily", self.keep_daily),
            ("--keep-weekly", self.keep_weekly),
            ("--keep-monthly", self.keep_monthly),
            ("--keep-yearly", self.keep_yearly),
        ];
        for (name, n) in counts {
            if n == Some(0) {
                return Err(CoreError::Usage(format!(
                    "{name} 0 would remove every snapshot in a group; refusing"
                )));
            }
        }
        if self.keep_within == Some(std::time::Duration::ZERO) {
            return Err(CoreError::Usage(
                "--keep-within 0 would remove every snapshot in a group; refusing".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ForgetOptions {
    /// 明確要刪的 snapshot key（已解析成完整 key）。
    pub snapshots: Vec<String>,
    pub policy: RetentionPolicy,
    /// 只算不刪。
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ForgetSummary {
    /// 已刪（dry-run 時是「會刪」）的 snapshot key。
    pub removed: Vec<String>,
    /// 政策保留下來的 snapshot 與理由。
    pub kept: Vec<(String, Vec<&'static str>)>,
}

/// 政策分組的 key：(client id, paths)。
type GroupKey = (Vec<u8>, Vec<Vec<u8>>);

/// 政策的輸入：一個 snapshot 的 key 與開始時間。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub key: String,
    pub time: OffsetDateTime,
}

/// 對**同一組**的候選套用政策。輸入不需排序；輸出依時間新 → 舊，
/// 每個 key 附上保留理由（空 = 刪）。`keep_within` 以這組最新的 snapshot 為基準。
pub fn apply_policy(
    candidates: &[Candidate],
    policy: &RetentionPolicy,
) -> Vec<(String, Vec<&'static str>)> {
    let mut sorted: Vec<&Candidate> = candidates.iter().collect();
    sorted.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.key.cmp(&a.key)));
    let latest = sorted.first().map(|c| c.time);

    // 每種桶：(理由, 剩餘數量, 上一個保留的桶 key)
    struct Bucket {
        reason: &'static str,
        remaining: u32,
        last: Option<String>,
        key_of: fn(OffsetDateTime) -> String,
    }
    let mut buckets: Vec<Bucket> = [
        ("hourly", policy.keep_hourly, hour_key as fn(_) -> _),
        ("daily", policy.keep_daily, day_key),
        ("weekly", policy.keep_weekly, week_key),
        ("monthly", policy.keep_monthly, month_key),
        ("yearly", policy.keep_yearly, year_key),
    ]
    .into_iter()
    .filter_map(|(reason, n, key_of)| {
        n.map(|remaining| Bucket {
            reason,
            remaining,
            last: None,
            key_of,
        })
    })
    .collect();
    let mut last_remaining = policy.keep_last.unwrap_or(0);

    let mut out = Vec::with_capacity(sorted.len());
    for c in sorted {
        let mut reasons = Vec::new();
        if last_remaining > 0 {
            last_remaining -= 1;
            reasons.push("last");
        }
        if let (Some(within), Some(latest)) = (policy.keep_within, latest) {
            if c.time >= latest - within {
                reasons.push("within");
            }
        }
        for b in buckets.iter_mut() {
            if b.remaining == 0 {
                continue;
            }
            let k = (b.key_of)(c.time);
            if b.last.as_deref() != Some(k.as_str()) {
                b.last = Some(k);
                b.remaining -= 1;
                reasons.push(b.reason);
            }
        }
        out.push((c.key.clone(), reasons));
    }
    out
}

fn hour_key(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!("{}-{:03}-{:02}", t.year(), t.ordinal(), t.hour())
}
fn day_key(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!("{}-{:03}", t.year(), t.ordinal())
}
fn week_key(t: OffsetDateTime) -> String {
    let (year, week, _) = t.to_offset(time::UtcOffset::UTC).to_iso_week_date();
    format!("{year}-W{week:02}")
}
fn month_key(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!("{}-{:02}", t.year(), u8::from(t.month()))
}
fn year_key(t: OffsetDateTime) -> String {
    t.to_offset(time::UtcOffset::UTC).year().to_string()
}

impl Repository {
    /// 刪 snapshot。明確指定的一律刪；其餘依政策分組決定。
    pub async fn forget(&self, opts: ForgetOptions) -> Result<ForgetSummary> {
        if opts.snapshots.is_empty() && opts.policy.is_empty() {
            return Err(CoreError::Usage(
                "nothing to forget: give snapshot ids or a retention policy (--keep-*)".to_owned(),
            ));
        }
        opts.policy.validate()?;
        let all = self.list_snapshots().await?;
        for key in &opts.snapshots {
            if !all.iter().any(|s| &s.key == key) {
                return Err(CoreError::SnapshotNotFound(key.clone()));
            }
        }
        let mut removed: Vec<String> = opts.snapshots.clone();
        removed.sort();
        removed.dedup();
        let mut summary = ForgetSummary::default();

        if !opts.policy.is_empty() {
            // 分組：(client id, paths)
            let mut groups: BTreeMap<GroupKey, Vec<Candidate>> = BTreeMap::new();
            for s in &all {
                if removed.contains(&s.key) {
                    continue;
                }
                let time = OffsetDateTime::parse(
                    &s.snapshot.time,
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|e| CoreError::Corrupt {
                    key: s.key.clone(),
                    reason: format!("bad time {:?}: {e}", s.snapshot.time),
                })?;
                let paths = s.snapshot.paths.iter().map(|p| p.to_vec()).collect();
                groups
                    .entry((s.snapshot.client_id.clone(), paths))
                    .or_default()
                    .push(Candidate {
                        key: s.key.clone(),
                        time,
                    });
            }
            for candidates in groups.values() {
                for (key, reasons) in apply_policy(candidates, &opts.policy) {
                    if reasons.is_empty() {
                        removed.push(key);
                    } else {
                        summary.kept.push((key, reasons));
                    }
                }
            }
        }

        if !opts.dry_run {
            for key in &removed {
                debug_assert!(key.starts_with(keys::SNAPSHOTS_PREFIX));
                self.backend().delete(key).await?;
            }
        }
        summary.removed = removed;
        Ok(summary)
    }
}
