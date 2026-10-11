//! forget（§9）：依保留政策或明確指定刪掉 snapshot。只刪 snapshot 本身，它獨占的 pack 由 prune 兩階段回收。
//!
//! - **分組**：(client, roots 的 `path` 與 `t`)，與 backup 找 parent 的條件相同（B1）。政策在每一組裡套用
//!   （[`crate::retention`]）。roots 讀不出來的 snapshot 分不了組，政策不碰它，回報在 `unreadable`。
//! - **每一組最新的那一份不刪**：它是那個 client 下一次 backup 的 parent，backup 沿用它的子樹時不查子樹裡
//!   每個 item 的持有者（B2）。任何非空的政策本來就保留它；明確指定它時拒絕，除非 `retire`——讓那個 client
//!   的這組 roots 退役，只能在它沒有進行中的 backup 時做，刪除完成之前也不能開始新的 backup（§9）。roots
//!   讀不出來的 snapshot 不會是 parent（backup 跳過讀不了的），所以不受這條限制。
//! - **明確指定**：`latest`、完整的 key、`<client>/<時間戳>`、時間戳或它的前綴。完整的 key 也接受只有
//!   `.r1` 的 snapshot，而且不必解得開：兩份都解不開的 snapshot 讓 prune 停住（P5），維護角色以完整的 key
//!   直接 forget 它立刻解除。
//! - **刪除順序**：先 `.r1` 再主體（主體＝commit，中途停下留下的是一個完整的 snapshot，不是只有 `.r1` 的）。
//!   刪除不存在的物件算成功；其他錯誤就停止，那個主體不刪。
//!
//! forget 不建立任何物件、不走柵欄，不需要 P0：寬鬆後端（rclone）上照樣能用，只是那裡沒有 prune 回收空間。

use std::collections::{BTreeMap, BTreeSet, HashSet};

use kist_format::v4::ids::ClientId;
use kist_format::v4::keys::{Key, SNAPSHOTS};
use time::OffsetDateTime;

use super::repo::Repo;
use crate::retention::{apply_policy, Candidate, RetentionPolicy};
use crate::{CoreError, Result};

