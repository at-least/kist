//! 打開 / 建立 repo，以及各種物件的讀寫幫手（v2）。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use kist_backend::{Backend, BackendError};
use kist_crypto::{create_key_slot, unlock_key_slot, KdfCost, KeyBinding, RepoKeys};
use kist_format::config::{ChunkerParams, RepoConfig};
use kist_format::index::IndexBlob;
use kist_format::snapshot::{parse_key_timestamp, Snapshot};
use kist_format::tree::{Entry, Tree};
use kist_format::{cbor, keys, Algorithm, ObjectId, TreeId};
use zeroize::Zeroizing;

use crate::index::ChunkIndex;
use crate::{blocking, CoreError, Result};

/// index blob / pack trailer 的壓縮等級（與 chunk 相同）。
const ZSTD_LEVEL: i32 = 3;

/// repo 裡目前所有 index blob 的原貌：prune 需要看每個 pack 完整的 entries（而不是
/// `ChunkIndex` 每個 chunk 只記一個位置），才能判斷重複 chunk 所在的每個 pack 都活著。
#[derive(Debug, Clone, Default)]
pub struct IndexBlobs {
    /// 未被取代的 blob。
    pub effective: Vec<(ObjectId, IndexBlob)>,
    /// 存在但被取代的 blob（GC 的候選）。
    pub superseded: Vec<ObjectId>,
}

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
    /// 本地 index 快取；`None` 表示每次都從 repo 讀全部 index blob。
    cache: Option<crate::cache::IndexCache>,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Repository({:?})", self.backend)
    }
}

/// index blob 的明文 = `algorithm byte ‖ (可能 zstd 過的) CBOR`。
fn encode_index_blob(blob: &IndexBlob) -> Result<Vec<u8>> {
    let plain = cbor::encode(blob)?;
    let compressed =
        zstd::encode_all(plain.as_slice(), ZSTD_LEVEL).map_err(|e| CoreError::Corrupt {
            key: "index".to_owned(),
            reason: format!("zstd failed: {e}"),
        })?;
    let mut out = Vec::with_capacity(1 + compressed.len());
    if compressed.len() < plain.len() - plain.len() / 16 {
        out.push(Algorithm::Zstd as u8);
        out.extend_from_slice(&compressed);
    } else {
        out.push(Algorithm::Raw as u8);
        out.extend_from_slice(&plain);
    }
    Ok(out)
}

/// 解開 index blob 的明文（見 [`encode_index_blob`]）。
fn decode_index_blob(payload: &[u8]) -> Result<IndexBlob> {
    let Some((algorithm, data)) = payload.split_first() else {
        return Err(CoreError::Corrupt {
            key: "index".to_owned(),
            reason: "index blob payload is empty (no algorithm byte)".to_owned(),
        });
    };
    let plain = match Algorithm::from_u8(*algorithm)? {
        Algorithm::Raw => data.to_vec(),
        Algorithm::Zstd => zstd::decode_all(data).map_err(|e| CoreError::Corrupt {
            key: "index".to_owned(),
            reason: format!("zstd decode failed: {e}"),
        })?,
    };
    Ok(cbor::decode(&plain)?)
}

