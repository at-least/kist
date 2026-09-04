//! restore：把一個 snapshot 還原到目標目錄。
//!
//! 目標目錄底下會重建完整的絕對路徑（`<target>/home/user/data/...`），
//! 這樣一個 snapshot 含多個來源路徑時不會互相覆蓋。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use kist_format::tree::{ChunkList, Content, Node, NodeKind};
use kist_format::{cbor, keys, ChunkId};

use crate::fsmeta;
use crate::index::ChunkIndex;
use crate::pack::decode_chunk;
use crate::repo::Repository;
use crate::{blocking, CoreError, Result};

#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {}

/// 讀取途中 chunk 的 pack 不見了（prune 的 repack 把它搬走了）就重新載入的 index。
/// 重載最多每分鐘一次：真的壞掉的 repo 不會每個 chunk 都重載一遍。
pub struct ReloadableIndex {
    index: RwLock<ChunkIndex>,
    last_reload: Mutex<Option<std::time::Instant>>,
}

impl ReloadableIndex {
    pub fn new(index: ChunkIndex) -> Self {
        Self {
            index: RwLock::new(index),
            last_reload: Mutex::new(None),
        }
    }

    pub async fn get(&self) -> tokio::sync::RwLockReadGuard<'_, ChunkIndex> {
        self.index.read().await
    }
}

const RELOAD_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// restore 的結果：單一檔案失敗不會中止整個 restore，而是記在 `errors` 裡。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RestoreSummary {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub errors: Vec<String>,
}

