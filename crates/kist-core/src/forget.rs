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
pub use crate::retention::{apply_policy, Candidate, RetentionPolicy};
use crate::{CoreError, Result};

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
                let time =
                    OffsetDateTime::from_unix_timestamp_nanos(i128::from(s.snapshot.time_ns))
                        .map_err(|e| CoreError::Corrupt {
                            key: s.key.clone(),
                            reason: format!("bad time {}: {e}", s.snapshot.time_ns),
                        })?;
                let paths = s.snapshot.roots.iter().map(|r| r.path.to_vec()).collect();
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
                // 副本隨主體（docs/format.md §13.5）。不存在（沒設副本）是
                // 常態；**其他**錯誤必須擋下主體的刪除：主體刪了、副本留
                // 著，是 prune 不清、check 當「主體意外遺失」災難訊號回報的
                // 孤兒——一次暫時性網路錯誤不該造成永久洩漏＋常設誤報。
                if let Err(e) = self
                    .backend()
                    .delete(&format!("{key}{}", kist_format::keys::REPLICA_SUFFIX))
                    .await
                {
                    if !matches!(e, kist_backend::BackendError::NotFound(_)) {
                        return Err(e.into());
                    }
                    tracing::debug!("no replica for {key}");
                }
                self.backend().delete(key).await?;
            }
        }
        summary.removed = removed;
        Ok(summary)
    }
}
