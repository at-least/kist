//! snapshot 根目錄的虛擬層級（v3）：roots 的定位字串（`/tmp/x/src`、
//! `s3://bucket/prefix`）不是單一目錄組件，mount 把它們展開成虛擬的
//! 中介目錄（瀏覽成 `tmp` → `x` → `src`）；葉節點是該 root 的 tree
//! 內容（合成的 DIR entry 帶 subtree）。與 restore 的映射同一套切段
//! 規則（format-v3-draft §9：去 scheme、`/` 切段）。

use std::collections::HashMap;
use std::sync::Arc;

use kist_format::snapshot::Root;
use kist_format::tree::{meta_kind, node_type, Entry};
use kist_format::TreeId;

/// 一個虛擬層級裡的條目：真實（root 的葉或 tree entry）或合成的中介目錄。
#[derive(Debug, Clone)]
pub enum VEntry {
    Real(Arc<Entry>),
    /// 合成的中介目錄（repo 裡沒有這個目錄）。
    Synthetic {
        name: Vec<u8>,
    },
}

impl VEntry {
    pub fn name(&self) -> &[u8] {
        match self {
            VEntry::Real(e) => &e.name,
            VEntry::Synthetic { name } => name,
        }
    }

    pub fn entry(&self) -> Option<&Entry> {
        match self {
            VEntry::Real(e) => Some(e),
            VEntry::Synthetic { .. } => None,
        }
    }
}

/// 展開後的虛擬層級表。頂層 = `b""`；中介層以完整虛擬路徑為 key
/// （`b"/tmp"`、`b"/tmp/x"`——前導斜線是我們自己的慣例，只當 key 用）。
#[derive(Debug, Default)]
pub struct VirtualRoot {
    levels: HashMap<Vec<u8>, Vec<VEntry>>,
}

/// 一個 root 的葉子形態（由 `corefs` 讀 tree 後判別，見 §9 的映射規則）。
pub enum RootContents {
    /// 目錄來源：葉子是攜帶 `subtree` 的合成 DIR（children 懶載入）。
    Dir,
    /// 檔案/symlink 來源：root tree 恰好一個非目錄 entry、名稱 = 定位末段
    /// ——葉子是那個 entry 本身（與 restore 的落點一致）。
    Leaf(Box<Entry>),
    /// 定位沒有組件（`/`、`s3://bucket/`）：root tree 的內容**攤平到頂層**
    /// （與 restore 的「空相對路徑 = 直接落在 target」一致）。
    Flatten(Vec<Entry>),
}

impl VirtualRoot {
    /// roots → 虛擬層級。每個 root 的定位字串切成組件，最後一段是攜帶
    /// `subtree` 的合成 DIR（瀏覽到那裡就載入 root 的 tree 內容）；
    /// 其餘組件是合成中介目錄。形態判別見 [`RootContents`]。
    pub fn build(roots: impl IntoIterator<Item = (Root, RootContents)>) -> Self {
        let mut levels: HashMap<Vec<u8>, Vec<VEntry>> = HashMap::new();
        for (root, contents) in roots {
            let comps = locator_components(root.path.as_slice());
            let Some((last, parents)) = comps.split_last() else {
                match contents {
                    RootContents::Flatten(entries) => {
                        for e in entries {
                            push_real(&mut levels, b"", &e);
                        }
                    }
                    RootContents::Dir | RootContents::Leaf(_) => {}
                }
                continue;
            };
            let mut parent: Vec<u8> = Vec::new();
            for comp in parents {
                let level = levels.entry(parent.clone()).or_default();
                if !has_name(level, comp) {
                    level.push(VEntry::Synthetic {
                        name: comp.to_vec(),
                    });
                }
                parent.push(b'/');
                parent.extend_from_slice(comp);
            }
            let leaf = match contents {
                RootContents::Dir => root_leaf_entry(last, root.tree),
                RootContents::Leaf(e) => *e,
                RootContents::Flatten(_) => root_leaf_entry(last, root.tree),
            };
            push_real(&mut levels, &parent, &leaf);
        }
        for level in levels.values_mut() {
            level.sort_by(|a, b| a.name().cmp(b.name()));
        }
        Self { levels }
    }

