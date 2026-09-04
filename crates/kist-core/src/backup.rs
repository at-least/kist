//! backup：走訪目錄、切塊、去重、寫 pack / tree / index / snapshot。
//!
//! 流程（寫入順序是刻意的，見 `docs/format.md` §11）：
//! 1. 讀進所有 index、列出已存在的 tree（避免重複上傳）。
//! 2. 找同一台 client、同一組路徑的上一個 snapshot 當 parent：
//!    檔案的 size 與 mtime 沒變就直接沿用它的 chunk 清單，不重讀檔案。
//! 3. 依名稱排序遞迴走訪。檔案在 blocking thread 裡串流切塊、算 ID、對 index 去重、
//!    新 chunk 壓縮加密進 pack；pack 滿了就交回 async 端上傳（最多 2 個同時在飛）。
//! 4. 每個目錄結束時封成 tree（決定性加密），名稱沒見過才上傳。
//! 5. 全部結束：flush 最後一個 pack、等上傳完成、寫 index blob、最後寫 snapshot。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kist_backend::{Backend, BackendError};
use kist_chunker::Chunker;
use kist_crypto::RepoKeys;
use kist_format::index::{IndexBlob, IndexPack};
use kist_format::snapshot::{format_key_timestamp, format_rfc3339, Snapshot, SnapshotStats};
use kist_format::tree::{
    ChunkList, Content, Node, NodeKind, NodeMeta, Tree, MAX_INLINE_CHUNKS, MAX_NODES_PER_TREE,
};
use kist_format::{cbor, keys, ChunkId, ObjectId};
use tokio::task::JoinSet;

use crate::fsmeta;
use crate::index::ChunkIndex;
use crate::pack::{FinishedPack, PackWriter};
use crate::repo::Repository;
use crate::{blocking, CoreError, Result};

/// 同時在上傳中的 pack 上限（每個最多 pack_target_size bytes 的記憶體）。
const MAX_INFLIGHT_UPLOADS: usize = 2;

#[derive(Debug, Clone)]
pub struct BackupOptions {
    pub client_id: [u8; 16],
    pub hostname: String,
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub snapshot_key: String,
    pub parent: Option<String>,
    pub root: ObjectId,
    pub stats: SnapshotStats,
}

/// 一個檔案切塊後的結果（從 blocking thread 回傳）。
struct FileResult {
    chunks: Vec<ChunkId>,
    bytes_total: u64,
    bytes_new: u64,
    chunks_new: u64,
    finished: Vec<FinishedPack>,
}

struct Backup {
    repo: Repository,
    chunker: Chunker,
    keys: Arc<RepoKeys>,
    /// 走訪期間會被移進 blocking closure 再移回來，所以用 Option。
    packer: Option<PackWriter>,
    index: Option<ChunkIndex>,
    known_trees: HashSet<ObjectId>,
    new_packs: Vec<IndexPack>,
    uploads: JoinSet<Result<()>>,
    stats: SnapshotStats,
}

impl Repository {
    pub async fn backup(&self, paths: &[PathBuf], opts: BackupOptions) -> Result<BackupSummary> {
        let mut abs_paths = Vec::new();
        for p in paths {
            let abs = std::fs::canonicalize(p).map_err(|e| CoreError::io(p, e))?;
            abs_paths.push(abs);
        }
        abs_paths.sort();
        abs_paths.dedup();
        let mut path_bytes = Vec::new();
        for p in &abs_paths {
            path_bytes.push(fsmeta::path_to_bytes(p)?);
        }

        let index = self.load_index().await?;
        let known_trees: HashSet<ObjectId> = self
            .backend()
            .list(keys::TREES_PREFIX)
            .await?
            .into_iter()
            .filter_map(|(k, _)| keys::object_id_from_key(&k).ok())
            .collect();

        let parent = self.find_parent(&opts.client_id, &path_bytes).await?;
        let parent_nodes = match &parent {
            Some((_, snap)) => self.read_tree_chain(&snap.root).await.unwrap_or_default(),
            None => Vec::new(),
        };
        let parent_map = nodes_by_name(parent_nodes);

        let mut b = Backup {
            repo: self.clone(),
            chunker: Chunker::new(self.config().chunker),
            keys: Arc::clone(self.keys()),
            packer: Some(PackWriter::new(
                Arc::clone(self.keys()),
                self.config().pack_target_size,
            )),
            index: Some(index),
            known_trees,
            new_packs: Vec::new(),
            uploads: JoinSet::new(),
            stats: SnapshotStats::default(),
        };

        // 根 tree：每個來源路徑一個節點，名稱是絕對路徑。
        let mut root_nodes = Vec::new();
        for (path, name) in abs_paths.iter().zip(path_bytes.iter()) {
            let meta = std::fs::symlink_metadata(path).map_err(|e| CoreError::io(path, e))?;
            let node = b
                .process_entry(path, name.clone(), &meta, parent_map.get(name))
                .await?;
            if let Some(node) = node {
                root_nodes.push(node);
            }
        }
        let root = b.write_tree_parts(root_nodes).await?;

        // flush 最後一個 pack，等所有上傳完成
        b.flush_pack().await?;
        b.wait_uploads(0).await?;

        // index blob（只包含這次新寫的 pack）
        if !b.new_packs.is_empty() {
            let packs = std::mem::take(&mut b.new_packs);
            self.write_index(IndexBlob::new(packs)).await?;
        }

        // snapshot（commit point）
        let parent_key = parent.as_ref().map(|(k, _)| k.clone());
        let stats = b.stats;
        let snapshot_key = self
            .commit_snapshot(&opts, path_bytes, root, parent_key.clone(), stats)
            .await?;
        Ok(BackupSummary {
            snapshot_key,
            parent: parent_key,
            root,
            stats,
        })
    }

