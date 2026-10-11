//! §9：snapshot、Root、定位的切段與 restore 的落點，以及 roots 的清單（RootSeg，`k=3`）。
//!
//! ```text
//! Snapshot { v: 4, roots: meta ID（RootSeg 或 Dir）, time: i64 奈秒（backup 開始）, host: text,
//!            user: text（省略若空）, client: bytes(16), parent: text（省略若沒有）, stats }
//! Root     { path: byte string, t: u8, tree: meta ID（t=1）, entry: Entry（t=0 或 2） }
//! ```
//!
//! snapshot 以 `K_snap` 封、AAD 是 key 路徑（`kist-crypto` 的 `v4`）。內容裡的 client 與時間必須和 key 相同
//! （[`Snapshot::check_key`]）。

use serde::{Deserialize, Serialize};

use super::ids::{ClientId, MetaId};
use super::keys::Key;
use super::meta::{build_dir_levels, kind, node_type, segment, Built, DirSeg, Entry, Item, Limits};
use super::{check_version, corrupt, invalid, peek_version, Result, VERSION};
use crate::cbor;

/// `..` 映射成的名稱（§9）。
pub const PARENT_SEGMENT: &[u8] = b"__parent__";

/// 一個備份來源：§4 的欄位順序 path, t, tree, entry。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    /// 來源定位（bytes）：本機絕對路徑、`sftp://host[:port]/path`、`s3://bucket/prefix`。
    #[serde(with = "serde_bytes")]
    pub path: Vec<u8>,
    /// 來源本身的類型：[`node_type`] 的 0 檔案、1 目錄、2 符號連結。
    pub t: u8,
    /// `t=1`：目錄內容的 meta ID（Seg 或 Dir）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<MetaId>,
    /// `t=0` 或 `2`：來源本身的 Entry。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Entry>,
}

impl Root {
    /// §9 的種類規則：`t=1` 有 `tree`、沒有 `entry`；`t=0` 或 `2` 有 `entry`、沒有 `tree`，`entry.t` 等於
    /// `t`，`entry.n` 等於定位的最後一段（所以定位至少要有一段）。
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: String| Err(corrupt("root", msg));
        if self.path.is_empty() {
            return bad("empty path".into());
        }
        let segs = split_locator(&self.path)?;
        match self.t {
            node_type::DIR => {
                if self.tree.is_none() || self.entry.is_some() {
                    return bad("a directory root has a tree and no entry".into());
                }
            }
            node_type::FILE | node_type::SYMLINK => {
                let Some(e) = &self.entry else {
                    return bad("a file or symlink root needs its entry".into());
                };
                if self.tree.is_some() {
                    return bad("a file or symlink root has no tree".into());
                }
                e.validate()?;
                if e.t != self.t {
                    return bad(format!("root t {} but entry t {}", self.t, e.t));
                }
                match segs.last() {
                    Some(last) if *last == e.n => {}
                    Some(_) => return bad("entry name is not the last path segment".into()),
                    None => return bad("a file or symlink root needs a path segment".into()),
                }
            }
            other => return bad(format!("unknown root type {other}")),
        }
        Ok(())
    }

    /// restore 的落點（相對於 restore 的目標）：`t=1` 的內容放在所有段組成的目錄；`t=0` 或 `2` 放在去掉最後
    /// 一段的目錄、名稱是 `entry.n`。
    pub fn placement(&self) -> Result<Placement> {
        self.validate()?;
        let mut dir = split_locator(&self.path)?;
        let name = if self.t == node_type::DIR {
            None
        } else {
            dir.pop()
        };
        Ok(Placement { dir, name })
    }
}

/// restore 的落點。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// 相對於 restore 目標的目錄（各段）。
    pub dir: Vec<Vec<u8>>,
    /// 檔案或符號連結的名稱；目錄 root 沒有。
    pub name: Option<Vec<u8>>,
}