    /// snapshot 根目錄那一層。
    pub fn top(&self) -> &[VEntry] {
        self.level(b"")
    }

    /// 合成中介層（key 見結構說明）。
    pub fn level(&self, key: &[u8]) -> &[VEntry] {
        self.levels.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 在一個層級裡以名稱找條目（層級已排序，二分搜尋）。
    pub fn lookup<'a>(level: &'a [VEntry], name: &[u8]) -> Option<&'a VEntry> {
        let i = level.partition_point(|v| v.name() < name);
        level.get(i).filter(|v| v.name() == name)
    }
}

/// root 定位 → 組件（與 fsmeta::locator_to_relative 同一套規則：
/// 去 scheme、`/` 切段、空與 `.` 組件正規化掉）。
fn locator_components(path: &[u8]) -> Vec<&[u8]> {
    let rest = match path.iter().position(|&b| b == b':') {
        Some(i) if path.len() >= i + 3 && &path[i + 1..i + 3] == b"//" => &path[i + 3..],
        _ => path,
    };
    rest.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        // `..` 映射成 `__parent__`：與 fsmeta::locator_to_relative、Go 端
        // locatorComponents 同一套規則——字面 `..` 是 FUSE 核心自己的
        // 東西，虛擬層重複吐一個只會是不可達的同名 entry。
        .map(|c| {
            if c == b".." {
                b"__parent__" as &[u8]
            } else {
                c
            }
        })
        .collect()
}

/// root 的葉組件 → 攜帶 subtree 的合成 DIR entry（posix 形狀：mount 的
/// Attr 對 uid/gid=0、mode 唯讀、mtime=snapshot 時間由呼叫端覆蓋）。
fn root_leaf_entry(name: &[u8], subtree: TreeId) -> Entry {
    Entry {
        name: name.to_vec(),
        kind: node_type::DIR,
        meta_kind: meta_kind::POSIX,
        size: 0,
        target: Vec::new(),
        content: 0,
        chunks: Vec::new(),
        subtree,
        mode: Some(0o040_555),
        uid: Some(0),
        gid: Some(0),
        mtime_ns: Some(0),
        ctime_ns: None,
        dev: None,
        inode: None,
        nlink: None,
        xattrs: None,
        etag: None,
        vern: None,
    }
}

fn has_name(level: &[VEntry], name: &[u8]) -> bool {
    level.iter().any(|v| v.name() == name)
}

