//! restore：把一個 snapshot 還原到目標目錄。
//!
//! 目標目錄底下會重建完整的絕對路徑（`<target>/home/user/data/...`），
//! 這樣一個 snapshot 含多個來源路徑時不會互相覆蓋。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kist_format::tree::{ChunkList, Content, Node, NodeKind};
use kist_format::{cbor, keys, ChunkId};

use crate::fsmeta;
use crate::index::ChunkIndex;
use crate::pack::decode_chunk;
use crate::repo::Repository;
use crate::{blocking, CoreError, Result};

#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {}

impl Repository {
    pub async fn restore(
        &self,
        snapshot_key: &str,
        target: &Path,
        _opts: RestoreOptions,
    ) -> Result<()> {
        let snapshot = self.read_snapshot(snapshot_key).await?;
        let index = self.load_index().await?;
        std::fs::create_dir_all(target).map_err(|e| CoreError::io(target, e))?;
        let nodes = self.read_tree_chain(&snapshot.root).await?;
        for node in nodes {
            let rel = fsmeta::bytes_to_relative_path(&node.name)?;
            let path = target.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
            }
            self.restore_node(&node, &path, &index).await?;
        }
        Ok(())
    }

    fn restore_node<'a>(
        &'a self,
        node: &'a Node,
        path: &'a Path,
        index: &'a ChunkIndex,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            match &node.kind {
                NodeKind::Dir { subtree } => {
                    std::fs::create_dir_all(path).map_err(|e| CoreError::io(path, e))?;
                    let children = self.read_tree_chain(subtree).await?;
                    for child in children {
                        let child_path = path.join(fsmeta::bytes_to_name(&child.name)?);
                        self.restore_node(&child, &child_path, index).await?;
                    }
                    // 子項目都寫完後才設目錄的 mtime，否則會被後續寫入覆蓋
                    fsmeta::apply(path, &node.meta, false)?;
                }
                NodeKind::File { size, content } => {
                    self.restore_file(path, *size, content, index).await?;
                    fsmeta::apply(path, &node.meta, false)?;
                }
                NodeKind::Symlink { target } => {
                    let target_path = PathBuf::from(fsmeta::bytes_to_name(target)?);
                    create_symlink(&target_path, path)?;
                    fsmeta::apply(path, &node.meta, true)?;
                }
            }
            Ok(())
        })
    }

    async fn restore_file(
        &self,
        path: &Path,
        size: u64,
        content: &Content,
        index: &ChunkIndex,
    ) -> Result<()> {
        let chunk_ids = self.resolve_content(content, index).await?;
        let file = std::fs::File::create(path).map_err(|e| CoreError::io(path, e))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut written = 0u64;
        for id in &chunk_ids {
            let data = self.read_chunk(id, index).await?;
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

    /// 從 pack 讀一個 chunk 的明文（range read + 解密 + 驗證）。
    pub(crate) async fn read_chunk(&self, id: &ChunkId, index: &ChunkIndex) -> Result<Vec<u8>> {
        let loc = *index.get(id).ok_or(CoreError::ChunkMissing(*id))?;
        let key = keys::pack(&loc.pack);
        let bytes = self
            .backend()
            .get_range(&key, loc.offset..loc.offset + loc.length)
            .await?;
        let keys = Arc::clone(self.keys());
        let id = *id;
        blocking(move || decode_chunk(&keys, &id, &bytes, loc.flags, loc.raw_len)).await
    }
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
