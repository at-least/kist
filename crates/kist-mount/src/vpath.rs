//! snapshot 根目錄的虛擬層級：根 tree 的條目以**絕對來源路徑**命名
//! （`/tmp/x/src`），不是單一目錄組件。mount 把它們展開成虛擬的中介目錄
//! （瀏覽成 `tmp` → `x` → `src`）；「真實」條目永遠壓過同名的合成目錄。
//! （Go 參考實作 `expandRoots` 的 Rust 版；同名時 Go 會兩筆都留，這裡改成
//! 真實條目直接替換合成條目——Go 的註解本來就說真實的要贏。）

use std::collections::HashMap;
use std::sync::Arc;

use kist_format::tree::Entry;

/// 一個虛擬層級裡的條目：真實（repo 裡的 tree entry）或合成的中介目錄。
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

impl VirtualRoot {
    pub fn build(entries: &[Entry]) -> Self {
        let mut levels: HashMap<Vec<u8>, Vec<VEntry>> = HashMap::new();

        // 先收斂每個層級（真實壓過合成），最後統一排序。
        for e in entries {
            let name = e.name.as_slice();
            if !name.contains(&b'/') {
                // 單一組件的根名（防禦：寫入端用絕對路徑）直接放頂層。
                push_real(&mut levels, b"", e);
                continue;
            }
            let comps: Vec<&[u8]> = split_path(name);
            if comps.is_empty() {
                // 名稱切不出組件（`/` 本身）——corefs 會先把這種條目的子樹
                // 攤平到頂層，這裡 defensively 跳過（不能 panic：FUSE callback）。
                continue;
            }
            let mut parent: Vec<u8> = Vec::new();
            for comp in &comps[..comps.len() - 1] {
                let level = levels.entry(parent.clone()).or_default();
                if !has_name(level, comp) {
                    level.push(VEntry::Synthetic {
                        name: comp.to_vec(),
                    });
                }
                parent.push(b'/');
                parent.extend_from_slice(comp);
            }
            // 真實條目以**最後一段組件**為名（原本是絕對路徑）。
            let mut leaf = e.clone();
            leaf.name = comps[comps.len() - 1].to_vec();
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

/// 絕對路徑 → 組件（不吃空組件：`//a`、前後斜線都正規化掉）。
fn split_path(name: &[u8]) -> Vec<&[u8]> {
    name.split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect()
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
    use kist_format::tree::{node_type, Entry};
    use kist_format::TreeId;

    fn entry_of(kind: u8, mode: u32, name: &[u8]) -> Entry {
        Entry {
            name: name.to_vec(),
            kind,
            mode,
            uid: 0,
            gid: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            size: 0,
            target: Vec::new(),
            chunks: Vec::new(),
            content: 0,
            subtree: TreeId::ZERO,
            dev: 0,
            inode: 0,
            nlink: 0,
            xattrs: None,
        }
    }

    fn dir_entry(name: &str) -> Entry {
        entry_of(node_type::DIR, 0o40755, name.as_bytes())
    }

    fn file_entry(name: &str) -> Entry {
        entry_of(node_type::FILE, 0o100644, name.as_bytes())
    }

    fn names(level: &[VEntry]) -> Vec<Vec<u8>> {
        level.iter().map(|v| v.name().to_vec()).collect()
    }

    #[test]
    fn single_component_passes_through() {
        // 防禦路徑：非絕對的根名直接放頂層
        let root = VirtualRoot::build(&[file_entry("src")]);
        assert_eq!(names(root.top()), vec![b"src".to_vec()]);
        assert!(root.top()[0].entry().is_some(), "頂層就是真實條目");
        assert!(root.level(b"/src").is_empty());
    }

    #[test]
    fn absolute_path_expands_into_synthetic_levels() {
        let root = VirtualRoot::build(&[file_entry("/tmp/x/src")]);
        // 頂層只有合成 tmp
        assert_eq!(names(root.top()), vec![b"tmp".to_vec()]);
        assert!(root.top()[0].entry().is_none(), "tmp 是合成的");
        // /tmp 底下只有合成 x；/tmp/x 底下是真實的 src
        assert_eq!(names(root.level(b"/tmp")), vec![b"x".to_vec()]);
        let level = root.level(b"/tmp/x");
        assert_eq!(names(level), vec![b"src".to_vec()]);
        assert!(VirtualRoot::lookup(level, b"src")
            .unwrap()
            .entry()
            .is_some());
    }

    #[test]
    fn many_roots_share_synthetic_levels() {
        let root = VirtualRoot::build(&[
            file_entry("/home/a/data"),
            file_entry("/home/b/data"),
            file_entry("/etc/config"),
        ]);
        let mut top = names(root.top());
        top.sort();
        assert_eq!(top, vec![b"etc".to_vec(), b"home".to_vec()]);
        assert_eq!(
            names(root.level(b"/home")),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert_eq!(names(root.level(b"/home/a")), vec![b"data".to_vec()]);
        assert_eq!(names(root.level(b"/home/b")), vec![b"data".to_vec()]);
    }

    #[test]
    fn real_entry_beats_synthetic_of_same_name() {
        // 先讓 /tmp/x/src 造出合成 tmp，再放一個真實的 /tmp（例如只備份了 /tmp 本身
        // 與 /tmp/x/src 兩個路徑）：真實 tmp 必須取代合成 tmp。
        let root = VirtualRoot::build(&[file_entry("/tmp/x/src"), dir_entry("/tmp")]);
        let top = root.top();
        assert_eq!(names(top), vec![b"tmp".to_vec()]);
        assert!(
            top[0].entry().is_some(),
            "頂層 tmp 必須是真實條目，不是被替換掉的合成目錄"
        );
    }

    #[test]
    fn duplicate_real_names_do_not_duplicate_entries() {
        let root = VirtualRoot::build(&[file_entry("/tmp/x/a"), file_entry("/tmp/x/b")]);
        assert_eq!(names(root.top()), vec![b"tmp".to_vec()]);
        assert_eq!(names(root.level(b"/tmp")), vec![b"x".to_vec()]);
        assert_eq!(
            names(root.level(b"/tmp/x")),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn levels_are_sorted_for_binary_search() {
        let root = VirtualRoot::build(&[file_entry("/z"), file_entry("/a"), file_entry("/m")]);
        assert_eq!(
            names(root.top()),
            vec![b"a".to_vec(), b"m".to_vec(), b"z".to_vec()]
        );
        assert!(VirtualRoot::lookup(root.top(), b"m").is_some());
        assert!(VirtualRoot::lookup(root.top(), b"b").is_none());
        assert!(VirtualRoot::lookup(root.top(), b"zz").is_none());
    }

    #[test]
    fn non_utf8_names_survive() {
        // 檔名是原始 OS bytes：0xFF 不是合法 UTF-8，只能原樣保留
        let mut name = b"/tmp/".to_vec();
        name.extend_from_slice(&[0xFF, 0xFE]);
        let root = VirtualRoot::build(&[Entry {
            name,
            ..file_entry("")
        }]);
        assert_eq!(names(root.top()), vec![b"tmp".to_vec()]);
        assert!(VirtualRoot::lookup(root.level(b"/tmp"), &[0xFF, 0xFE]).is_some());
    }

    #[test]
    fn root_slash_entry_is_skipped_without_panicking() {
        // `kist backup /` 的根條目名稱就是 "/"：切不出組件，跳過（corefs 會先把
        // 它的子樹攤平到頂層）；這裡只驗證不 panic、不產生幽靈條目。
        let root = VirtualRoot::build(&[dir_entry("/")]);
        assert!(root.top().is_empty());
    }

    #[test]
    fn slash_only_and_empty_components_are_normalized() {
        let root = VirtualRoot::build(&[file_entry("//tmp//x//src/")]);
        assert_eq!(names(root.top()), vec![b"tmp".to_vec()]);
        assert_eq!(names(root.level(b"/tmp")), vec![b"x".to_vec()]);
        assert_eq!(names(root.level(b"/tmp/x")), vec![b"src".to_vec()]);
    }
}