#[derive(Debug, Clone, Default)]
pub struct ForgetOptions {
    /// 明確要刪的 snapshot（使用者的寫法）。
    pub snapshots: Vec<String>,
    pub policy: RetentionPolicy,
    /// 明確指定的 snapshot 是它那一組最新的一份時也刪（那組 roots 退役，§9）。
    pub retire: bool,
    /// 只算不刪。
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ForgetSummary {
    /// 刪掉的（dry-run 時是會刪的）snapshot 主體的 key，依時間。
    pub removed: Vec<String>,
    /// 政策保留的 snapshot 與理由。
    pub kept: Vec<(String, Vec<&'static str>)>,
    /// 讀不出 roots、政策沒有碰的 snapshot（只能以完整的 key 明確刪除）。
    pub unreadable: Vec<String>,
}

/// 分組的 key：(client, roots 的 (path, t))。
type Group = (ClientId, Vec<(Vec<u8>, u8)>);

/// 列表裡的 snapshot：主體與 `.r1` 在不在。
#[derive(Debug, Default, Clone, Copy)]
struct Listed {
    body: bool,
    r1: bool,
}

fn snapshot_parts(key: &Key) -> Option<(ClientId, i64)> {
    match key {
        Key::Snapshot { client, ts, .. } => Some((*client, ts.nanos())),
        _ => None,
    }
}

impl Repo {
    /// forget（§9）。見模組說明。
    pub async fn forget(&self, opts: ForgetOptions) -> Result<ForgetSummary> {
        if opts.snapshots.is_empty() && opts.policy.is_empty() {
            return Err(CoreError::Usage(
                "nothing to forget: give snapshots or a retention policy (--keep-*)".to_owned(),
            ));
        }
        opts.policy.validate()?;

        let mut listed: BTreeMap<String, Listed> = BTreeMap::new();
        for o in self.backend().list(SNAPSHOTS.trim_end_matches('/')).await? {
            match Key::parse(&o.key) {
                Ok(k @ Key::Snapshot { replica, .. }) => {
                    let e = listed.entry(k.primary().path()).or_default();
                    if replica {
                        e.r1 = true;
                    } else {
                        e.body = true;
                    }
                }
                _ => tracing::warn!("{}: not a v4 snapshot key; ignored", o.key),
            }
        }

        // 明確指定的：完整的 key 照列表找（主體或 .r1，不解密），其他寫法只找主體
        let mut explicit: BTreeSet<String> = BTreeSet::new();
        for spec in &opts.snapshots {
            let key = match Key::parse(spec) {
                Ok(k @ Key::Snapshot { replica: false, .. }) => {
                    if !listed.contains_key(&k.path()) {
                        return Err(CoreError::SnapshotNotFound(spec.clone()));
                    }
                    k.path()
                }
                _ => self.resolve_snapshot(spec).await?.path(),
            };
            explicit.insert(key);
        }

        // 主體的分組；讀不出 roots 的不分組
        let live = self.live().await?;
        let mut groups: BTreeMap<Group, Vec<(String, i64)>> = BTreeMap::new();
        let mut unreadable = Vec::new();
        for (path, l) in &listed {
            if !l.body {
                continue;
            }
            let key = Key::parse(path)?;
            let roots = async {
                let snap = self.read_snapshot(&key).await?;
                let tree = snap.roots;
                live.run_send(|r| async move { r.roots(&tree).await }).await
            }
            .await;
            let Some((client, nanos)) = snapshot_parts(&key) else {
                continue;
            };
            match roots {
                Ok(roots) => {
                    let group = (client, roots.into_iter().map(|r| (r.path, r.t)).collect());
                    groups.entry(group).or_default().push((path.clone(), nanos));
                }
                Err(e) => {
                    tracing::warn!("{path}: {e}; the retention policy leaves it alone");
                    unreadable.push(path.clone());
                }
            }
        }

        // 每一組最新的那一份（backup 的 parent）
        let newest: HashSet<String> = groups
            .values()
            .filter_map(|v| v.iter().max_by_key(|(k, n)| (*n, k.clone())))
            .map(|(k, _)| k.clone())
            .collect();

        let mut removed = explicit.clone();
        let mut summary = ForgetSummary {
            unreadable,
            ..Default::default()
        };
        if !opts.policy.is_empty() {
            for members in groups.values() {
                let mut candidates = Vec::new();
                for (key, nanos) in members {
                    if explicit.contains(key) {
                        continue;
                    }
                    let time = OffsetDateTime::from_unix_timestamp_nanos(i128::from(*nanos))
                        .map_err(|e| CoreError::corrupt(key, format!("bad time {nanos}: {e}")))?;
                    candidates.push(Candidate {
                        key: key.clone(),
                        time,
                    });
                }
                for (key, reasons) in apply_policy(&candidates, &opts.policy) {
                    if reasons.is_empty() {
                        removed.insert(key);
                    } else {
                        summary.kept.push((key, reasons));
                    }
                }
            }
        }

        let protected: Vec<&str> = removed
            .iter()
            .filter(|k| newest.contains(*k))
            .map(String::as_str)
            .collect();
        if !protected.is_empty() && !opts.retire {
            return Err(CoreError::Usage(format!(
                "not forgetting {}: the newest snapshot of a client and its roots is the parent of \
                 that client's next backup; to retire those roots, pass --retire while that client \
                 has no backup running",
                protected.join(", ")
            )));
        }

        let mut removed: Vec<String> = removed.into_iter().collect();
        removed.sort_by_key(|k| {
            Key::parse(k)
                .ok()
                .and_then(|k| snapshot_parts(&k))
                .map(|(c, n)| (n, c))
        });
        if !opts.dry_run {
            for path in &removed {
                let key = Key::parse(path)?;
                let Key::Snapshot { client, ts, .. } = key else {
                    continue;
                };
                for replica in [true, false] {
                    let k = Key::Snapshot {
                        client,
                        ts: ts.clone(),
                        replica,
                    };
                    self.backend().delete(&k.path()).await?;
                }
            }
        }
        summary.removed = removed;
        Ok(summary)
    }
}