    /// 同一台 client 最新的 snapshot，且備份的路徑組相同，才當 parent。
    async fn find_parent(
        &self,
        client_id: &[u8; 16],
        paths: &[Vec<u8>],
    ) -> Result<Option<(String, Snapshot)>> {
        let prefix = keys::snapshot_prefix(client_id);
        let mut keys: Vec<String> = self
            .backend()
            .list(&prefix)
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        keys.sort();
        let Some(latest) = keys.pop() else {
            return Ok(None);
        };
        let snap = match self.read_snapshot(&latest).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "cannot read previous snapshot {latest}: {e}; not using it as parent"
                );
                return Ok(None);
            }
        };
        let same_paths = snap.paths.len() == paths.len()
            && snap
                .paths
                .iter()
                .zip(paths.iter())
                .all(|(a, b)| a.as_ref() as &[u8] == b.as_slice());
        Ok(same_paths.then_some((latest, snap)))
    }

    async fn commit_snapshot(
        &self,
        opts: &BackupOptions,
        paths: Vec<Vec<u8>>,
        root: ObjectId,
        parent: Option<String>,
        stats: SnapshotStats,
    ) -> Result<String> {
        // 同一奈秒撞 key 幾乎不可能，但 conditional put 失敗時換個時間戳再試。
        for _ in 0..3 {
            let now = time::OffsetDateTime::now_utc();
            let key = keys::snapshot(&opts.client_id, &format_key_timestamp(now)?);
            let snapshot = Snapshot {
                version: Snapshot::VERSION,
                client_id: opts.client_id.to_vec(),
                hostname: opts.hostname.clone(),
                username: opts.username.clone(),
                time: format_rfc3339(now)?,
                paths: paths
                    .iter()
                    .cloned()
                    .map(serde_bytes::ByteBuf::from)
                    .collect(),
                root,
                parent: parent.clone(),
                stats,
            };
            match self.write_snapshot(&key, snapshot).await {
                Ok(()) => return Ok(key),
                Err(CoreError::Backend(BackendError::AlreadyExists(_))) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(CoreError::Backend(BackendError::AlreadyExists(
            "snapshot key collided three times".to_owned(),
        )))
    }
}

fn nodes_by_name(nodes: Vec<Node>) -> HashMap<Vec<u8>, Node> {
    nodes.into_iter().map(|n| (n.name.clone(), n)).collect()
}

impl Backup {
    /// 處理一個目錄項目，回傳它的 tree 節點；不支援的類型回 `None`（略過並警告）。
    async fn process_entry(
        &mut self,
        path: &Path,
        name: Vec<u8>,
        meta: &std::fs::Metadata,
        parent: Option<&Node>,
    ) -> Result<Option<Node>> {
        let ft = meta.file_type();
        let node_meta = fsmeta::capture(meta);
        let kind = if ft.is_symlink() {
            let target = std::fs::read_link(path).map_err(|e| CoreError::io(path, e))?;
            self.stats.symlinks += 1;
            NodeKind::Symlink {
                target: fsmeta::path_to_bytes(&target)?,
            }
        } else if ft.is_dir() {
            let parent_subtree = match parent {
                Some(Node {
                    kind: NodeKind::Dir { subtree },
                    ..
                }) => Some(*subtree),
                _ => None,
            };
            let subtree = self.process_dir(path, parent_subtree).await?;
            self.stats.dirs += 1;
            NodeKind::Dir { subtree }
        } else if ft.is_file() {
            let (size, content) = self.process_file(path, meta, &node_meta, parent).await?;
            self.stats.files += 1;
            NodeKind::File { size, content }
        } else {
            tracing::warn!("{}: unsupported file type, skipped", path.display());
            return Ok(None);
        };
        Ok(Some(Node {
            name,
            meta: node_meta,
            kind,
        }))
    }

    /// 遞迴處理一個目錄，回傳它（最後一段）tree 的名稱。
    fn process_dir<'a>(
        &'a mut self,
        path: &'a Path,
        parent_subtree: Option<ObjectId>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ObjectId>> + 'a>> {
        Box::pin(async move {
            let parent_map = match parent_subtree {
                Some(id) => match self.repo.read_tree_chain(&id).await {
                    Ok(nodes) => nodes_by_name(nodes),
                    Err(e) => {
                        tracing::warn!("cannot read parent tree {id}: {e}");
                        HashMap::new()
                    }
                },
                None => HashMap::new(),
            };

            let mut entries = Vec::new();
            let rd = std::fs::read_dir(path).map_err(|e| CoreError::io(path, e))?;
            for entry in rd {
                let entry = entry.map_err(|e| CoreError::io(path, e))?;
                let name = fsmeta::name_to_bytes(&entry.file_name())?;
                entries.push((name, entry.path()));
            }
            entries.sort();

            let mut nodes = Vec::new();
            let mut prev = None;
            for (name, child_path) in entries {
                let meta = match std::fs::symlink_metadata(&child_path) {
                    Ok(m) => m,
                    Err(e) => {
                        // 走訪途中被刪掉的檔案：略過，不讓整個 backup 失敗
                        tracing::warn!("{}: {e}, skipped", child_path.display());
                        continue;
                    }
                };
                let node = self
                    .process_entry(&child_path, name.clone(), &meta, parent_map.get(&name))
                    .await?;
                if let Some(node) = node {
                    nodes.push(node);
                }
                if nodes.len() >= MAX_NODES_PER_TREE {
                    let part = std::mem::take(&mut nodes);
                    prev = Some(self.write_tree(Tree::new(part, prev)).await?);
                }
            }
            self.write_tree(Tree::new(nodes, prev)).await
        })
    }

    /// 根層級的節點清單也可能很長，同樣分段。
    async fn write_tree_parts(&mut self, nodes: Vec<Node>) -> Result<ObjectId> {
        let mut prev = None;
        let mut iter = nodes.into_iter().peekable();
        loop {
            let mut part = Vec::new();
            while part.len() < MAX_NODES_PER_TREE {
                match iter.next() {
                    Some(n) => part.push(n),
                    None => break,
                }
            }
            let id = self.write_tree(Tree::new(part, prev)).await?;
            if iter.peek().is_none() {
                return Ok(id);
            }
            prev = Some(id);
        }
    }

    async fn write_tree(&mut self, tree: Tree) -> Result<ObjectId> {
        let (id, bytes) = self.repo.seal_tree(tree).await?;
        if self.known_trees.insert(id) {
            self.repo.backend().put(&keys::tree(&id), bytes).await?;
        }
        Ok(id)
    }

    /// 處理一個檔案：size、mtime、ctime、inode 都與 parent 相同就沿用它的 chunk 清單，否則讀檔切塊。
    async fn process_file(
        &mut self,
        path: &Path,
        meta: &std::fs::Metadata,
        node_meta: &NodeMeta,
        parent: Option<&Node>,
    ) -> Result<(u64, Content)> {
        let size = meta.len();
        if let Some(Node {
            meta: pmeta,
            kind:
                NodeKind::File {
                    size: psize,
                    content,
                },
            ..
        }) = parent
        {
            if *psize == size && fsmeta::unchanged(pmeta, node_meta) {
                let ids = match content {
                    Content::Direct { chunks } | Content::Indirect { chunks } => chunks,
                };
                let index = self
                    .index
                    .as_ref()
                    .ok_or_else(|| CoreError::Join("index missing".into()))?;
                if ids.iter().all(|id| index.contains(id)) {
                    self.stats.bytes_total += size;
                    self.stats.chunks_total += ids.len() as u64;
                    return Ok((size, content.clone()));
                }
            }
        }

        let result = self.chunk_file(path.to_path_buf()).await?;
        self.stats.bytes_total += result.bytes_total;
        self.stats.bytes_new += result.bytes_new;
        self.stats.chunks_total += result.chunks.len() as u64;
        self.stats.chunks_new += result.chunks_new;
        self.handle_finished(result.finished).await?;

        if result.chunks.len() <= MAX_INLINE_CHUNKS {
            return Ok((
                size,
                Content::Direct {
                    chunks: result.chunks,
                },
            ));
        }
        // 大檔：chunk 清單本身當資料存
        let list_bytes = cbor::encode(&ChunkList::new(result.chunks))?;
        let list_result = self.chunk_bytes(list_bytes).await?;
        self.stats.chunks_new += list_result.chunks_new;
        self.handle_finished(list_result.finished).await?;
        Ok((
            size,
            Content::Indirect {
                chunks: list_result.chunks,
            },
        ))
    }

    async fn chunk_file(&mut self, path: PathBuf) -> Result<FileResult> {
        let file = File::open(&path).map_err(|e| CoreError::io(&path, e))?;
        self.chunk_reader(move || Ok(BufReader::with_capacity(1 << 20, file)), path)
            .await
    }

    async fn chunk_bytes(&mut self, bytes: Vec<u8>) -> Result<FileResult> {
        self.chunk_reader(
            move || Ok(std::io::Cursor::new(bytes)),
            PathBuf::from("<chunk list>"),
        )
        .await
    }

    /// 在 blocking thread 裡切塊、去重、打包。packer 與 index 移進去再移出來。
    async fn chunk_reader<R, F>(&mut self, open: F, path: PathBuf) -> Result<FileResult>
    where
        R: std::io::Read + Send + 'static,
        F: FnOnce() -> Result<R> + Send + 'static,
    {
        let mut packer = self
            .packer
            .take()
            .ok_or_else(|| CoreError::Join("packer missing".into()))?;
        let mut index = self
            .index
            .take()
            .ok_or_else(|| CoreError::Join("index missing".into()))?;
        let chunker = self.chunker;
        let keys = Arc::clone(&self.keys);

        let (packer, index, result) = blocking(move || {
            let mut result = FileResult {
                chunks: Vec::new(),
                bytes_total: 0,
                bytes_new: 0,
                chunks_new: 0,
                finished: Vec::new(),
            };
            let work = (|| -> Result<()> {
                let reader = open()?;
                for chunk in chunker.chunks(reader) {
                    let chunk = chunk.map_err(|e| match e {
                        kist_chunker::ChunkerError::Io(io) => CoreError::io(&path, io),
                        other => other.into(),
                    })?;
                    let id = keys.chunk_id(&chunk);
                    result.bytes_total += chunk.len() as u64;
                    if !index.contains(&id) {
                        let entry = packer.add(id, &chunk)?;
                        index.add_pending(&entry);
                        result.bytes_new += chunk.len() as u64;
                        result.chunks_new += 1;
                        if packer.is_full() {
                            if let Some(p) = packer.finish()? {
                                index.resolve_pending(p.id, p.bytes.len() as u64, &p.entries);
                                result.finished.push(p);
                            }
                        }
                    }
                    result.chunks.push(id);
                }
                Ok(())
            })();
            match work {
                Ok(()) => Ok((packer, index, result)),
                Err(e) => Err(e),
            }
        })
        .await?;
        self.packer = Some(packer);
        self.index = Some(index);
        Ok(result)
    }

    async fn flush_pack(&mut self) -> Result<()> {
        let mut packer = self
            .packer
            .take()
            .ok_or_else(|| CoreError::Join("packer missing".into()))?;
        let finished = blocking(move || {
            let p = packer.finish()?;
            Ok((packer, p))
        })
        .await;
        let (packer, finished) = finished?;
        self.packer = Some(packer);
        if let Some(p) = finished {
            if let Some(index) = self.index.as_mut() {
                index.resolve_pending(p.id, p.bytes.len() as u64, &p.entries);
            }
            self.handle_finished(vec![p]).await?;
        }
        Ok(())
    }

    /// 把封好的 pack 丟去上傳，並記進 index blob 的內容。
    async fn handle_finished(&mut self, packs: Vec<FinishedPack>) -> Result<()> {
        for p in packs {
            self.wait_uploads(MAX_INFLIGHT_UPLOADS - 1).await?;
            self.new_packs.push(IndexPack {
                pack: p.id,
                size: p.bytes.len() as u64,
                entries: p.entries,
            });
            self.stats.packs_new += 1;
            let backend: Backend = self.repo.backend().clone();
            let key = keys::pack(&p.id);
            let bytes = p.bytes;
            self.uploads
                .spawn(async move { backend.put(&key, bytes).await.map_err(Into::into) });
        }
        Ok(())
    }

    /// 等到在飛的上傳數 ≤ `max`。
    async fn wait_uploads(&mut self, max: usize) -> Result<()> {
        while self.uploads.len() > max {
            match self.uploads.join_next().await {
                Some(Ok(r)) => r?,
                Some(Err(e)) => return Err(CoreError::Join(e.to_string())),
                None => break,
            }
        }
        Ok(())
    }
}