impl Repository {
    /// 建立新 repo：產生 master key、用密碼包起來、寫 `config`。已存在則拒絕。
    pub async fn init(backend: Backend, password: &[u8], opts: InitOptions) -> Result<Self> {
        let created_ns = time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            .try_into()
            .map_err(|_| CoreError::InvalidConfig("clock out of range".to_owned()))?;
        let repo_id = kist_crypto::random_bytes::<16>()?.to_vec();
        let binding = KeyBinding {
            repo_id: repo_id.clone(),
            chunker: opts.chunker,
        };
        let password = Zeroizing::new(password.to_vec());
        let cost = opts.kdf_cost;
        let (slot, master) = blocking(move || {
            Ok(create_key_slot(&password, "default", created_ns, cost, &binding)?)
        })
        .await?;
        let mut config = RepoConfig::new(repo_id, created_ns, slot);
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
            cache: None,
        })
    }

    /// 打開既有 repo：讀 `config`、用密碼解開 master key、派生子金鑰。不用本地快取。
    pub async fn open(backend: Backend, password: &[u8]) -> Result<Self> {
        Self::open_with_cache(backend, password, None).await
    }

    /// 同 [`Self::open`]，並在 `cache_root` 底下維護這個 repo 的本地 index 快取。
    pub async fn open_with_cache(
        backend: Backend,
        password: &[u8],
        cache_root: Option<PathBuf>,
    ) -> Result<Self> {
        let bytes = match backend.get(keys::CONFIG).await {
            Ok(b) => b,
            Err(BackendError::NotFound(_)) => return Err(CoreError::NotARepository),
            Err(e) => return Err(e.into()),
        };
        let config: RepoConfig = cbor::decode(&bytes)?;
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
        let keys = Arc::new(RepoKeys::from_master(&master));
        let cache = cache_root.map(|root| {
            crate::cache::IndexCache::new(&root, &keys.cache_id(), &backend.location().to_string())
        });
        Ok(Self {
            backend,
            config,
            keys,
            cache,
        })
    }

    pub fn cache(&self) -> Option<&crate::cache::IndexCache> {
        self.cache.as_ref()
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

    /// 讀 index blob：名稱 = BLAKE3(密文)，先驗再解。
    pub(crate) async fn read_index_blob(&self, id: &ObjectId) -> Result<IndexBlob> {
        let key = keys::index(id);
        let bytes = self.backend.get(&key).await?;
        let keys = Arc::clone(&self.keys);
        let key_owned = key.clone();
        let expected = *id;
        blocking(move || {
            let actual = ObjectId::of(&bytes);
            if actual != expected {
                return Err(CoreError::Corrupt {
                    key: key_owned,
                    reason: format!("content hash {actual} does not match its name"),
                });
            }
            let payload = keys.open_index_blob(&bytes).map_err(|e| CoreError::Corrupt {
                key: key_owned.clone(),
                reason: e.to_string(),
            })?;
            let blob = decode_index_blob(&payload).map_err(|e| CoreError::Corrupt {
                key: key_owned.clone(),
                reason: e.to_string(),
            })?;
            if blob.version != kist_format::FORMAT_VERSION {
                return Err(CoreError::Corrupt {
                    key: key_owned,
                    reason: format!(
                        "index blob declares version {}, this build reads {}",
                        blob.version,
                        kist_format::FORMAT_VERSION
                    ),
                });
            }
            Ok(blob)
        })
        .await
    }

    /// 讀 tree：AAD = 自己的 ID，解開後重算 keyed hash 對名稱——
    /// 名稱是對**明文**的 hash，這一步證明當初寫入時名稱沒有說謊。
    pub(crate) async fn read_tree(&self, id: &TreeId) -> Result<Tree> {
        let key = keys::tree(id);
        let bytes = self.backend.get(&key).await?;
        let keys = Arc::clone(&self.keys);
        let key_owned = key.clone();
        let expected = *id;
        blocking(move || {
            let plain = keys
                .open_tree(&expected, &bytes)
                .map_err(|e| CoreError::Corrupt {
                    key: key_owned.clone(),
                    reason: e.to_string(),
                })?;
            let actual = keys.tree_id(&plain);
            if actual != expected {
                return Err(CoreError::Corrupt {
                    key: key_owned,
                    reason: format!("content hash {actual} does not match its name"),
                });
            }
            let tree: Tree = cbor::decode(&plain).map_err(|e| CoreError::Corrupt {
                key: key_owned.clone(),
                reason: e.to_string(),
            })?;
            if tree.version != kist_format::FORMAT_VERSION {
                return Err(CoreError::Corrupt {
                    key: key_owned,
                    reason: format!(
                        "tree declares version {}, this build reads {}",
                        tree.version,
                        kist_format::FORMAT_VERSION
                    ),
                });
            }
            Ok(tree)
        })
        .await
    }

    /// 把 tree 編成規範 CBOR、以明文 keyed hash 命名、隨機 nonce 密封。
    /// 回傳（名稱, bytes)；不上傳。
    pub(crate) async fn seal_tree(&self, tree: Tree) -> Result<(TreeId, Vec<u8>)> {
        let keys = Arc::clone(&self.keys);
        blocking(move || {
            let plain = cbor::encode(&tree)?;
            let id = keys.tree_id(&plain);
            let bytes = keys.seal_tree(&id, &plain)?;
            Ok((id, bytes))
        })
        .await
    }

    /// 沿 `prev` 收集一個目錄的所有段，回傳依名稱排序的完整節點清單。
    pub(crate) async fn read_tree_chain(&self, last: &TreeId) -> Result<Vec<Entry>> {
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
            parts.push(tree.entries);
        }
        parts.reverse();
        Ok(parts.into_iter().flatten().collect())
    }

    /// 寫一個 index blob。一般 backup 會自己呼叫；公開給維護工具（rebuild-index、repack）用。
    pub async fn write_index(&self, blob: IndexBlob) -> Result<ObjectId> {
        let keys = Arc::clone(&self.keys);
        let (id, bytes) = blocking(move || {
            let payload = encode_index_blob(&blob)?;
            let bytes = keys.seal_index_blob(&payload)?;
            Ok((ObjectId::of(&bytes), bytes))
        })
        .await?;
        self.backend.put(&keys::index(&id), bytes).await?;
        Ok(id)
    }

    /// backup / restore 用的 index：有本地快取就用快取（只讀新 blob），否則讀全部 blob。
    /// 任何一個 blob 壞掉就整個失敗（`check` 有寬鬆版本）。
    pub async fn load_index(&self) -> Result<ChunkIndex> {
        if let Some(cache) = &self.cache {
            let mut live = Vec::new();
            for o in self.backend.list(keys::INDEXES_PREFIX).await? {
                live.push(keys::object_id_from_key(&o.key)?);
            }
            live.sort();
            return cache
                .load(&live, |id| async move { self.read_index_blob(&id).await })
                .await
                .map_err(|e| CoreError::Corrupt {
                    key: "index".to_owned(),
                    reason: format!("{e}; run `kist rebuild-index`"),
                });
        }
        let mut errors = Vec::new();
        let index = self.load_index_lenient(&mut errors).await?;
        if let Some(first) = errors.into_iter().next() {
            return Err(CoreError::Corrupt {
                key: "index".to_owned(),
                reason: format!("{first}; run `kist rebuild-index`"),
            });
        }
        Ok(index)
    }

    /// 讀進所有 index blob（不用快取），壞掉的記在 `errors` 裡繼續。
    /// 回傳的 `effective` 已排除被 `supersedes` 列到的 blob。
    pub async fn load_index_blobs(&self, errors: &mut Vec<CoreError>) -> Result<IndexBlobs> {
        let mut all = Vec::new();
        for o in self.backend.list(keys::INDEXES_PREFIX).await? {
            let id = match keys::object_id_from_key(&o.key) {
                Ok(id) => id,
                Err(e) => {
                    errors.push(e.into());
                    continue;
                }
            };
            match self.read_index_blob(&id).await {
                Ok(blob) => all.push((id, blob)),
                Err(e) => errors.push(e),
            }
        }
        let superseded: HashSet<ObjectId> = all
            .iter()
            .flat_map(|(_, b)| b.supersedes.iter().copied())
            .collect();
        let (dropped, effective): (Vec<_>, Vec<_>) =
            all.into_iter().partition(|(id, _)| superseded.contains(id));
        Ok(IndexBlobs {
            effective,
            superseded: dropped.into_iter().map(|(id, _)| id).collect(),
        })
    }

    /// 讀進所有 index blob 合成一個 `ChunkIndex`，壞掉的記在 `errors` 裡繼續。
    /// 被其他 blob 的 `supersedes` 列到的 blob 整個忽略（repack 之後新舊並存時以新的為準）。
    pub(crate) async fn load_index_lenient(
        &self,
        errors: &mut Vec<CoreError>,
    ) -> Result<ChunkIndex> {
        let blobs = self.load_index_blobs(errors).await?;
        let mut index = ChunkIndex::new();
        for (_, blob) in &blobs.effective {
            for pack in &blob.packs {
                index.add_pack(pack);
            }
        }
        Ok(index)
    }

    /// backup 專用的 index：讀進所有有效 index blob，合併規則是
    /// **未標記 pack 優先、其次名稱最小**——與 prune 的正本選擇同一個
    /// rank（規格 §10、§13.1）。沒有這條規則，smallest-wins 會把 chunk
    /// 導向被標記的舊副本，backup 在 prune 收掉它之前每次都重傳。
    /// 不走本地快取：快取的位置是歷史選擇，不知道標記集合。
    pub(crate) async fn load_index_for_backup(
        &self,
        marks: &std::collections::HashSet<ObjectId>,
    ) -> Result<ChunkIndex> {
        let mut errors = Vec::new();
        let blobs = self.load_index_blobs(&mut errors).await?;
        if let Some(first) = errors.into_iter().next() {
            return Err(CoreError::Corrupt {
                key: "index".to_owned(),
                reason: format!("{first}; run `kist rebuild-index`"),
            });
        }
        let mut index = ChunkIndex::new();
        for (_, blob) in &blobs.effective {
            for pack in &blob.packs {
                index.add_pack_ranked(pack, |id| marks.contains(id));
            }
        }
        Ok(index)
    }

    pub(crate) async fn write_snapshot(&self, key: &str, snapshot: Snapshot) -> Result<()> {
        let keys = Arc::clone(&self.keys);
        let key_owned = key.to_owned();
        let bytes = blocking(move || {
            let plain = cbor::encode(&snapshot)?;
            Ok(keys.seal_snapshot(&key_owned, &plain)?)
        })
        .await?;
        self.backend.put_if_absent(key, bytes).await?;
        Ok(())
    }

    /// 讀 snapshot（AAD = 完整 key），並驗證內容與 key 一致（client id、時間戳）。
    pub(crate) async fn read_snapshot(&self, key: &str) -> Result<Snapshot> {
        let bytes = match self.backend.get(key).await {
            Ok(b) => b,
            Err(BackendError::NotFound(_)) => return Err(CoreError::SnapshotNotFound(key.to_owned())),
            Err(e) => return Err(e.into()),
        };
        let keys = Arc::clone(&self.keys);
        let key_owned = key.to_owned();
        let snapshot: Snapshot = blocking(move || {
            let plain = keys
                .open_snapshot(&key_owned, &bytes)
                .map_err(|e| CoreError::Corrupt {
                    key: key_owned.clone(),
                    reason: e.to_string(),
                })?;
            let snap: Snapshot = cbor::decode(&plain).map_err(|e| CoreError::Corrupt {
                key: key_owned.clone(),
                reason: e.to_string(),
            })?;
            if snap.version != kist_format::FORMAT_VERSION {
                return Err(CoreError::Corrupt {
                    key: key_owned,
                    reason: format!(
                        "snapshot declares version {}, this build reads {}",
                        snap.version,
                        kist_format::FORMAT_VERSION
                    ),
                });
            }
            Ok(snap)
        })
        .await?;
        // snapshot 不是以內容命名：用內容裡的 client 與時間反算 key，必須一致。
        let ts = key.rsplit('/').next()
            .ok_or_else(|| CoreError::Corrupt {
                key: key.to_owned(),
                reason: "not a snapshot key".to_owned(),
            })?;
        let t = parse_key_timestamp(ts)?;
        let expected_ns: i64 = t.unix_timestamp_nanos().try_into().map_err(|_| {
            CoreError::Corrupt {
                key: key.to_owned(),
                reason: "timestamp out of range".to_owned(),
            }
        })?;
        if snapshot.client_id.len() != 16
            || keys::snapshot(&snapshot.client_id, ts) != key
            || snapshot.time_ns != expected_ns
        {
            return Err(CoreError::Corrupt {
                key: key.to_owned(),
                reason: "snapshot content does not belong under this key".to_owned(),
            });
        }
        Ok(snapshot)
    }
}