/// 放入真實條目；同名合成條目被替換（真實的贏）。
fn push_real(levels: &mut HashMap<Vec<u8>, Vec<VEntry>>, key: &[u8], e: &Entry) {
    let level = levels.entry(key.to_vec()).or_default();
    match level
        .iter()
        .position(|v| matches!(v, VEntry::Synthetic { .. }) && v.name() == e.name)
    {
        Some(i) => level[i] = VEntry::Real(Arc::new(e.clone())),
        None => level.push(VEntry::Real(Arc::new(e.clone()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_bytes::ByteBuf;

    fn root_of(path: &str) -> Root {
        Root {
            path: ByteBuf::from(path.as_bytes().to_vec()),
            tree: TreeId::from_bytes([0xA5; 32]),
        }
    }

    fn names(level: &[VEntry]) -> Vec<Vec<u8>> {
        level.iter().map(|v| v.name().to_vec()).collect()
    }

    fn dir_root(r: Root) -> (Root, RootContents) {
        (r, RootContents::Dir)
    }

    #[test]
    fn single_component_root_is_a_real_top_level_entry() {
        let root = VirtualRoot::build([dir_root(root_of("data"))]);
        assert_eq!(names(root.top()), vec![b"data".to_vec()]);
        assert!(root.top()[0].entry().is_some(), "頂層就是 root 葉");
    }

    #[test]
    fn absolute_path_expands_into_synthetic_levels() {
        let root = VirtualRoot::build([dir_root(root_of("/tmp/x/src"))]);
        assert_eq!(names(root.top()), vec![b"tmp".to_vec()]);
        assert!(root.top()[0].entry().is_none(), "tmp 是合成的");
        assert_eq!(names(root.level(b"/tmp")), vec![b"x".to_vec()]);
        let level = root.level(b"/tmp/x");
        assert_eq!(names(level), vec![b"src".to_vec()]);
        let leaf = VirtualRoot::lookup(level, b"src").unwrap().entry().unwrap();
        assert_eq!(leaf.subtree, TreeId::from_bytes([0xA5; 32]));
    }

    #[test]
    fn remote_locators_strip_their_scheme() {
        // s3://bucket/prefix → bucket/prefix（與 restore 映射一致）
        let root = VirtualRoot::build([dir_root(root_of("s3://bucket/prefix"))]);
        assert_eq!(names(root.top()), vec![b"bucket".to_vec()]);
        assert_eq!(names(root.level(b"/bucket")), vec![b"prefix".to_vec()]);
    }

    #[test]
    fn many_roots_share_synthetic_levels() {
        let root = VirtualRoot::build([
            dir_root(root_of("/home/a/data")),
            dir_root(root_of("/home/b/data")),
            dir_root(root_of("/etc/config")),
        ]);
        let mut top = names(root.top());
        top.sort();
        assert_eq!(top, vec![b"etc".to_vec(), b"home".to_vec()]);
        assert_eq!(
            names(root.level(b"/home")),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn empty_locator_is_skipped_without_panicking() {
        let root = VirtualRoot::build([dir_root(root_of("/"))]);
        assert!(root.top().is_empty());
    }

    #[test]
    fn non_utf8_components_survive() {
        let mut path = b"/tmp/".to_vec();
        path.extend_from_slice(&[0xFF, 0xFE]);
        // 非 UTF-8 用 Bytes 直接構造
        let root = VirtualRoot::build([(
            Root {
                path: ByteBuf::from(path),
                tree: TreeId::from_bytes([1; 32]),
            },
            RootContents::Dir,
        )]);
        assert_eq!(names(root.top()), vec![b"tmp".to_vec()]);
        assert!(VirtualRoot::lookup(root.level(b"/tmp"), &[0xFF, 0xFE]).is_some());
    }

    #[test]
    fn levels_are_sorted_for_binary_search() {
        let root = VirtualRoot::build([
            dir_root(root_of("/z")),
            dir_root(root_of("/a")),
            dir_root(root_of("/m")),
        ]);
        assert_eq!(
            names(root.top()),
            vec![b"a".to_vec(), b"m".to_vec(), b"z".to_vec()]
        );
        assert!(VirtualRoot::lookup(root.top(), b"m").is_some());
        assert!(VirtualRoot::lookup(root.top(), b"b").is_none());
    }

    /// `..` 組件的對映必須與 fsmeta::locator_to_relative（和 Go 端的
    /// locatorComponents）同一套：映射成 `__parent__`，不是留下字面 `..`。
    /// 字面 `..` 在 FUSE 目錄裡是核心自己的東西，虛擬層重複吐一個
    /// 不可達的同名 entry。
    #[test]
    fn dotdot_component_maps_to_parent_marker() {
        // /a/../b：a 之下是 __parent__（.. 的映射）與 b，絕不是字面 ..。
        let root = VirtualRoot::build([dir_root(root_of("/a/../b"))]);
        assert_eq!(names(root.top()), vec![b"a".to_vec()]);
        let under_a = names(root.level(b"/a"));
        assert!(
            under_a.iter().any(|n| n == &b"__parent__".to_vec()),
            "`..` 應映射成 __parent__，得到 {under_a:?}"
        );
        assert!(
            !under_a.iter().any(|n| n == &b"..".to_vec()),
            "字面 `..` 不該留在虛擬層：{under_a:?}"
        );
        assert_eq!(
            names(root.level(b"/a/__parent__")),
            vec![b"b".to_vec()],
            "root 葉在映射後的路徑下"
        );
    }
}
