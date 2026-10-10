//! item 的真相：每個 item 的內容與它引用的 item（DAG）。ID 以內容 intern，內容相同 ID 就相同，
//! 跨 client、跨 snapshot 的去重因此自然發生。

use std::collections::{BTreeMap, BTreeSet};

use crate::types::{ItemId, Kind};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Node {
    File(Vec<ItemId>),
    Dir(ItemId),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ItemDef {
    /// 一個 chunk（資料）。
    Chunk(u32),
    /// 一個目錄（metadata，只有一段）。
    Seg(Vec<(u8, Node)>),
    /// snapshot 的 root 清單（metadata）。
    Roots(Vec<ItemId>),
    /// 誠實的 snapshot 不會引用的資料 item（給惡意寫入用）。
    Junk(u32),
}

#[derive(Clone, Debug, Default)]
pub struct Universe {
    defs: Vec<ItemDef>,
    intern: BTreeMap<ItemDef, ItemId>,
}

impl Universe {
    pub fn intern(&mut self, def: ItemDef) -> ItemId {
        if let Some(id) = self.intern.get(&def) {
            return *id;
        }
        let id = ItemId(self.defs.len() as u32);
        self.defs.push(def.clone());
        self.intern.insert(def, id);
        id
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    fn def(&self, id: ItemId) -> Option<&ItemDef> {
        self.defs.get(id.0 as usize)
    }

    pub fn kind(&self, id: ItemId) -> Kind {
        match self.def(id) {
            Some(ItemDef::Seg(_)) | Some(ItemDef::Roots(_)) => Kind::Meta,
            _ => Kind::Data,
        }
    }

    pub fn children(&self, id: ItemId) -> Vec<ItemId> {
        match self.def(id) {
            Some(ItemDef::Seg(entries)) => entries
                .iter()
                .flat_map(|(_, node)| match node {
                    Node::File(chunks) => chunks.clone(),
                    Node::Dir(dir) => vec![*dir],
                })
                .collect(),
            Some(ItemDef::Roots(roots)) => roots.clone(),
            _ => Vec::new(),
        }
    }

    pub fn reachable(&self, root: ItemId) -> BTreeSet<ItemId> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if seen.insert(id) {
                stack.extend(self.children(id));
            }
        }
        seen
    }

    fn file(&mut self, seeds: &[u32]) -> Node {
        Node::File(
            seeds
                .iter()
                .map(|s| self.intern(ItemDef::Chunk(*s)))
                .collect(),
        )
    }

    /// 一組 roots（`job`）在某個內容版本（`variant` 的 5 個 bit）下的樹，回傳 root 清單的 ID。
    /// 子目錄 `a` 只看 bit 0–1，bit 0–1 不變時整棵沿用（parent 子樹的快速路徑）；chunk 200 與
    /// 500+job 在多個版本之間共用，`b` 的內容跨 job 共用一半。
    pub fn tree(&mut self, job: u8, variant: u8) -> ItemId {
        let bit = |i: u8| u32::from((variant >> i) & 1);
        let job = u32::from(job);
        let a0 = self.file(&[100 + bit(0), 200]);
        let a1 = self.file(&[110 + bit(1)]);
        let a = self.intern(ItemDef::Seg(vec![(0, a0), (1, a1)]));
        let b0 = self.file(&[300 + bit(2) + 10 * (job % 2)]);
        let b = self.intern(ItemDef::Seg(vec![(0, b0)]));
        let f = self.file(&[400 + bit(3), 410 + bit(4), 500 + job]);
        let root_dir = self.intern(ItemDef::Seg(vec![
            (0, Node::Dir(a)),
            (1, Node::Dir(b)),
            (2, f),
        ]));
        self.intern(ItemDef::Roots(vec![root_dir]))
    }
}