impl Repository {
    /// 還原到 `target`。目標目錄最好是空的：既有檔案會被覆寫、既有 symlink 會被跟隨。
    pub async fn restore(
        &self,
        snapshot_key: &str,
        target: &Path,
        _opts: RestoreOptions,
    ) -> Result<RestoreSummary> {
        let snapshot = self.read_snapshot(snapshot_key).await?;
        let index = ReloadableIndex::new(self.load_index().await?);
        std::fs::create_dir_all(target).map_err(|e| CoreError::io(target, e))?;
        let nodes = self.read_tree_chain(&snapshot.root).await?;
        let mut summary = RestoreSummary::default();
        for node in nodes {
            let rel = fsmeta::bytes_to_relative_path(&node.name)?;
            let path = target.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
            }
            self.restore_node(&node, &path, &index, &mut summary).await;
        }
        Ok(summary)
    }

    /// 還原一個節點。錯誤記進 summary，不往上拋：一個壞掉的 chunk 不該讓其他 99% 的檔案也拿不回來。
    fn restore_node<'a>(
        &'a self,
        node: &'a Node,
        path: &'a Path,
        index: &'a ReloadableIndex,
        summary: &'a mut RestoreSummary,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            let result = match &node.kind {
                NodeKind::Dir { subtree } => {
                    self.restore_dir(subtree, node, path, index, summary).await
                }
                NodeKind::File { size, content } => {
                    match self.restore_file(path, *size, content, index).await {
                        Ok(()) => {
                            summary.files += 1;
                            fsmeta::apply(path, &node.meta, false)
                        }
                        Err(e) => {
                            // 別留下寫到一半的檔案：使用者會誤以為它是完整的
                            let _ = std::fs::remove_file(path);
                            Err(e)
                        }
                    }
                }
                NodeKind::Symlink { target } => match fsmeta::bytes_to_name(target) {
                    Ok(name) => match replace_with_symlink(&PathBuf::from(name), path) {
                        Ok(()) => {
                            summary.symlinks += 1;
                            fsmeta::apply(path, &node.meta, true)
                        }
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                },
            };
            if let Err(e) = result {
                tracing::warn!("{}: {e}", path.display());
                summary.errors.push(format!("{}: {e}", path.display()));
            }
        })
    }

    async fn restore_dir(
        &self,
        subtree: &kist_format::ObjectId,
        node: &Node,
        path: &Path,
        index: &ReloadableIndex,
        summary: &mut RestoreSummary,
    ) -> Result<()> {
        std::fs::create_dir_all(path).map_err(|e| CoreError::io(path, e))?;
        let children = self.read_tree_chain(subtree).await?;
        for child in children {
            if let Err(e) = fsmeta::validate_child_name(&child.name) {
                summary.errors.push(format!("{}: {e}", path.display()));
                continue;
            }
            let child_path = path.join(fsmeta::bytes_to_name(&child.name)?);
            self.restore_node(&child, &child_path, index, summary).await;
        }
        summary.dirs += 1;
        // 子項目都寫完後才設目錄的 mtime，否則會被後續寫入覆蓋
        fsmeta::apply(path, &node.meta, false)
    }

    async fn restore_file(
        &self,
        path: &Path,
        size: u64,
        content: &Content,
        index: &ReloadableIndex,
    ) -> Result<()> {
        let chunk_ids = match content {
            Content::Direct { chunks } => chunks.clone(),
            Content::Indirect { chunks } => {
                let mut bytes = Vec::new();
                for id in chunks {
                    bytes.extend_from_slice(&self.read_chunk_reloading(id, index).await?);
                }
                let list: ChunkList = cbor::decode(&bytes).map_err(|e| CoreError::Corrupt {
                    key: "<chunk list>".to_owned(),
                    reason: e.to_string(),
                })?;
                list.chunks
            }
        };
        let file = std::fs::File::create(path).map_err(|e| CoreError::io(path, e))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut written = 0u64;
        for id in &chunk_ids {
            let data = self.read_chunk_reloading(id, index).await?;
            writer
                .write_all(&data)
                .map_err(|e| CoreError::io(path, e))?;
            written += data.len() as u64;
        }
        writer.flush().map_err(|e| CoreError::io(path, e))?;
        if written != size {
            return Err(CoreError::Corrupt {
                key: path.display().to_string(),
                reason: format!("restored {written} bytes but snapshot says {size}"),
            });
        }
        Ok(())
    }

    /// Direct 直接回傳；Indirect 先把清單 chunk 讀出來解成 ChunkList。
    pub(crate) async fn resolve_content(
        &self,
        content: &Content,
        index: &ChunkIndex,
    ) -> Result<Vec<ChunkId>> {
        match content {
            Content::Direct { chunks } => Ok(chunks.clone()),
            Content::Indirect { chunks } => {
                let mut bytes = Vec::new();
                for id in chunks {
                    bytes.extend_from_slice(&self.read_chunk(id, index).await?);
                }
                let list: ChunkList = cbor::decode(&bytes).map_err(|e| CoreError::Corrupt {
                    key: "<chunk list>".to_owned(),
                    reason: e.to_string(),
                })?;
                Ok(list.chunks)
            }
        }
    }

    /// 同 `read_chunk`，但 chunk 不在 index 或它的 pack 不見了時重新載入 index 再試一次：
    /// prune 的 repack 會把活 chunk 搬到新 pack、之後刪舊 pack，開始得比較早的 restore
    /// 手上的 index 指到舊位置。重載後還是找不到才是真的壞。
    pub async fn read_chunk_reloading(
        &self,
        id: &ChunkId,
        index: &ReloadableIndex,
    ) -> Result<Vec<u8>> {
        let first = {
            let guard = index.index.read().await;
            self.read_chunk(id, &guard).await
        };
        match first {
            Err(CoreError::ChunkMissing(_))
            | Err(CoreError::Backend(kist_backend::BackendError::NotFound(_))) => {}
            other => return other,
        }
        {
            let mut last = index.last_reload.lock().await;
            let due = last.is_none_or(|t| t.elapsed() >= RELOAD_MIN_INTERVAL);
            if due {
                tracing::warn!(
                    "chunk {id}: its pack is missing; reloading the index (a repack may be in progress)"
                );
                let fresh = self.load_index().await?;
                *index.index.write().await = fresh;
                *last = Some(std::time::Instant::now());
            }
        }
        let guard = index.index.read().await;
        self.read_chunk(id, &guard).await
    }

    /// 從 pack 讀一個 chunk 的明文（range read + 解密 + 驗證）。
    pub(crate) async fn read_chunk(&self, id: &ChunkId, index: &ChunkIndex) -> Result<Vec<u8>> {
        let loc = index.get(id).ok_or(CoreError::ChunkMissing(*id))?;
        let key = keys::pack(&loc.pack);
        let end = loc
            .offset
            .checked_add(loc.length)
            .ok_or_else(|| CoreError::Corrupt {
                key: key.clone(),
                reason: format!("chunk {id} range overflows"),
            })?;
        let bytes = self.backend().get_range(&key, loc.offset..end).await?;
        let keys = Arc::clone(self.keys());
        let id = *id;
        blocking(move || decode_chunk(&keys, &id, &bytes, loc.flags, loc.raw_len)).await
    }
}

/// 建 symlink 前先移除既有的檔案或 symlink（第二次 restore 到同一目錄）；既有的是目錄則回錯。
fn replace_with_symlink(target: &Path, link: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(link) {
        if meta.is_dir() {
            return Err(CoreError::Corrupt {
                key: link.display().to_string(),
                reason: "a directory is in the way of a symlink".to_owned(),
            });
        }
        std::fs::remove_file(link).map_err(|e| CoreError::io(link, e))?;
    }
    create_symlink(target, link)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|e| CoreError::io(link, e))
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    // Windows 建 symlink 需要特權；失敗只警告，不讓整個 restore 中止。
    if let Err(e) = std::os::windows::fs::symlink_file(target, link) {
        tracing::warn!("{}: cannot create symlink: {e}", link.display());
    }
    Ok(())
}
