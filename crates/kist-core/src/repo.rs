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
    /// trees/snapshots 的 `.r1` 副本數（0 或 1）。`None` = 依後端決定
    /// （本機 = 1、遠端 = 0；format-v3-draft §11）。
    pub replicas: Option<u8>,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            chunker: ChunkerParams::default(),
            pack_target_size: 64 * 1024 * 1024,
            kdf_cost: KdfCost::default(),
            replicas: None,
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

/// 計數 writer：zstd 的 `write::Encoder` 沒有 total_in，明文長度自己數
///（用來判斷壓縮是否省下 ≥ 1/16）。
struct CountingWriter<W> {
    inner: W,
    written: u64,
}

impl<W: std::io::Write> std::io::Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// index blob 的明文 = `algorithm byte ‖ (可能 zstd 過的) CBOR`。
/// CBOR **直接串流進 zstd 編碼器**：100 萬 chunk 的明文 ≈ 56 MiB，
/// 不先物化成 Vec 再壓縮（那會多一份同樣大的暫存，疊在 backup 收尾的峰值上）。
fn encode_index_blob(blob: &IndexBlob) -> Result<Vec<u8>> {
    let mut enc = zstd::stream::write::Encoder::new(
        CountingWriter {
            inner: Vec::new(),
            written: 0,
        },
        ZSTD_LEVEL,
    )
    .map_err(|e| CoreError::Corrupt {
        key: "index".to_owned(),
        reason: format!("zstd failed: {e}"),
    })?;
    cbor::encode_to_writer(blob, &mut enc)?;
    let compressed_writer = enc.finish().map_err(|e| CoreError::Corrupt {
        key: "index".to_owned(),
        reason: format!("zstd failed: {e}"),
    })?;
    let plain_len = usize::try_from(compressed_writer.written).unwrap_or(usize::MAX);
    let mut compressed = compressed_writer.inner;
    if compressed.len() < plain_len - plain_len / 16 {
        compressed.reserve(1);
        compressed.insert(0, Algorithm::Zstd as u8);
        Ok(compressed)
    } else {
        // 幾乎不會走到（index 內容重複性高）。真發生時退回原文，
        // 規格行為不變，代價是重新物化一次明文。
        let mut out = Vec::with_capacity(1 + plain_len);
        out.push(Algorithm::Raw as u8);
        let plain = cbor::encode(blob)?;
        out.extend_from_slice(&plain);
        Ok(out)
    }
}

/// index blob 解壓上限（與 Go 端同一數字）：一個 blob 描述 repo 裡每個
/// pack，真實 blob 遠遠不到，壞掉的長度欄位不該能要到 GiB 級記憶體。
const MAX_INDEX_PLAIN: u64 = 1 << 30;

/// 解開 index blob 的明文（見 [`encode_index_blob`]）。
fn decode_index_blob(payload: &[u8]) -> Result<IndexBlob> {
    decode_index_blob_limited(payload, MAX_INDEX_PLAIN)
}

/// 同上，上限由呼叫端給（測試用小上限釘行為；正式路徑恆為
/// [`MAX_INDEX_PLAIN`]）。串流解到上限+1 為止，超過即 Corrupt。
fn decode_index_blob_limited(payload: &[u8], limit: u64) -> Result<IndexBlob> {
    let Some((algorithm, data)) = payload.split_first() else {
        return Err(CoreError::Corrupt {
            key: "index".to_owned(),
            reason: "index blob payload is empty (no algorithm byte)".to_owned(),
        });
    };
    let plain = match Algorithm::from_u8(*algorithm)? {
        Algorithm::Raw => data.to_vec(),
        Algorithm::Zstd => {
            use std::io::Read;
            let decoder =
                zstd::stream::read::Decoder::with_buffer(data).map_err(|e| CoreError::Corrupt {
                    key: "index".to_owned(),
                    reason: format!("zstd decode failed: {e}"),
                })?;
            let mut out = Vec::new();
            decoder
                .take(limit.saturating_add(1))
                .read_to_end(&mut out)
                .map_err(|e| CoreError::Corrupt {
                    key: "index".to_owned(),
                    reason: format!("zstd decode failed: {e}"),
                })?;
            if out.len() as u64 > limit {
                return Err(CoreError::Corrupt {
                    key: "index".to_owned(),
                    reason: format!(
                        "index blob decompresses to {} bytes, over the {} limit",
                        out.len(),
                        limit
                    ),
                });
            }
            out
        }
    };
    Ok(cbor::decode(&plain)?)
}

