//! 可達性走訪：從所有 snapshot 出發，走遍 tree，收集「活著的 tree」與「被引用的 chunk」。
//!
//! `check` 與 `prune` 共用同一個走訪：check 對每個檔案另外做大小的驗證，prune 只要集合。
//! 任何讀不到的 snapshot / tree / chunk 清單、以及**不在 index 裡的被引用 chunk**都記在 `errors`；
//! prune 看到 `errors` 非空必須拒絕做任何標記或刪除（引用不完整就不能說誰是垃圾），
//! check 則照常回報。

use std::collections::HashSet;

use kist_format::snapshot::Snapshot;
use kist_format::tree::{Content, Node, NodeKind};
use kist_format::{keys, ChunkId, ObjectId};

use crate::index::ChunkIndex;
use crate::repo::Repository;
use crate::Result;

#[derive(Debug, Default)]
pub struct Reachability {
    /// 讀得出來的 snapshot（key、內容）。
    pub snapshots: Vec<(String, Snapshot)>,
    /// 從任一 snapshot 走得到的 tree（含 `prev` 段）。
    pub live_trees: HashSet<ObjectId>,
    /// 被引用的 chunk：資料 chunk 與 Indirect 的清單 chunk 都算。
    pub referenced_chunks: HashSet<ChunkId>,
    /// 走訪途中讀不到的東西（snapshot、tree、chunk 清單）。
    pub errors: Vec<String>,
}

/// 走訪時對每個檔案節點的回呼輸入。
pub struct FileVisit<'a> {
    /// 這個節點所在的 tree 的 key。
    pub tree_key: &'a str,
    pub node: &'a Node,
    pub size: u64,
    /// 解開 Indirect 之後的資料 chunk。
    pub data_chunks: &'a [ChunkId],
}

impl Repository {
    /// 走遍所有 snapshot。`on_file` 對每個檔案節點呼叫一次（同一個 tree 只走一次）。
    pub(crate) async fn walk_references(
        &self,
        index: &ChunkIndex,
        on_file: &mut dyn FnMut(FileVisit<'_>),
    ) -> Result<Reachability> {
        let mut reach = Reachability::default();
        let snapshot_keys = self.list_snapshot_keys().await?;
        for key in snapshot_keys {
            let snapshot = match self.read_snapshot(&key).await {
                Ok(s) => s,
                Err(e) => {
                    reach.errors.push(format!("{key}: {e}"));
                    continue;
                }
            };
            let root = snapshot.root;
            reach.snapshots.push((key.clone(), snapshot));
            self.walk_tree(&root, &key, index, on_file, &mut reach)
                .await;
        }
        Ok(reach)
    }

    fn walk_tree<'a>(
        &'a self,
        id: &'a ObjectId,
        context: &'a str,
        index: &'a ChunkIndex,
        on_file: &'a mut dyn FnMut(FileVisit<'_>),
        reach: &'a mut Reachability,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
        Box::pin(async move {
            if !reach.live_trees.insert(*id) {
                return;
            }
            let key = keys::tree(id);
            let tree = match self.read_tree(id).await {
                Ok(t) => t,
                Err(e) => {
                    reach.errors.push(format!("{key} (from {context}): {e}"));
                    return;
                }
            };
            if let Some(prev) = tree.prev {
                self.walk_tree(&prev, context, index, on_file, reach).await;
            }
            for node in &tree.nodes {
                match &node.kind {
                    NodeKind::Dir { subtree } => {
                        self.walk_tree(subtree, context, index, on_file, reach)
                            .await;
                    }
                    NodeKind::File { size, content } => {
                        let data = match content {
                            Content::Direct { chunks } => chunks.clone(),
                            Content::Indirect { chunks } => {
                                reach.referenced_chunks.extend(chunks.iter().copied());
                                if let Some(missing) = chunks.iter().find(|c| !index.contains(c)) {
                                    reach.errors.push(format!(
                                        "{key}: chunk list chunk {missing} is missing from the index"
                                    ));
                                    continue;
                                }
                                match self.resolve_content(content, index).await {
                                    Ok(ids) => ids,
                                    Err(e) => {
                                        reach.errors.push(format!("{key}: chunk list: {e}"));
                                        continue;
                                    }
                                }
                            }
                        };
                        reach.referenced_chunks.extend(data.iter().copied());
                        for c in &data {
                            if !index.contains(c) {
                                reach
                                    .errors
                                    .push(format!("{key}: chunk {c} is missing from the index"));
                            }
                        }
                        on_file(FileVisit {
                            tree_key: &key,
                            node,
                            size: *size,
                            data_chunks: &data,
                        });
                    }
                    NodeKind::Symlink { .. } => {}
                }
            }
        })
    }
}
