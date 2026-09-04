//! 打開 / 建立 repo，以及各種物件的讀寫幫手。

use std::collections::HashSet;
use std::sync::Arc;

use kist_backend::{Backend, BackendError};
use kist_crypto::{create_key_slot, unlock_key_slot, KdfCost, KeyBinding, RepoKeys};
use kist_format::config::{ChunkerParams, RepoConfig};
use kist_format::envelope::{Compression, ObjectKind};
use kist_format::index::IndexBlob;
use kist_format::snapshot::{format_key_timestamp, format_rfc3339, Snapshot};
use kist_format::tree::{Node, Tree};
use kist_format::{cbor, keys, ObjectId};
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use crate::index::ChunkIndex;
use crate::{blocking, CoreError, Result};

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub chunker: ChunkerParams,
    pub pack_target_size: u64,
    pub kdf_cost: KdfCost,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            chunker: ChunkerParams::default(),
            pack_target_size: 64 * 1024 * 1024,
            kdf_cost: KdfCost::default(),
        }
    }
}

#[derive(Clone)]
pub struct Repository {
    backend: Backend,
    config: RepoConfig,
    keys: Arc<RepoKeys>,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Repository({:?})", self.backend)
    }
}

impl Repository {
    /// 建立新 repo：產生 master key、用密碼包起來、寫 `config`。已存在則拒絕。
    pub async fn init(backend: Backend, password: &[u8], opts: InitOptions) -> Result<Self> {
        let now = format_rfc3339(time::OffsetDateTime::now_utc())?;
        let repo_id = kist_crypto::random_bytes::<16>()?.to_vec();
        let binding = KeyBinding {
            repo_id: repo_id.clone(),
            chunker: opts.chunker,
        };
        let password = Zeroizing::new(password.to_vec());
        let created = now.clone();
        let cost = opts.kdf_cost;
        let (slot, master) = blocking(move || {
            Ok(create_key_slot(
                &password, "default", &created, cost, &binding,
            )?)
        })
        .await?;
        let mut config = RepoConfig::new(repo_id, now, slot);
        config.chunker = opts.chunker;
        config.pack_target_size = opts.pack_target_size;
        config
            .validate()
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;

        let bytes = cbor::encode(&config)?;
        match backend.put_if_absent(keys::CONFIG, bytes).await {
            Ok(()) => {}
            Err(BackendError::AlreadyExists(_)) => return Err(CoreError::RepoExists),
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            backend,
            config,
            keys: Arc::new(RepoKeys::from_master(&master)),
        })
    }

    /// 打開既有 repo：讀 `config`、用密碼解開 master key、派生子金鑰。
    pub async fn open(backend: Backend, password: &[u8]) -> Result<Self> {
        let bytes = match backend.get(keys::CONFIG).await {
            Ok(b) => b,
            Err(BackendError::NotFound(_)) => return Err(CoreError::NotARepository),
            Err(e) => return Err(e.into()),
        };
        let config: RepoConfig = cbor::decode(&bytes)?;
        if config.version != kist_format::FORMAT_VERSION {
            return Err(kist_format::FormatError::UnsupportedVersion {
                what: "repository config",
                version: config.version,
            }
            .into());
        }
        // config 是明文：先確認參數合理，再用它們（綁在 AAD 裡）解 master key
        config
            .validate()
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;
        let binding = KeyBinding {
            repo_id: config.repo_id.clone(),
            chunker: config.chunker,
        };
        let password = Zeroizing::new(password.to_vec());
        let slot = config.key.clone();
        let master = blocking(move || Ok(unlock_key_slot(&password, &slot, &binding)?)).await?;
        Ok(Self {
            backend,
            config,
            keys: Arc::new(RepoKeys::from_master(&master)),
        })
    }

    pub fn config(&self) -> &RepoConfig {
        &self.config
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn keys(&self) -> &Arc<RepoKeys> {
        &self.keys
    }

    /// 讀一個 envelope 物件並解出 CBOR。
    ///
    /// 以內容命名的物件（`trees/*`、`indexes/*`）會先驗證「名稱 = BLAKE3(bytes)」：
    /// AAD 只綁物件種類，沒綁名稱，若有人把 tree A 的檔案複製到 tree B 的名稱上，
    /// 解密照樣成功；只有這一步能抓到。
    pub(crate) async fn read_object<T: DeserializeOwned + Send + 'static>(
        &self,
        kind: ObjectKind,
        key: &str,
    ) -> Result<T> {
        let bytes = self.backend.get(key).await?;
        let keys = Arc::clone(&self.keys);
        let key_owned = key.to_owned();
        let expected_name = if matches!(kind, ObjectKind::Tree | ObjectKind::Index) {
            Some(keys::object_id_from_key(key)?)
        } else {
            None
        };
        blocking(move || {
            if let Some(expected) = expected_name {
                let actual = ObjectId::of(&bytes);
                if actual != expected {
                    return Err(CoreError::Corrupt {
                        key: key_owned,
                        reason: format!("content hash {actual} does not match its name"),
                    });
                }
            }
            let plain = keys
                .open_object(kind, &bytes)
                .map_err(|e| CoreError::Corrupt {
                    key: key_owned.clone(),
                    reason: e.to_string(),
                })?;
            cbor::decode(&plain).map_err(|e| CoreError::Corrupt {
                key: key_owned,
                reason: e.to_string(),
            })
        })
        .await
    }

    /// 把 tree 封裝成 bytes（決定性），回傳名稱與 bytes；不上傳。
    pub(crate) async fn seal_tree(&self, tree: Tree) -> Result<(ObjectId, Vec<u8>)> {
        let keys = Arc::clone(&self.keys);
        blocking(move || {
            let plain = cbor::encode(&tree)?;
            let bytes = keys.seal_object(ObjectKind::Tree, Compression::Zstd, &plain)?;
            Ok((ObjectId::of(&bytes), bytes))
        })
        .await
    }

    pub(crate) async fn read_tree(&self, id: &ObjectId) -> Result<Tree> {
        self.read_object(ObjectKind::Tree, &keys::tree(id)).await
    }

    /// 沿 `prev` 收集一個目錄的所有段，回傳依名稱排序的完整節點清單。
    pub(crate) async fn read_tree_chain(&self, last: &ObjectId) -> Result<Vec<Node>> {
        let mut parts = Vec::new();
        let mut next = Some(*last);
        let mut seen = HashSet::new();
        while let Some(id) = next {
            if !seen.insert(id) {
                return Err(CoreError::Corrupt {
                    key: keys::tree(&id),
                    reason: "tree chain loops".to_owned(),
                });
            }
            let tree = self.read_tree(&id).await?;
            next = tree.prev;
            parts.push(tree.nodes);
        }
        parts.reverse();
        Ok(parts.into_iter().flatten().collect())
    }

    pub(crate) async fn write_index(&self, blob: IndexBlob) -> Result<ObjectId> {
        let keys = Arc::clone(&self.keys);
        let (id, bytes) = blocking(move || {
            let plain = cbor::encode(&blob)?;
            let bytes = keys.seal_object(ObjectKind::Index, Compression::Zstd, &plain)?;
            Ok((ObjectId::of(&bytes), bytes))
        })
        .await?;
        self.backend.put(&keys::index(&id), bytes).await?;
        Ok(id)
    }

    /// 讀進所有 index blob。任何一個壞掉就整個失敗（`check` 有寬鬆版本）。
    pub async fn load_index(&self) -> Result<ChunkIndex> {
        let mut errors = Vec::new();
        let index = self.load_index_lenient(&mut errors).await?;
        if let Some(first) = errors.into_iter().next() {
            return Err(first);
        }
        Ok(index)
    }

    /// 讀進所有 index blob，壞掉的記在 `errors` 裡繼續。
    pub(crate) async fn load_index_lenient(
        &self,
        errors: &mut Vec<CoreError>,
    ) -> Result<ChunkIndex> {
        let mut index = ChunkIndex::new();
        for (key, _) in self.backend.list(keys::INDEXES_PREFIX).await? {
            match self.read_object::<IndexBlob>(ObjectKind::Index, &key).await {
                Ok(blob) => {
                    for pack in &blob.packs {
                        index.add_pack(pack);
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        Ok(index)
    }

    pub(crate) async fn write_snapshot(&self, key: &str, snapshot: Snapshot) -> Result<()> {
        let keys = Arc::clone(&self.keys);
        let bytes = blocking(move || {
            let plain = cbor::encode(&snapshot)?;
            Ok(keys.seal_object(ObjectKind::Snapshot, Compression::Zstd, &plain)?)
        })
        .await?;
        self.backend.put_if_absent(key, bytes).await?;
        Ok(())
    }

    /// 讀 snapshot，並驗證內容與 key 一致（client id、時間戳）：
    /// snapshot 不是以內容命名，所以用內容裡的欄位反過來對 key。
    pub(crate) async fn read_snapshot(&self, key: &str) -> Result<Snapshot> {
        let snapshot: Snapshot = match self.read_object(ObjectKind::Snapshot, key).await {
            Err(CoreError::Backend(BackendError::NotFound(_))) => {
                return Err(CoreError::SnapshotNotFound(key.to_owned()))
            }
            other => other?,
        };
        let expected_key = {
            let t = time::OffsetDateTime::parse(
                &snapshot.time,
                &time::format_description::well_known::Rfc3339,
            )
            .map_err(|e| CoreError::Corrupt {
                key: key.to_owned(),
                reason: format!("bad time {:?}: {e}", snapshot.time),
            })?;
            keys::snapshot(&snapshot.client_id, &format_key_timestamp(t)?)
        };
        if expected_key != key {
            return Err(CoreError::Corrupt {
                key: key.to_owned(),
                reason: format!("snapshot content belongs to {expected_key}"),
            });
        }
        Ok(snapshot)
    }
}