impl Repository {
    /// 建立新 repo：產生 master key、用密碼包起來、寫 `config`。已存在則拒絕。
    pub async fn init(backend: Backend, password: &[u8], opts: InitOptions) -> Result<Self> {
        let created_ns = time::OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
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
            Ok(create_key_slot(
                &password, "default", created_ns, cost, &binding,
            )?)
        })
        .await?;
        let mut config = RepoConfig::new(repo_id, created_ns, slot);
        config.chunker = opts.chunker;
        config.pack_target_size = opts.pack_target_size;
        // v3 副本預設（format-v3-draft §11）：本機後端 = 1（單碟無冗餘，
        // trees/snapshots 是不可重建的 metadata）；S3/SFTP/rclone = 0
        //（後端已有冗餘，或頻寬成本）。InitOptions.replicas 可覆寫。
        config.replicas = match opts.replicas {
            Some(n) => n,
            None => match backend.location() {
                kist_backend::RepoLocation::Local(_) => 1,
                _ => 0,
            },
        };
        config
            .validate()
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;

        let bytes = cbor::encode(&config)?;
        // 讀回驗證：寬鬆後端（rclone://）上 put_if_absent 沒有原子守門，兩個 init
        // 同時跑可能互相覆蓋 config。讀回比對攔下大多數交錯（讀回看到對方的內容
        // 就回 ConcurrentWrite），但不是鎖——對方的 rename 若落在自己讀回之後，
        // 兩邊都會回報成功。嚴格後端上這次讀取只是便宜的確認（hardlink 守門本來
        // 就保證讀回一致）。
        match backend.put_if_absent_verified(keys::CONFIG, bytes).await {
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
        // config 是明文：先確認參數合理，再解 master key（v3：不變式在認證
        // 密文裡，解開後回頭比對）。
        config
            .validate()
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;
        // min_reader 閘門：repo 要求的最低版本高於本 build → 明確拒絕，
        // 不是靠忽略未知欄位半讀（format-v3-draft §11）。
        if u32::from(config.min_reader) > kist_format::FORMAT_VERSION {
            return Err(CoreError::InvalidConfig(format!(
                "repository requires a reader of format v{} or newer; this build reads v{}",
                config.min_reader,
                kist_format::FORMAT_VERSION
            )));
        }
        let password = Zeroizing::new(password.to_vec());
        let slot = config.key.clone();
        let unlocked = blocking(move || Ok(unlock_key_slot(&password, &slot)?)).await?;
        unlocked
            .invariants
            .validate()
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;
        unlocked
            .invariants
            .check_matches(&config)
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;
        let keys = Arc::new(RepoKeys::from_master(&unlocked.master));
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
            let payload = keys
                .open_index_blob(&bytes)
                .map_err(|e| CoreError::Corrupt {
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
    /// 主體讀不到或驗不過時嘗試 `.r1` 副本（同 bytes；v3 §13.5）。
    pub(crate) async fn read_tree(&self, id: &TreeId) -> Result<Tree> {
        match self.read_tree_once(&keys::tree(id), id).await {
            Ok(t) => Ok(t),
            Err(e @ CoreError::Backend(BackendError::NotFound(_)))
            | Err(e @ CoreError::Corrupt { .. }) => {
                let replica = keys::tree_replica(id);
                match self.read_tree_once(&replica, id).await {
                    Ok(t) => {
                        tracing::warn!(
                            "tree {id} read from its .r1 replica (primary missing or corrupt)"
                        );
                        Ok(t)
                    }
                    Err(_) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// 讀單一 key 的 tree 並完整驗證（AAD、名稱 hash、版本、結構）。
    /// backup 的自我修復用它判斷既有的 bytes 是否還是好的。
    pub(crate) async fn read_tree_once(&self, key: &str, id: &TreeId) -> Result<Tree> {
        let bytes = self.backend.get(key).await?;
        let keys = Arc::clone(&self.keys);
        let key_owned = key.to_owned();
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
            tree.validate().map_err(|e| CoreError::Corrupt {
                key: keys::tree(&expected),
                reason: e.to_string(),
            })?;
            Ok(tree)
        })
        .await
    }

    /// 樹的復活訊號：覆寫式 Put 固定 8 bytes——**後端 mtime 的刷新就是訊號**。
    /// 內容固定，覆寫無害（v2 的 backup 本來就覆寫整棵樹的 bytes；v3 把
    /// 覆寫面縮到這一種物件，format-v3-draft §13.1）。
    pub(crate) async fn touch_tree(&self, id: &TreeId) -> Result<()> {
        self.backend
            .put(&keys::touch(id), keys::TOUCH_MAGIC.to_vec())
            .await?;
        Ok(())
    }

    /// 把 tree 編成規範 CBOR、以明文 keyed hash 命名、隨機 nonce 密封。
    /// 回傳（名稱, bytes)；不上傳。pub：kist-mount 的測試要手工組 snapshot。
    /// 呼叫端必須把 bytes 存到 `keys::tree(&id)` 這個 key，內容才找得回來。
    pub async fn seal_tree(&self, tree: Tree) -> Result<(TreeId, Vec<u8>)> {
        let keys = Arc::clone(&self.keys);
        blocking(move || {
            // 寫入端跑與讀取端同一套結構不變式（§8.1）：來源端冒出的 `..`／
            // 重複／未排序名稱在這裡當場失敗，不寫出所有讀取端都拒讀的 tree
            // （Go 的 Encode 同款）。
            tree.validate()?;
            let plain = cbor::encode(&tree)?;
            let id = keys.tree_id(&plain);
            let bytes = keys.seal_tree(&id, &plain)?;
            Ok((id, bytes))
        })
        .await
    }

    /// 沿 `prev` 收集一個目錄的所有段，回傳依名稱排序的完整節點清單。
    /// 公開給 `kist-mount`（FUSE 掛載逐目錄載入）。
    pub async fn read_tree_chain(&self, last: &TreeId) -> Result<Vec<Entry>> {
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
            let mut payload = encode_index_blob(&blob)?;
            // 就地加密把 tag 附加在後：先預留空間，避免附加時整份重配
            payload.reserve(kist_format::pack::TAG_LEN);
            let bytes = keys.seal_index_blob_in_place(payload)?;
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

    /// 寫 snapshot 物件。pub：kist-mount 的測試要手工組 snapshot（一般流程用
    /// `backup`，它會先 prepare/commit，不會直接呼叫這個）。
    /// **不檢查 key 與內容一致**——`read_snapshot` 讀取時會核對（client、時間戳），
    /// 呼叫端必須自己保證 `keys::snapshot(&client_id, ts)` 與內容相符。
    /// `replicas=1` 時 `.r1` 副本**先寫**：主體出現＝commit，副本先行不會
    /// 造成假 commit（format-v3-draft §13.5）。
    pub async fn write_snapshot(&self, key: &str, snapshot: Snapshot) -> Result<()> {
        let keys = Arc::clone(&self.keys);
        let key_owned = key.to_owned();
        let bytes = blocking(move || {
            let plain = cbor::encode(&snapshot)?;
            Ok(keys.seal_snapshot(&key_owned, &plain)?)
        })
        .await?;
        if self.config.replicas > 0 {
            let replica_key = format!("{key}{}", kist_format::keys::REPLICA_SUFFIX);
            let replica_bytes = bytes.clone();
            match self
                .backend
                .put_if_absent(&replica_key, replica_bytes)
                .await
            {
                Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.backend.put_if_absent(key, bytes).await?;
        Ok(())
    }

    /// 讀 snapshot（AAD = 完整 key），並驗證內容與 key 一致（client id、時間戳）。
    /// 主體讀不到時嘗試 `.r1` 副本。
    pub(crate) async fn read_snapshot(&self, key: &str) -> Result<Snapshot> {
        let bytes = match self.backend.get(key).await {
            Ok(b) => b,
            Err(BackendError::NotFound(_)) => {
                let replica = format!("{key}{}", kist_format::keys::REPLICA_SUFFIX);
                match self.backend.get(&replica).await {
                    Ok(b) => {
                        tracing::warn!(
                            "snapshot {key} read from its .r1 replica (primary missing)"
                        );
                        b
                    }
                    Err(_) => return Err(CoreError::SnapshotNotFound(key.to_owned())),
                }
            }
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
                    key: key_owned.clone(),
                    reason: format!(
                        "snapshot declares version {}, this build reads {}",
                        snap.version,
                        kist_format::FORMAT_VERSION
                    ),
                });
            }
            // 結構驗證與 Go 的 load 同款（roots 非空、排序、唯一）：
            // 解得開不等於合法，structurally-invalid 的 snapshot 不能往下走。
            snap.validate().map_err(|e| CoreError::Corrupt {
                key: key_owned.clone(),
                reason: e.to_string(),
            })?;
            Ok(snap)
        })
        .await?;
        // snapshot 不是以內容命名：用內容裡的 client 與時間反算 key，必須一致。
        let ts = key.rsplit('/').next().ok_or_else(|| CoreError::Corrupt {
            key: key.to_owned(),
            reason: "not a snapshot key".to_owned(),
        })?;
        let t = parse_key_timestamp(ts)?;
        let expected_ns: i64 =
            t.unix_timestamp_nanos()
                .try_into()
                .map_err(|_| CoreError::Corrupt {
                    key: key.to_owned(),
                    reason: "timestamp out of range".to_owned(),
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
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod index_cap_tests {
    use crate::repo::decode_index_blob_limited;
    use kist_format::Algorithm;

    /// index blob 的解壓上限（Go 端 1 GiB 同款）：超過上限的 frame 是
    /// 炸彈不是 blob。正式上限太大，測試以小上限釘同一條規則。
    #[test]
    fn index_blob_over_the_limit_is_refused() {
        let bomb = {
            let mut out = vec![Algorithm::Zstd as u8];
            out.extend_from_slice(&zstd::encode_all(&vec![0u8; 4 << 20][..], 3).unwrap());
            out
        };
        let err = match decode_index_blob_limited(&bomb, 1 << 20) {
            Err(e) => e,
            Ok(_) => panic!("超過上限的 index blob 必須被拒"),
        };
        assert!(matches!(err, crate::CoreError::Corrupt { .. }), "{err:?}");
        // 上限之內照常解（會因內容不是合法 IndexBlob 而錯，但不是上限錯）。
        let ok = {
            let mut out = vec![Algorithm::Zstd as u8];
            out.extend_from_slice(&zstd::encode_all(&vec![0u8; 1 << 20][..], 3).unwrap());
            out
        };
        if let Err(e) = decode_index_blob_limited(&ok, 4 << 20) {
            assert!(!format!("{e:?}").contains("limit"), "不該是上限錯誤：{e:?}");
        }
    }
}
