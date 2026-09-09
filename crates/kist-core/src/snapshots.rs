//! 列出與解析 snapshot。

use kist_format::keys;
use kist_format::snapshot::Snapshot;

use crate::repo::Repository;
use crate::{CoreError, Result};

#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotInfo {
    /// repo 裡的 key：`snapshots/<client hex>/<ts>`。
    pub key: String,
    pub snapshot: Snapshot,
}

impl SnapshotInfo {
    /// key 裡的時間戳部分。
    pub fn timestamp(&self) -> &str {
        timestamp_of(&self.key)
    }

    /// key 裡的 client id（hex）。
    pub fn client_hex(&self) -> &str {
        self.key
            .strip_prefix(keys::SNAPSHOTS_PREFIX)
            .and_then(|s| s.strip_prefix('/'))
            .and_then(|s| s.split('/').next())
            .unwrap_or("")
    }
}

fn timestamp_of(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

impl Repository {
    /// 所有 snapshot 的 key，依時間排序（舊 → 新）。
    /// `.r1` 副本不是獨立的 snapshot（它是主體的逐 byte 複製，
    /// format-v3-draft §13.5），列出時排除。
    pub async fn list_snapshot_keys(&self) -> Result<Vec<String>> {
        let mut keys: Vec<String> = self
            .backend()
            .list(keys::SNAPSHOTS_PREFIX)
            .await?
            .into_iter()
            .map(|o| o.key)
            .filter(|k| !k.ends_with(kist_format::keys::REPLICA_SUFFIX))
            .collect();
        keys.sort_by(|a, b| timestamp_of(a).cmp(timestamp_of(b)).then_with(|| a.cmp(b)));
        Ok(keys)
    }

    /// 所有 snapshot（會逐一讀取解密），依時間排序。
    pub async fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        let mut out = Vec::new();
        for key in self.list_snapshot_keys().await? {
            let snapshot = self.read_snapshot(&key).await?;
            out.push(SnapshotInfo { key, snapshot });
        }
        Ok(out)
    }

    /// 把使用者給的 snapshot 指定轉成 key。接受：`latest`、完整 key、
    /// `<client hex>/<ts>`、`<ts>`、或 `<ts>` 的前綴（必須唯一）。
    pub async fn resolve_snapshot(&self, spec: &str) -> Result<String> {
        let keys = self.list_snapshot_keys().await?;
        if spec == "latest" {
            return keys
                .last()
                .cloned()
                .ok_or_else(|| CoreError::SnapshotNotFound(spec.to_owned()));
        }
        if keys.iter().any(|k| k == spec) {
            return Ok(spec.to_owned());
        }
        let matches: Vec<&String> = keys
            .iter()
            .filter(|k| k.ends_with(&format!("/{spec}")) || timestamp_of(k).starts_with(spec))
            .collect();
        match matches.len() {
            0 => Err(CoreError::SnapshotNotFound(spec.to_owned())),
            1 => Ok(matches[0].clone()),
            n => Err(CoreError::AmbiguousSnapshot(spec.to_owned(), n)),
        }
    }

    pub async fn read_snapshot_by_key(&self, key: &str) -> Result<Snapshot> {
        self.read_snapshot(key).await
    }
}