/// §9 的定位切段：有 `scheme://`（字母開頭，後面是字母、數字、`+`、`.`、`-`）就去掉；以 `/` 切，空段與 `.`
/// 丟掉，`..` 映射成 [`PARENT_SEGMENT`]；段裡有 NUL 是腐壞。
pub fn split_locator(path: &[u8]) -> Result<Vec<Vec<u8>>> {
    let rest = strip_scheme(path);
    let mut out = Vec::new();
    for seg in rest.split(|b| *b == b'/') {
        if seg.contains(&0) {
            return Err(corrupt("root", "a path segment contains NUL"));
        }
        match seg {
            b"" | b"." => {}
            b".." => out.push(PARENT_SEGMENT.to_vec()),
            s => out.push(s.to_vec()),
        }
    }
    Ok(out)
}

fn strip_scheme(path: &[u8]) -> &[u8] {
    let Some(pos) = path.windows(3).position(|w| w == b"://") else {
        return path;
    };
    let scheme = &path[..pos];
    let ok = scheme.first().is_some_and(u8::is_ascii_alphabetic)
        && scheme
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'));
    if ok {
        &path[pos + 3..]
    } else {
        path
    }
}

/// snapshot 的 roots 的一段：§4 的欄位順序 v, k, roots。roots 依 `path` bytes 升冪、不重複。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootSeg {
    pub v: u32,
    pub k: u8,
    pub roots: Vec<Root>,
}

impl RootSeg {
    pub fn new(roots: Vec<Root>) -> Self {
        Self {
            v: VERSION,
            k: kind::ROOT_SEG,
            roots,
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        for r in &self.roots {
            r.validate()?;
        }
        let mut prev: Option<&[u8]> = None;
        for r in &self.roots {
            if prev.is_some_and(|p| p >= r.path.as_slice()) {
                return Err(corrupt(
                    "roots",
                    "paths must be ascending with no duplicates",
                ));
            }
            prev = Some(&r.path);
        }
        Ok(())
    }
}

/// 寫入端：snapshot 的 roots（依 `path` 升冪、不重複、至少一個）→ 一個 RootSeg，或多段時的 RootSeg 與最多
/// 三層的 Dir（`first` 是那一段第一個 root 的 path）。
pub fn build_roots(
    roots: Vec<Root>,
    limits: Limits,
    is_cut: &dyn Fn(&[u8]) -> bool,
    meta_id: &dyn Fn(&[u8]) -> MetaId,
) -> Result<Built> {
    if roots.is_empty() {
        return Err(invalid("roots", "a snapshot has at least one root"));
    }
    RootSeg::new(roots.clone())
        .check()
        .map_err(|e| invalid("roots", e.to_string()))?;
    let base = cbor::encode(&RootSeg::new(Vec::new()))?.len() - 1;
    let segs = segment(
        roots,
        base,
        limits,
        |r| Ok(cbor::encode(r)?.len()),
        |r| r.path.as_slice(),
        is_cut,
    )?;
    let mut items = Vec::new();
    let single = segs.len() == 1;
    let mut dir_segs = Vec::with_capacity(segs.len());
    for s in segs {
        let first = s.first().map(|r| r.path.clone()).unwrap_or_default();
        let bytes = Item::RootSeg(RootSeg::new(s)).encode()?;
        let id = meta_id(&bytes);
        items.push((id, bytes));
        if single {
            return Ok(Built { root: id, items });
        }
        dir_segs.push(DirSeg { first, id });
    }
    let root = build_dir_levels(dir_segs, limits, is_cut, meta_id, &mut items)?;
    Ok(Built { root, items })
}

/// 備份統計：§4 的欄位順序 files, dirs, symlinks, bytes（零值省略）。`files`、`symlinks` 按名稱計；`dirs` 是
/// 目錄節點數、roots 不算；`bytes` 是內容總和，`(dev, ino)` 只算一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SnapshotStats {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dirs: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub symlinks: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// commit 記錄：§4 的欄位順序 v, roots, time, host, user, client, parent, stats。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub v: u32,
    /// roots 的清單：RootSeg（一段）或 Dir（多段）的 meta ID。
    pub roots: MetaId,
    /// backup 開始的時間（Unix 奈秒），與 key 的時間戳相同。
    pub time: i64,
    pub host: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    pub client: ClientId,
    /// 同一個 client、roots 的路徑與 `t` 都相同的上一個 snapshot 的 key（只用於加速）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub stats: SnapshotStats,
}

