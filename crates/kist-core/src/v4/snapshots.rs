//! 列出、選取與讀取 snapshot（§9）。主體出現＝commit，所以列出的是主體；只有 `.r1` 的 snapshot（commit 前
//! 中斷，或主體遺失）不列出，由 check 回報（§13）。

use kist_format::v4::keys::{Key, SNAPSHOTS};
use kist_format::v4::snapshot::Snapshot;

use super::repo::Repo;
use crate::{CoreError, Result};

fn not_found(e: &CoreError) -> bool {
    matches!(
        e,
        CoreError::Backend(kist_backend::BackendError::NotFound(_))
    )
}

/// 一個 snapshot 與它的 key（主體）。
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub key: Key,
    pub snapshot: Snapshot,
}

impl Repo {
    /// 讀一個 snapshot。`key` 是主體時，主體讀不了（不見、解不開、內容與 key 不符）就讀 `.r1` 並警告：P5 的
    /// 順序；`.r1` 在 B4 通過之後才寫，內容自己是完整的。`key` 是 `.r1` 時只讀它。
    pub async fn read_snapshot(&self, key: &Key) -> Result<Snapshot> {
        let Key::Snapshot {
            client,
            ts,
            replica,
        } = key
        else {
            return Err(CoreError::corrupt(key.path(), "not a snapshot key"));
        };
        if *replica {
            return self.read_snapshot_copy(key).await;
        }
        let body = match self.read_snapshot_copy(key).await {
            Ok(s) => return Ok(s),
            Err(e) => e,
        };
        let r1 = Key::Snapshot {
            client: *client,
            ts: ts.clone(),
            replica: true,
        };
        match self.read_snapshot_copy(&r1).await {
            Ok(s) => {
                tracing::warn!("{}: {body}; read its .r1 instead", key.path());
                Ok(s)
            }
            Err(e) if not_found(&body) && not_found(&e) => {
                Err(CoreError::SnapshotNotFound(key.path()))
            }
            Err(e) => Err(CoreError::corrupt(
                key.path(),
                format!("body: {body}; .r1: {e}"),
            )),
        }
    }

    /// 讀一份（主體或 `.r1`，不換另一份）：`K_snap` 解密（AAD 是主體的 key 路徑）、解碼、內容與 key 核對。
    pub async fn read_snapshot_copy(&self, key: &Key) -> Result<Snapshot> {
        let Key::Snapshot { client, ts, .. } = key else {
            return Err(CoreError::corrupt(key.path(), "not a snapshot key"));
        };
        let bytes = self.backend().get(&key.path()).await?;
        let plain = self
            .keys()
            .open_snapshot(client, ts, &bytes)
            .map_err(|e| CoreError::corrupt(key.path(), e.to_string()))?;
        let snap = Snapshot::decode(&plain)?;
        snap.check_key(key)?;
        Ok(snap)
    }

    /// 所有 snapshot 主體的 key，依時間（舊到新），同一時間依 client。不是 v4 key 的物件警告後略過。
    pub async fn list_snapshot_keys(&self) -> Result<Vec<Key>> {
        let mut keys = Vec::new();
        for o in self.backend().list(SNAPSHOTS.trim_end_matches('/')).await? {
            match Key::parse(&o.key) {
                Ok(Key::Snapshot {
                    client,
                    ts,
                    replica: false,
                }) => keys.push((ts.nanos(), client, ts)),
                Ok(Key::Snapshot { replica: true, .. }) => {}
                _ => tracing::warn!("{}: not a v4 snapshot key; ignored", o.key),
            }
        }
        keys.sort_by_key(|(nanos, client, _)| (*nanos, *client));
        Ok(keys
            .into_iter()
            .map(|(_, client, ts)| Key::Snapshot {
                client,
                ts,
                replica: false,
            })
            .collect())
    }

    /// 所有 snapshot（逐一讀取），依時間。
    pub async fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        let mut out = Vec::new();
        for key in self.list_snapshot_keys().await? {
            let snapshot = self.read_snapshot(&key).await?;
            out.push(SnapshotInfo { key, snapshot });
        }
        Ok(out)
    }

    /// 使用者給的 snapshot 指定 → key。接受 `latest`、完整的 key、`<client>/<時間戳>`、`<時間戳>` 或它的前綴
    /// （必須唯一）。
    pub async fn resolve_snapshot(&self, spec: &str) -> Result<Key> {
        let keys = self.list_snapshot_keys().await?;
        if spec == "latest" {
            return keys
                .last()
                .cloned()
                .ok_or_else(|| CoreError::SnapshotNotFound(spec.to_owned()));
        }
        let matches: Vec<&Key> = keys
            .iter()
            .filter(|k| {
                let Key::Snapshot { client, ts, .. } = k else {
                    return false;
                };
                k.path() == spec
                    || format!("{client}/{}", ts.as_str()) == spec
                    || ts.as_str().starts_with(spec)
            })
            .collect();
        match matches.as_slice() {
            [] => Err(CoreError::SnapshotNotFound(spec.to_owned())),
            [k] => Ok((*k).clone()),
            many => Err(CoreError::AmbiguousSnapshot(spec.to_owned(), many.len())),
        }
    }
}
