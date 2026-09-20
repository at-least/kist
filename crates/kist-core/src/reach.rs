//! 可達性走訪：從所有 snapshot 出發，走遍 tree，收集「活著的 tree」與「被引用的 chunk」。
//!
//! `check` 與 `prune` 共用同一個走訪：check 對每個檔案另外做大小的驗證，prune 只要集合。
//! 任何讀不到的 snapshot / tree / chunk 清單、以及**不在 index 裡的被引用 chunk**都記在 `errors`；
//! prune 看到 `errors` 非空必須拒絕做任何標記或刪除（引用不完整就不能說誰是垃圾），
//! check 則照常回報。

use std::collections::HashSet;

use kist_format::snapshot::Snapshot;
use kist_format::tree::{content_type, node_type, Entry};
use kist_format::{keys, ChunkId, ObjectId, TreeId};

use crate::index::ChunkLocator;
use crate::repo::Repository;
use crate::{Result, MAX_TREE_DEPTH};

#[derive(Debug, Default)]
pub struct Reachability {
    /// 讀得出來的 snapshot（key、內容）。
    pub snapshots: Vec<(String, Snapshot)>,
    /// 從任一 snapshot 走得到的 tree（含 `prev` 段），以 gc 命名空間的名稱（ObjectId）記錄。
    pub live_trees: HashSet<ObjectId>,
    /// 被引用的 chunk：資料 chunk 與 Indirect 的清單 chunk 都算。
    /// 收集時不排序、可能重複；prune 排序去重後合併進自己的索引
    ///（100 萬 chunk ≈ 32 MiB，HashSet 要 67 MiB 以上）。
    pub referenced_chunks: Vec<ChunkId>,
    /// 走訪途中讀不到的東西（snapshot、tree、chunk 清單）。
    pub errors: Vec<String>,
}

/// 走訪時對每個檔案節點的回呼輸入。
pub struct FileVisit<'a> {
    /// 這個節點所在的 tree 的 key。
    pub tree_key: &'a str,
    pub node: &'a Entry,
    pub size: u64,
    /// 解開 Indirect 之後的資料 chunk。
    pub data_chunks: &'a [ChunkId],
}

impl Repository {
    /// 走遍所有 snapshot。`on_file` 對每個檔案節點呼叫一次（同一個 tree 只走一次）。
    /// `Send`：讓 prune / check 的 future 能被 `tokio::spawn`（daemon 在別的 task 上跑工作）。
    pub(crate) async fn walk_references<I>(
        &self,
        index: &I,
        on_file: &mut (dyn FnMut(FileVisit<'_>) + Send),
    ) -> Result<Reachability>
    where
        I: ChunkLocator + Sync + ?Sized,
    {
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
            let roots: Vec<TreeId> = snapshot.roots.iter().map(|r| r.tree).collect();
            reach.snapshots.push((key.clone(), snapshot));
            for root in roots {
                self.walk_tree(&root, 1, &key, index, on_file, &mut reach)
                    .await;
            }
        }
        Ok(reach)
    }

    /// `depth`：這棵 tree 在 DIR 巢狀裡的深度（根 = 1）。prev 段**不計**深度
    /// ——同一層目錄的分段，誠實的大目錄可以很長。DIR 巢狀超過
    /// [`MAX_TREE_DEPTH`] 只能出自腐壞或敵意 repo：記錄錯誤、不深入。
    fn walk_tree<'a, I>(
        &'a self,
        id: &'a TreeId,
        depth: usize,
        context: &'a str,
        index: &'a I,
        on_file: &'a mut (dyn FnMut(FileVisit<'_>) + Send),
        reach: &'a mut Reachability,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
    where
        I: ChunkLocator + Sync + ?Sized + 'a,
    {
        Box::pin(async move {
            // prev chain 用迴圈走：遞迴是 Box::pin 的 future，每一段都吃一份
            // stack frame，而且抱著 `tree` await 下去會把整條 chain（一個大
            // 目錄可能上百段）同時留在記憶體（100 萬檔 ≈ 240 MiB）。每段先
            // 處理完就丟，再去下一段；visited 集合擋掉圈。
            let mut id = *id;
            loop {
                if !reach
                    .live_trees
                    .insert(ObjectId::from_bytes(*id.as_bytes()))
                {
                    return;
                }
                let key = keys::tree(&id);
                let tree = match self.read_tree(&id).await {
                    Ok(t) => t,
                    Err(e) => {
                        reach.errors.push(format!("{key} (from {context}): {e}"));
                        return;
                    }
                };
                let prev = tree.prev;
                for node in &tree.entries {
                    match node.kind {
                        node_type::DIR if !node.subtree.is_zero() => {
                            if depth >= MAX_TREE_DEPTH {
                                reach.errors.push(format!(
                                    "{key}: tree nesting deeper than {MAX_TREE_DEPTH} levels \
                                     (corrupt or hostile repository)"
                                ));
                                continue;
                            }
                            self.walk_tree(
                                &node.subtree,
                                depth + 1,
                                context,
                                index,
                                on_file,
                                reach,
                            )
                            .await;
                        }
                        node_type::FILE => {
                            let data = if node.content == content_type::DIRECT {
                                node.chunks.clone()
                            } else {
                                reach.referenced_chunks.extend(node.chunks.iter().copied());
                                if let Some(missing) =
                                    node.chunks.iter().find(|c| !index.contains(c))
                                {
                                    reach.errors.push(format!(
                                        "{key}: chunk list chunk {missing} is missing from the index"
                                    ));
                                    continue;
                                }
                                match self.resolve_chunks(&node.chunks, node.content, index).await {
                                    Ok(ids) => ids,
                                    Err(e) => {
                                        reach.errors.push(format!("{key}: chunk list: {e}"));
                                        continue;
                                    }
                                }
                            };
                            reach.referenced_chunks.extend(data.iter().copied());
                            for c in &data {
                                if !index.contains(c) {
                                    reach.errors.push(format!(
                                        "{key}: chunk {c} is missing from the index"
                                    ));
                                }
                            }
                            on_file(FileVisit {
                                tree_key: &key,
                                node,
                                size: node.size,
                                data_chunks: &data,
                            });
                        }
                        _ => {}
                    }
                }
                drop(tree);
                match prev {
                    Some(p) => id = p,
                    None => return,
                }
            }
        })
    }
}