impl Snapshot {
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()
            .map_err(|e| invalid("snapshot", e.to_string()))?;
        Ok(cbor::encode(self)?)
    }

    /// 讀取端：版本不是 4 就拒絕（較舊的 prune 遇到較新的 snapshot 也是在這裡停下，§11.3 P5）。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        peek_version("snapshot", bytes)?;
        let s: Snapshot = cbor::decode(bytes)?;
        s.validate()?;
        Ok(s)
    }

    fn validate(&self) -> Result<()> {
        check_version("snapshot", self.v)?;
        if let Some(p) = &self.parent {
            match Key::parse(p)? {
                Key::Snapshot {
                    client,
                    replica: false,
                    ..
                } if client == self.client => {}
                _ => {
                    return Err(corrupt(
                        "snapshot",
                        format!("parent {p:?} is not a snapshot of the same client"),
                    ))
                }
            }
        }
        Ok(())
    }

    /// 內容與 key 的核對：client 與時間都必須和 key 相同（副本 `.r1` 也一樣）。
    pub fn check_key(&self, key: &Key) -> Result<()> {
        match key {
            Key::Snapshot { client, ts, .. }
                if *client == self.client && ts.nanos() == self.time =>
            {
                Ok(())
            }
            _ => Err(corrupt(
                "snapshot",
                format!("content does not match its key {}", key.path()),
            )),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::v4::keys::SnapTs;
    use crate::v4::meta::{check_dir_child, decode_item, meta_kind};
    use crate::v4::Error;

    fn file_entry(name: &str) -> Entry {
        let mut e = Entry::new(
            name.as_bytes().to_vec(),
            node_type::FILE,
            meta_kind::GENERIC,
        );
        e.mtime = Some(1);
        e
    }

    fn dir_root(path: &str) -> Root {
        Root {
            path: path.as_bytes().to_vec(),
            t: node_type::DIR,
            tree: Some(MetaId::from_bytes([1; 32])),
            entry: None,
        }
    }

    fn file_root(path: &str, name: &str) -> Root {
        Root {
            path: path.as_bytes().to_vec(),
            t: node_type::FILE,
            tree: None,
            entry: Some(file_entry(name)),
        }
    }

    fn segs(path: &str) -> Vec<String> {
        split_locator(path.as_bytes())
            .unwrap()
            .into_iter()
            .map(|s| String::from_utf8(s).unwrap())
            .collect()
    }

    #[test]
    fn locators_split_as_specified() {
        assert_eq!(segs("/srv/data"), ["srv", "data"]);
        assert_eq!(segs("s3://bucket/prefix"), ["bucket", "prefix"]);
        assert_eq!(segs("/"), Vec::<String>::new());
        assert_eq!(segs("a/./b//c/"), ["a", "b", "c"]);
        assert_eq!(segs("/x/../y"), ["x", "__parent__", "y"]);
        assert_eq!(segs("sftp://host:22/srv"), ["host:22", "srv"]);
        assert_eq!(segs("s3+x.y-z://b"), ["b"]);
        // 不是 scheme 的 :// 照樣切。
        assert_eq!(segs("/a://b"), ["a:", "b"]);
        assert_eq!(segs("1x://b"), ["1x:", "b"]);
        assert!(split_locator(b"/a\0b").is_err());
    }

    #[test]
    fn root_kind_rules() {
        dir_root("/srv/data").validate().unwrap();
        dir_root("/").validate().unwrap();
        file_root("/etc/hosts", "hosts").validate().unwrap();
        let mut link = file_root("/etc/link", "link");
        link.t = node_type::SYMLINK;
        let e = link.entry.as_mut().unwrap();
        e.t = node_type::SYMLINK;
        e.target = b"/x".to_vec();
        link.validate().unwrap();

        let mut cases: Vec<(&str, Root)> = Vec::new();
        let mut r = dir_root("/srv");
        r.tree = None;
        cases.push(("dir without tree", r));
        let mut r = dir_root("/srv");
        r.entry = Some(file_entry("srv"));
        cases.push(("dir with entry", r));
        let mut r = file_root("/etc/hosts", "hosts");
        r.entry = None;
        cases.push(("file without entry", r));
        let mut r = file_root("/etc/hosts", "hosts");
        r.tree = Some(MetaId::from_bytes([1; 32]));
        cases.push(("file with tree", r));
        cases.push(("entry name differs", file_root("/etc/hosts", "other")));
        let mut r = file_root("/", "x");
        r.path = b"/".to_vec();
        cases.push(("file root without a segment", r));
        let mut r = file_root("/etc/hosts", "hosts");
        r.t = node_type::SYMLINK; // entry 還是檔案
        cases.push(("root t differs from entry t", r));
        let mut r = dir_root("/srv");
        r.t = 3;
        cases.push(("unknown t", r));
        cases.push(("empty path", dir_root("")));
        cases.push(("NUL in path", dir_root("/a\0b")));
        for (what, r) in cases {
            assert!(matches!(r.validate(), Err(Error::Corrupt { .. })), "{what}");
        }
    }

    #[test]
    fn restore_placement() {
        let p = |dir: &[&str], name: Option<&str>| Placement {
            dir: dir.iter().map(|d| d.as_bytes().to_vec()).collect(),
            name: name.map(|n| n.as_bytes().to_vec()),
        };
        assert_eq!(
            dir_root("/srv/data").placement().unwrap(),
            p(&["srv", "data"], None)
        );
        assert_eq!(dir_root("/").placement().unwrap(), p(&[], None));
        assert_eq!(
            file_root("/etc/hosts", "hosts").placement().unwrap(),
            p(&["etc"], Some("hosts"))
        );
        assert_eq!(
            file_root("s3://bucket/key", "key").placement().unwrap(),
            p(&["bucket"], Some("key"))
        );
    }

    fn sample(parent: Option<String>) -> Snapshot {
        Snapshot {
            v: VERSION,
            roots: MetaId::from_bytes([2; 32]),
            time: 1_788_523_200_123_456_789,
            host: "h".into(),
            user: String::new(),
            client: ClientId::from_bytes([0x1f; 16]),
            parent,
            stats: SnapshotStats {
                files: 3,
                dirs: 1,
                symlinks: 0,
                bytes: 10,
            },
        }
    }

    fn key_of(s: &Snapshot) -> Key {
        Key::Snapshot {
            client: s.client,
            ts: SnapTs::from_nanos(s.time).unwrap(),
            replica: false,
        }
    }

    #[test]
    fn snapshots_round_trip_and_check_their_key() {
        let parent = Key::Snapshot {
            client: ClientId::from_bytes([0x1f; 16]),
            ts: SnapTs::from_nanos(1).unwrap(),
            replica: false,
        }
        .path();
        for s in [sample(None), sample(Some(parent.clone()))] {
            let bytes = s.encode().unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("user"), "an empty user is omitted");
            assert_eq!(text.contains("parent"), s.parent.is_some());
            assert!(!text.contains("symlinks"), "zero stats are omitted");
            let order = ["roots", "time", "host", "client", "stats"];
            let pos: Vec<usize> = order.iter().map(|k| text.find(k).unwrap()).collect();
            assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");
            let back = Snapshot::decode(&bytes).unwrap();
            assert_eq!(back, s);
            back.check_key(&key_of(&s)).unwrap();
            let mut r1 = key_of(&s);
            if let Key::Snapshot { replica, .. } = &mut r1 {
                *replica = true;
            }
            back.check_key(&r1).unwrap();
        }
        let s = sample(None);
        // key 的 client 或時間不同。
        let other_client = Key::Snapshot {
            client: ClientId::MAINT,
            ts: SnapTs::from_nanos(s.time).unwrap(),
            replica: false,
        };
        assert!(s.check_key(&other_client).is_err());
        let other_time = Key::Snapshot {
            client: s.client,
            ts: SnapTs::from_nanos(s.time + 1).unwrap(),
            replica: false,
        };
        assert!(s.check_key(&other_time).is_err());
        assert!(s.check_key(&Key::Config).is_err());
    }

    #[test]
    fn bad_snapshots_are_rejected() {
        let mut v5 = sample(None);
        v5.v = 5;
        assert!(matches!(
            Snapshot::decode(&cbor::encode(&v5).unwrap()),
            Err(Error::Version { version: 5, .. })
        ));
        let foreign = Key::Snapshot {
            client: ClientId::MAINT,
            ts: SnapTs::from_nanos(1).unwrap(),
            replica: false,
        }
        .path();
        for p in [foreign, "packs/x".into(), "snapshots/oops".into()] {
            let s = sample(Some(p.clone()));
            assert!(Snapshot::decode(&cbor::encode(&s).unwrap()).is_err(), "{p}");
            assert!(s.encode().is_err(), "{p}");
        }
        // 明寫的 null parent 解成沒有 parent。
        #[derive(Serialize)]
        struct WithNull<'a> {
            v: u32,
            roots: MetaId,
            time: i64,
            host: &'a str,
            client: ClientId,
            parent: Option<String>,
        }
        let s = sample(None);
        let bytes = cbor::encode(&WithNull {
            v: 4,
            roots: s.roots,
            time: s.time,
            host: "h",
            client: s.client,
            parent: None,
        })
        .unwrap();
        assert_eq!(Snapshot::decode(&bytes).unwrap().parent, None);
    }

    #[test]
    fn roots_build_into_one_or_more_segments_that_read_back() {
        let fake = |b: &[u8]| MetaId::from_bytes(*blake3::hash(b).as_bytes());
        let never = |_: &[u8]| false;
        let roots: Vec<Root> = (0..40).map(|i| dir_root(&format!("/r{i:03}"))).collect();
        let one = build_roots(roots.clone(), Limits::V4, &never, &fake).unwrap();
        assert_eq!(one.items.len(), 1);
        assert!(matches!(
            decode_item(&one.items[0].1).unwrap(),
            Item::RootSeg(_)
        ));
        // 上限小到每段 4 個 root：多段，以 Dir 索引。
        let limits = Limits {
            min_items: 1_000_000,
            max_bytes: 260,
        };
        let many = build_roots(roots.clone(), limits, &never, &fake).unwrap();
        let items: std::collections::HashMap<_, _> = many
            .items
            .iter()
            .map(|(id, b)| (*id, decode_item(b).unwrap()))
            .collect();
        let mut got = Vec::new();
        fn walk(id: MetaId, items: &std::collections::HashMap<MetaId, Item>, out: &mut Vec<Root>) {
            match &items[&id] {
                Item::RootSeg(s) => out.extend(s.roots.iter().cloned()),
                Item::Dir(d) => {
                    for (i, s) in d.segs.iter().enumerate() {
                        check_dir_child(d, i, &items[&s.id], kind::ROOT_SEG).unwrap();
                        walk(s.id, items, out);
                    }
                }
                other => panic!("{other:?}"),
            }
        }
        walk(many.root, &items, &mut got);
        assert_eq!(got, roots);
        assert!(matches!(items[&many.root], Item::Dir(_)));
        // 沒有 root、沒排好、重複都拒絕。
        assert!(build_roots(Vec::new(), Limits::V4, &never, &fake).is_err());
        let mut unsorted = roots.clone();
        unsorted.swap(0, 1);
        assert!(build_roots(unsorted, Limits::V4, &never, &fake).is_err());
        let dup = vec![dir_root("/a"), dir_root("/a")];
        assert!(build_roots(dup, Limits::V4, &never, &fake).is_err());
        // Dir 第一層指向的是 Seg 而不是 RootSeg：種類不符。
        let Item::Dir(d) = &items[&many.root] else {
            unreachable!()
        };
        let first = d.segs[0].first.clone();
        let mut e = file_entry("x");
        e.n = first;
        let seg = Item::Seg(crate::v4::meta::Seg::new(vec![e]));
        let level1 = match &items[&d.segs[0].id] {
            Item::Dir(x) => x.clone(),
            _ => d.clone(),
        };
        assert!(check_dir_child(&level1, 0, &seg, kind::ROOT_SEG).is_err());
    }
}
