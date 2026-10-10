//! §7、§8：metadata item 與 Entry。
//!
//! 每個 item 是 `alg byte ‖ CBOR`，以 `K_meta` 封、AAD 是自己的 meta ID；`k` 標示種類，在受認證的明文裡：
//!
//! | k | item | 內容 |
//! | --- | --- | --- |
//! | 0 | [`Seg`] | 一個目錄的一段，entries 依名稱 bytes 升冪、不重複 |
//! | 1 | [`Dir`] | 多段時的索引；`level=1` 指向段，`level=2`、`3` 指向下一層 Dir |
//! | 2 | [`ChunkList`] | 檔案的 chunk 超過 256 個時的清單，或清單的一段 |
//! | 3 | [`RootSeg`] | snapshot 的 roots 的一段（[`super::snapshot`]） |
//! | 4 | [`ChunkIndex`] | chunk 清單多段時的索引，每段帶涵蓋的明文 bytes |
//!
//! 切段（[`segment`]）、建構（[`build_dir`]、[`build_chunks`]）與讀取端驗證都在這裡；需要金鑰的兩件事
//! （切段條件用的 `K_cut`、meta ID 用的 `K_meta_id`）以閉包傳入，這個 crate 不持有金鑰。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

use super::ids::{ChunkId, ItemId, MetaId};
pub use super::snapshot::RootSeg;
use super::{check_version, corrupt, invalid, Result, VERSION};
use crate::cbor;

/// 單一 item 的規範 CBOR 上限（也是 mpack 裡一項的明文上限）。
pub const MAX_ITEM_BYTES: usize = 2 * 1024 * 1024;
/// 一段至少這麼多項之後，才看切段條件。
pub const MIN_SEG_ITEMS: usize = 1024;
/// Dir 與 ChunkIndex 最多三層。
pub const MAX_LEVEL: u8 = 3;
/// `ct=0` 的 chunks 上限；超過就改用 ChunkList／ChunkIndex。
pub const MAX_INLINE_CHUNKS: usize = 256;
/// 一個 entry 的 xattrs 合計（名稱＋值的 bytes）上限。
pub const MAX_XATTR_BYTES: usize = 256 * 1024;
/// s3 entry 的 etag 與 vern 各自的上限（與 v3 相同）。
pub const MAX_ETAG_VER_BYTES: usize = 1024;

/// item 的種類（`k`）。
pub mod kind {
    pub const SEG: u8 = 0;
    pub const DIR: u8 = 1;
    pub const CHUNK_LIST: u8 = 2;
    pub const ROOT_SEG: u8 = 3;
    pub const CHUNK_INDEX: u8 = 4;
}

/// 節點類型（Entry 的 `t`，也是 Root 的 `t`）。
pub mod node_type {
    pub const FILE: u8 = 0;
    pub const DIR: u8 = 1;
    pub const SYMLINK: u8 = 2;
}

/// 內容形式（`ct`）。
pub mod content_type {
    /// `chunks` 就是檔案內容的 chunk ID（省略時也是這個意思），最多 [`super::MAX_INLINE_CHUNKS`] 個。
    pub const DIRECT: u8 = 0;
    /// `chunks` 恰好一個元素：ChunkList（只有一段）或 ChunkIndex（多段）的 meta ID。
    pub const INDIRECT: u8 = 1;
}

/// metadata 種類（`mk`），決定欄位的必填、選填與必須缺席（§8 的表）。
pub mod meta_kind {
    pub const POSIX: u8 = 0;
    pub const SFTP: u8 = 1;
    pub const S3: u8 = 2;
    pub const GENERIC: u8 = 3;
}

/// 目錄裡的一個名字：§4 的欄位順序 n, t, mk, size, target, ct, chunks, tree, mode, uid, gid, mtime, ctime,
/// dev, ino, nlink, xattrs, etag, vern（與 v3 相同；只差 `tree` 指向 Seg 或 Dir，`ct=1` 的 `chunks` 是清單的
/// meta ID）。Option＝缺席，零值＝省略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 名稱（原始 OS bytes），一律是單一路徑元件。
    #[serde(with = "serde_bytes")]
    pub n: Vec<u8>,
    /// [`node_type`]。
    pub t: u8,
    /// [`meta_kind`]。
    pub mk: u8,
    /// 檔案內容的長度（只有檔案；0 省略）。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub size: u64,
    /// 符號連結的目標（只有符號連結）。
    #[serde(default, with = "serde_bytes", skip_serializing_if = "Vec::is_empty")]
    pub target: Vec<u8>,
    /// [`content_type`]；省略＝直接。
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub ct: u8,
    /// `ct=0`：chunk ID；`ct=1`：一個 meta ID。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ItemId>,
    /// 子目錄內容的 meta ID（只有目錄）：Seg（一段）或 Dir（多段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<MetaId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// 修改時間（奈秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    /// inode 變更時間（奈秒，只有 posix）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctime: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ino: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nlink: Option<u64>,
    /// 擴充屬性（只有 posix）：key 依 bytes 字典序（BTreeMap），重複的 key 拒絕。
    #[serde(
        default,
        skip_serializing_if = "xattrs_absent",
        deserialize_with = "de_xattrs_strict"
    )]
    pub xattrs: Option<BTreeMap<ByteBuf, ByteBuf>>,
    #[serde(default, with = "serde_bytes", skip_serializing_if = "bytes_absent")]
    pub etag: Option<ByteBuf>,
    #[serde(default, with = "serde_bytes", skip_serializing_if = "bytes_absent")]
    pub vern: Option<ByteBuf>,
}

impl Entry {
    /// 只有名稱、類型與 metadata 種類，其餘缺席。
    pub fn new(n: Vec<u8>, t: u8, mk: u8) -> Self {
        Self {
            n,
            t,
            mk,
            size: 0,
            target: Vec::new(),
            ct: content_type::DIRECT,
            chunks: Vec::new(),
            tree: None,
            mode: None,
            uid: None,
            gid: None,
            mtime: None,
            ctime: None,
            dev: None,
            ino: None,
            nlink: None,
            xattrs: None,
            etag: None,
            vern: None,
        }
    }

    /// §8 的結構與 per-kind 規則，以及 §7 的上限。讀取端一律檢查。
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: String| Err(corrupt("entry", msg));
        if !is_clean_component(&self.n) {
            return bad(format!(
                "name {:?} is not a single clean path component",
                self.n
            ));
        }
        if self.ct > content_type::INDIRECT {
            return bad(format!("unknown content type {}", self.ct));
        }
        match self.t {
            node_type::FILE => {
                if !self.target.is_empty() || self.tree.is_some() {
                    return bad("a file carries no target or tree".into());
                }
            }
            node_type::DIR => {
                if self.size != 0
                    || !self.target.is_empty()
                    || !self.chunks.is_empty()
                    || self.ct != content_type::DIRECT
                    || self.tree.is_none()
                {
                    return bad("a directory carries a tree and nothing else".into());
                }
            }
            node_type::SYMLINK => {
                if self.target.is_empty()
                    || self.size != 0
                    || !self.chunks.is_empty()
                    || self.ct != content_type::DIRECT
                    || self.tree.is_some()
                {
                    return bad("a symlink carries a target and nothing else".into());
                }
            }
            other => return bad(format!("unknown node type {other}")),
        }
        match self.ct {
            content_type::DIRECT if self.chunks.len() > MAX_INLINE_CHUNKS => {
                return bad(format!(
                    "{} inline chunks, over {MAX_INLINE_CHUNKS} (must be indirect)",
                    self.chunks.len()
                ))
            }
            content_type::INDIRECT if self.chunks.len() != 1 => {
                return bad(format!(
                    "an indirect file has {} chunks, not exactly one list",
                    self.chunks.len()
                ))
            }
            _ => {}
        }
        let posix_only = self.ctime.is_some()
            || self.dev.is_some()
            || self.ino.is_some()
            || self.nlink.is_some()
            || self.xattrs.is_some();
        let owner = self.mode.is_some() || self.uid.is_some() || self.gid.is_some();
        let s3_only = self.etag.is_some() || self.vern.is_some();
        match self.mk {
            meta_kind::POSIX => {
                if self.mode.is_none()
                    || self.uid.is_none()
                    || self.gid.is_none()
                    || self.mtime.is_none()
                {
                    return bad("posix requires mode, uid, gid and mtime".into());
                }
            }
            meta_kind::SFTP => {
                if self.mtime.is_none() {
                    return bad("sftp requires mtime".into());
                }
                if posix_only || s3_only {
                    return bad("sftp carries no posix-only or s3 fields".into());
                }
            }
            meta_kind::S3 => {
                if owner || posix_only {
                    return bad("s3 carries no posix fields".into());
                }
                for (what, v) in [("etag", &self.etag), ("vern", &self.vern)] {
                    if v.as_ref().is_some_and(|b| b.len() > MAX_ETAG_VER_BYTES) {
                        return bad(format!("{what} exceeds {MAX_ETAG_VER_BYTES} bytes"));
                    }
                }
            }
            meta_kind::GENERIC => {
                if owner || posix_only || s3_only {
                    return bad("generic carries mtime only".into());
                }
            }
            other => return bad(format!("unknown metadata kind {other}")),
        }
        if let Some(x) = &self.xattrs {
            let total: usize = x.iter().map(|(k, v)| k.len() + v.len()).sum();
            if total > MAX_XATTR_BYTES {
                return bad(format!(
                    "xattrs total {total} bytes, over {MAX_XATTR_BYTES}"
                ));
            }
        }
        Ok(())
    }
}

/// 單一路徑元件：非空、沒有 `/` 與 NUL、不是 `.` 或 `..`。
pub fn is_clean_component(n: &[u8]) -> bool {
    !n.is_empty() && !n.contains(&b'/') && !n.contains(&0) && n != b"." && n != b".."
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

fn xattrs_absent(v: &Option<BTreeMap<ByteBuf, ByteBuf>>) -> bool {
    v.as_ref().is_none_or(BTreeMap::is_empty)
}

fn bytes_absent(v: &Option<ByteBuf>) -> bool {
    v.as_ref().is_none_or(|b| b.is_empty())
}

/// xattrs 的嚴格解碼：重複的 key 是偽造或損壞（§4 第 8 條）；serde 的 BTreeMap 會默默保留最後一個。
fn de_xattrs_strict<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<BTreeMap<ByteBuf, ByteBuf>>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Option<BTreeMap<ByteBuf, ByteBuf>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a map of xattrs")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut a: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut m = BTreeMap::new();
            while let Some((k, v)) = a.next_entry::<ByteBuf, ByteBuf>()? {
                if m.insert(k, v).is_some() {
                    return Err(serde::de::Error::custom("duplicate xattr key"));
                }
            }
            Ok(Some(m))
        }
    }
    d.deserialize_map(V)
}

/// 一個目錄的一段：§4 的欄位順序 v, k, entries。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seg {
    pub v: u32,
    pub k: u8,
    pub entries: Vec<Entry>,
}

impl Seg {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self {
            v: VERSION,
            k: kind::SEG,
            entries,
        }
    }

    fn check(&self) -> Result<()> {
        for e in &self.entries {
            e.validate()?;
        }
        check_ascending("segment", self.entries.iter().map(|e| e.n.as_slice()))
    }
}

/// 多段時的索引：§4 的欄位順序 v, k, level, segs。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dir {
    pub v: u32,
    pub k: u8,
    pub level: u8,
    pub segs: Vec<DirSeg>,
}

/// §4 的欄位順序 first, id。`first` 是那一段第一項的名稱（目錄）或路徑（roots）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirSeg {
    #[serde(with = "serde_bytes")]
    pub first: Vec<u8>,
    pub id: MetaId,
}

impl Dir {
    pub fn new(level: u8, segs: Vec<DirSeg>) -> Self {
        Self {
            v: VERSION,
            k: kind::DIR,
            level,
            segs,
        }
    }

    fn check(&self) -> Result<()> {
        check_level("directory index", self.level)?;
        if self.segs.is_empty() {
            return Err(corrupt("directory index", "no segments"));
        }
        check_ascending(
            "directory index",
            self.segs.iter().map(|s| s.first.as_slice()),
        )
    }
}

/// chunk 清單（或它的一段）：§4 的欄位順序 v, k, chunks。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkList {
    pub v: u32,
    pub k: u8,
    pub chunks: Vec<ChunkId>,
}

impl ChunkList {
    pub fn new(chunks: Vec<ChunkId>) -> Self {
        Self {
            v: VERSION,
            k: kind::CHUNK_LIST,
            chunks,
        }
    }

    fn check(&self) -> Result<()> {
        if self.chunks.is_empty() {
            return Err(corrupt("chunk list", "empty"));
        }
        Ok(())
    }
}

/// chunk 清單多段時的索引：§4 的欄位順序 v, k, level, parts。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkIndex {
    pub v: u32,
    pub k: u8,
    pub level: u8,
    pub parts: Vec<ChunkPart>,
}

/// §4 的欄位順序 size, id。`size` 是那一段涵蓋的 chunk 明文長度合計。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkPart {
    pub size: u64,
    pub id: MetaId,
}

impl ChunkIndex {
    pub fn new(level: u8, parts: Vec<ChunkPart>) -> Self {
        Self {
            v: VERSION,
            k: kind::CHUNK_INDEX,
            level,
            parts,
        }
    }

    fn check(&self) -> Result<()> {
        check_level("chunk index", self.level)?;
        if self.parts.is_empty() {
            return Err(corrupt("chunk index", "no parts"));
        }
        if self.parts.iter().any(|p| p.size == 0) {
            return Err(corrupt("chunk index", "a part covers no bytes"));
        }
        self.total_size().map(|_| ())
    }

    /// 全部 `size` 的合計，必須等於 Entry 的 `size`（或上一層那一段的 `size`）。
    pub fn total_size(&self) -> Result<u64> {
        self.parts
            .iter()
            .try_fold(0u64, |acc, p| acc.checked_add(p.size))
            .ok_or_else(|| corrupt("chunk index", "sizes overflow"))
    }
}

/// 解碼出來的 metadata item。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Seg(Seg),
    Dir(Dir),
    ChunkList(ChunkList),
    RootSeg(RootSeg),
    ChunkIndex(ChunkIndex),
}

#[derive(Deserialize)]
struct Header {
    v: u32,
    k: u8,
}

/// 讀取端：解碼一個 item 的規範 CBOR（解密、解壓之後）並檢查它自己。跨 item 的檢查見 [`check_dir_child`]、
/// [`check_index_child`]。
pub fn decode_item(bytes: &[u8]) -> Result<Item> {
    if bytes.len() > MAX_ITEM_BYTES {
        return Err(corrupt(
            "metadata item",
            format!("{} bytes, over {MAX_ITEM_BYTES}", bytes.len()),
        ));
    }
    let h: Header = cbor::decode(bytes)?;
    check_version("metadata item", h.v)?;
    let item = match h.k {
        kind::SEG => Item::Seg(cbor::decode(bytes)?),
        kind::DIR => Item::Dir(cbor::decode(bytes)?),
        kind::CHUNK_LIST => Item::ChunkList(cbor::decode(bytes)?),
        kind::ROOT_SEG => Item::RootSeg(cbor::decode(bytes)?),
        kind::CHUNK_INDEX => Item::ChunkIndex(cbor::decode(bytes)?),
        other => return Err(corrupt("metadata item", format!("unknown kind {other}"))),
    };
    item.check()?;
    Ok(item)
}

impl Item {
    /// 寫入端：檢查後編成規範 CBOR；超過 [`MAX_ITEM_BYTES`] 就不寫。
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check()
            .map_err(|e| invalid("metadata item", e.to_string()))?;
        let bytes = match self {
            Item::Seg(x) => cbor::encode(x)?,
            Item::Dir(x) => cbor::encode(x)?,
            Item::ChunkList(x) => cbor::encode(x)?,
            Item::RootSeg(x) => cbor::encode(x)?,
            Item::ChunkIndex(x) => cbor::encode(x)?,
        };
        if bytes.len() > MAX_ITEM_BYTES {
            return Err(invalid(
                "metadata item",
                format!("{} bytes, over {MAX_ITEM_BYTES}", bytes.len()),
            ));
        }
        Ok(bytes)
    }

    fn check(&self) -> Result<()> {
        let (v, k, expect) = match self {
            Item::Seg(x) => (x.v, x.k, kind::SEG),
            Item::Dir(x) => (x.v, x.k, kind::DIR),
            Item::ChunkList(x) => (x.v, x.k, kind::CHUNK_LIST),
            Item::RootSeg(x) => (x.v, x.k, kind::ROOT_SEG),
            Item::ChunkIndex(x) => (x.v, x.k, kind::CHUNK_INDEX),
        };
        check_version("metadata item", v)?;
        if k != expect {
            return Err(corrupt(
                "metadata item",
                format!("kind {k}, expected {expect}"),
            ));
        }
        match self {
            Item::Seg(x) => x.check(),
            Item::Dir(x) => x.check(),
            Item::ChunkList(x) => x.check(),
            Item::RootSeg(x) => x.check(),
            Item::ChunkIndex(x) => x.check(),
        }
    }

    /// 段或索引裡依序的名稱（Dir 的 `first`）；chunk 清單與 ChunkIndex 沒有名稱。
    fn names(&self) -> Option<Vec<&[u8]>> {
        match self {
            Item::Seg(x) => Some(x.entries.iter().map(|e| e.n.as_slice()).collect()),
            Item::Dir(x) => Some(x.segs.iter().map(|s| s.first.as_slice()).collect()),
            Item::RootSeg(x) => Some(x.roots.iter().map(|r| r.path.as_slice()).collect()),
            Item::ChunkList(_) | Item::ChunkIndex(_) => None,
        }
    }
}

fn check_level(what: &'static str, level: u8) -> Result<()> {
    if (1..=MAX_LEVEL).contains(&level) {
        Ok(())
    } else {
        Err(corrupt(
            what,
            format!("level {level} is outside 1..={MAX_LEVEL}"),
        ))
    }
}

/// 名稱嚴格遞增（依 bytes）。
fn check_ascending<'a>(what: &'static str, names: impl Iterator<Item = &'a [u8]>) -> Result<()> {
    let mut prev: Option<&[u8]> = None;
    for n in names {
        if prev.is_some_and(|p| p >= n) {
            return Err(corrupt(what, "names must be ascending with no duplicates"));
        }
        prev = Some(n);
    }
    Ok(())
}

/// 讀取端：Dir 底下第 `i` 段載入之後的檢查。`level=1` 時它必須是 `leaf` 種類的段（目錄是
/// [`kind::SEG`]、roots 是 [`kind::ROOT_SEG`]），`level>1` 時是低一層的 Dir；非空、第一項的名稱等於 Dir 裡的 `first`，而且每一項都小於下一段
/// 的 `first`（目錄依名稱排好之後才切段）。
pub fn check_dir_child(dir: &Dir, i: usize, child: &Item, leaf: u8) -> Result<()> {
    let bad = |msg: String| Err(corrupt("directory index", msg));
    let Some(seg) = dir.segs.get(i) else {
        return bad(format!("no segment {i}"));
    };
    let kind_ok = match child {
        Item::Dir(d) => dir.level > 1 && d.level == dir.level - 1,
        Item::Seg(_) => dir.level == 1 && leaf == kind::SEG,
        Item::RootSeg(_) => dir.level == 1 && leaf == kind::ROOT_SEG,
        Item::ChunkList(_) | Item::ChunkIndex(_) => false,
    };
    if !kind_ok {
        return bad(format!(
            "segment {i} has the wrong kind for level {}",
            dir.level
        ));
    }
    let names = child.names().unwrap_or_default();
    match names.first() {
        None => return bad(format!("segment {i} is empty")),
        Some(n) if *n != seg.first.as_slice() => {
            return bad(format!("segment {i} does not start with its first name"))
        }
        Some(_) => {}
    }
    if let Some(next) = dir.segs.get(i + 1) {
        if names.iter().any(|n| *n >= next.first.as_slice()) {
            return bad(format!("segment {i} reaches into segment {}", i + 1));
        }
    }
    Ok(())
}

/// 讀取端：ChunkIndex 底下第 `i` 段載入之後的檢查。`level=1` 時它必須是 ChunkList，`chunk_bytes` 是呼叫端依
/// map 的記錄算出的那些 chunk 的明文長度合計；`level>1` 時是低一層的 ChunkIndex，比對它的 size 合計。兩者都必須
/// 等於這一段的 `size`。
pub fn check_index_child(
    index: &ChunkIndex,
    i: usize,
    child: &Item,
    chunk_bytes: Option<u64>,
) -> Result<()> {
    let bad = |msg: String| Err(corrupt("chunk index", msg));
    let Some(part) = index.parts.get(i) else {
        return bad(format!("no part {i}"));
    };
    let covered = match (child, chunk_bytes) {
        (Item::ChunkList(_), Some(b)) if index.level == 1 => b,
        (Item::ChunkIndex(c), None) if index.level > 1 && c.level == index.level - 1 => {
            c.total_size()?
        }
        _ => {
            return bad(format!(
                "part {i} has the wrong kind for level {}",
                index.level
            ))
        }
    };
    if covered != part.size {
        return bad(format!(
            "part {i} covers {covered} bytes, the index says {}",
            part.size
        ));
    }
    Ok(())
}

/// 讀取端：一個檔案的內容總長必須等於 Entry 的 `size`。
pub fn check_file_size(entry: &Entry, total: u64) -> Result<()> {
    if total == entry.size {
        Ok(())
    } else {
        Err(corrupt(
            "entry",
            format!("content is {total} bytes, the entry says {}", entry.size),
        ))
    }
}

/// §7 的切段條件：名稱的 keyed BLAKE3(`K_cut`, 名稱)（由持有金鑰的一方算出）前兩個 byte 以 little-endian
/// 讀成整數，低 12 位元全為 0。
pub fn is_cut_hash(hash: &[u8; 32]) -> bool {
    u16::from_le_bytes([hash[0], hash[1]]) & 0x0fff == 0
}

/// 切段的上限。正式值是 [`Limits::V4`]；測試用小的值才測得到多層索引。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub min_items: usize,
    pub max_bytes: usize,
}

impl Limits {
    pub const V4: Limits = Limits {
        min_items: MIN_SEG_ITEMS,
        max_bytes: MAX_ITEM_BYTES,
    };
}

/// CBOR 陣列標頭的長度（最短編碼）。
fn array_head_len(n: usize) -> usize {
    match n {
        0..=23 => 1,
        24..=0xff => 2,
        0x100..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}

/// 一種段在陣列以外的固定長度：把陣列是空的這個結構編碼，減掉空陣列標頭的 1 byte（不經過檢查：Dir 與
/// ChunkIndex 本身不能是空的，這裡只量長度）。
fn base_len<T: Serialize>(empty: &T) -> Result<usize> {
    Ok(cbor::encode(empty)?.len() - 1)
}

/// §7 的切段規則。依順序加入：一段在加入某一項**之前**已有至少 `min_items` 項、而且那一項的名稱讓 `is_cut`
/// 成立，就在這一項之後切；加入下一項會讓這一段的規範 CBOR（`base` ＋ 陣列標頭 ＋ 各項）超過 `max_bytes`
/// 時，在它之前切。一項自己就放不進一段時寫不進去（錯誤）。沒有任何項時回傳一個空段（只有一段的空目錄）。
pub fn segment<T>(
    items: Vec<T>,
    base: usize,
    limits: Limits,
    encoded_len: impl Fn(&T) -> Result<usize>,
    name: impl Fn(&T) -> &[u8],
    is_cut: &dyn Fn(&[u8]) -> bool,
) -> Result<Vec<Vec<T>>> {
    let mut out = Vec::new();
    let mut cur: Vec<T> = Vec::new();
    let mut sum = 0usize;
    for item in items {
        let len = encoded_len(&item)?;
        if base + array_head_len(1) + len > limits.max_bytes {
            return Err(invalid(
                "metadata item",
                format!("one element is {len} bytes, too large for any segment"),
            ));
        }
        if !cur.is_empty() && base + array_head_len(cur.len() + 1) + sum + len > limits.max_bytes {
            out.push(std::mem::take(&mut cur));
            sum = 0;
        }
        let cut = cur.len() >= limits.min_items && is_cut(name(&item));
        cur.push(item);
        sum += len;
        if cut {
            out.push(std::mem::take(&mut cur));
            sum = 0;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// 建好的 metadata：根的 meta ID，以及要寫進 mpack 的 item（meta ID、規範 CBOR），子 item 在前。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub root: MetaId,
    pub items: Vec<(MetaId, Vec<u8>)>,
}

/// 編碼一個 item、算出它的 meta ID、加進要寫的清單。
fn push(
    items: &mut Vec<(MetaId, Vec<u8>)>,
    item: &Item,
    meta_id: &dyn Fn(&[u8]) -> MetaId,
) -> Result<MetaId> {
    let bytes = item.encode()?;
    let id = meta_id(&bytes);
    items.push((id, bytes));
    Ok(id)
}

/// 寫入端：一個目錄的內容（依名稱升冪、不重複）→ 一個 Seg，或多段時的 Seg 與最多三層的 Dir。`is_cut` 是
/// 用 `K_cut` 的切段條件，`meta_id` 是用 `K_meta_id` 的 keyed hash。
pub fn build_dir(
    entries: Vec<Entry>,
    limits: Limits,
    is_cut: &dyn Fn(&[u8]) -> bool,
    meta_id: &dyn Fn(&[u8]) -> MetaId,
) -> Result<Built> {
    for e in &entries {
        e.validate()
            .map_err(|err| invalid("directory", err.to_string()))?;
    }
    check_ascending("directory", entries.iter().map(|e| e.n.as_slice()))
        .map_err(|err| invalid("directory", err.to_string()))?;
    let base = base_len(&Seg::new(Vec::new()))?;
    let segs = segment(
        entries,
        base,
        limits,
        |e| Ok(cbor::encode(e)?.len()),
        |e| e.n.as_slice(),
        is_cut,
    )?;
    let mut items = Vec::new();
    let mut level_segs = Vec::with_capacity(segs.len());
    let single = segs.len() == 1;
    for s in segs {
        let first = s.first().map(|e| e.n.clone()).unwrap_or_default();
        let id = push(&mut items, &Item::Seg(Seg::new(s)), meta_id)?;
        if single {
            return Ok(Built { root: id, items });
        }
        level_segs.push(DirSeg { first, id });
    }
    let root = build_dir_levels(level_segs, limits, is_cut, meta_id, &mut items)?;
    Ok(Built { root, items })
}

/// 從第一層開始往上建 Dir，直到一個 Dir 放得下；超過三層寫不進去。
pub(crate) fn build_dir_levels(
    mut segs: Vec<DirSeg>,
    limits: Limits,
    is_cut: &dyn Fn(&[u8]) -> bool,
    meta_id: &dyn Fn(&[u8]) -> MetaId,
    items: &mut Vec<(MetaId, Vec<u8>)>,
) -> Result<MetaId> {
    for level in 1..=MAX_LEVEL {
        let base = base_len(&Dir::new(level, Vec::new()))?;
        let groups = segment(
            segs,
            base,
            limits,
            |s| Ok(cbor::encode(s)?.len()),
            |s| s.first.as_slice(),
            is_cut,
        )?;
        if groups.len() == 1 {
            let group = groups.into_iter().next().unwrap_or_default();
            return push(items, &Item::Dir(Dir::new(level, group)), meta_id);
        }
        let mut next = Vec::with_capacity(groups.len());
        for g in groups {
            let first = g.first().map(|s| s.first.clone()).unwrap_or_default();
            let id = push(items, &Item::Dir(Dir::new(level, g)), meta_id)?;
            next.push(DirSeg { first, id });
        }
        segs = next;
    }
    Err(invalid(
        "directory",
        format!("needs more than {MAX_LEVEL} index levels"),
    ))
}

/// 建好的檔案內容：Entry 的 `ct` 與 `chunks`，以及要寫的 item（`ct=0` 時沒有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltChunks {
    pub ct: u8,
    pub chunks: Vec<ItemId>,
    pub items: Vec<(MetaId, Vec<u8>)>,
}

/// 寫入端：檔案的 chunk（ID 與明文長度，依內容順序）。不超過 256 個 → `ct=0`；否則依切段規則（名稱以
/// chunk ID 的 32 bytes 代替）寫成一個 ChunkList，或多段時的 ChunkList 與最多三層的 ChunkIndex，`ct=1`。
pub fn build_chunks(
    chunks: Vec<(ChunkId, u64)>,
    limits: Limits,
    is_cut: &dyn Fn(&[u8]) -> bool,
    meta_id: &dyn Fn(&[u8]) -> MetaId,
) -> Result<BuiltChunks> {
    if chunks.len() <= MAX_INLINE_CHUNKS {
        return Ok(BuiltChunks {
            ct: content_type::DIRECT,
            chunks: chunks.into_iter().map(|(id, _)| id.into()).collect(),
            items: Vec::new(),
        });
    }
    let id_len = cbor::encode(&ChunkId::ZERO)?.len();
    let base = base_len(&ChunkList::new(Vec::new()))?;
    let lists = segment(
        chunks,
        base,
        limits,
        |_| Ok(id_len),
        |(id, _)| id.as_bytes().as_slice(),
        is_cut,
    )?;
    let mut items = Vec::new();
    let single = lists.len() == 1;
    let mut parts = Vec::with_capacity(lists.len());
    for list in lists {
        let size = sum_sizes(list.iter().map(|(_, s)| *s))?;
        let ids = list.into_iter().map(|(id, _)| id).collect();
        let id = push(&mut items, &Item::ChunkList(ChunkList::new(ids)), meta_id)?;
        if single {
            return Ok(BuiltChunks {
                ct: content_type::INDIRECT,
                chunks: vec![id.into()],
                items,
            });
        }
        parts.push(ChunkPart { size, id });
    }
    for level in 1..=MAX_LEVEL {
        let base = base_len(&ChunkIndex::new(level, Vec::new()))?;
        let groups = segment(
            parts,
            base,
            limits,
            |p| Ok(cbor::encode(p)?.len()),
            |p| p.id.as_bytes().as_slice(),
            is_cut,
        )?;
        if groups.len() == 1 {
            let group = groups.into_iter().next().unwrap_or_default();
            let id = push(
                &mut items,
                &Item::ChunkIndex(ChunkIndex::new(level, group)),
                meta_id,
            )?;
            return Ok(BuiltChunks {
                ct: content_type::INDIRECT,
                chunks: vec![id.into()],
                items,
            });
        }
        let mut next = Vec::with_capacity(groups.len());
        for g in groups {
            let size = sum_sizes(g.iter().map(|p| p.size))?;
            let id = push(
                &mut items,
                &Item::ChunkIndex(ChunkIndex::new(level, g)),
                meta_id,
            )?;
            next.push(ChunkPart { size, id });
        }
        parts = next;
    }
    Err(invalid(
        "file",
        format!("needs more than {MAX_LEVEL} index levels"),
    ))
}

fn sum_sizes(sizes: impl Iterator<Item = u64>) -> Result<u64> {
    let mut total = 0u64;
    for s in sizes {
        total = total
            .checked_add(s)
            .ok_or_else(|| invalid("file", "sizes overflow"))?;
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::v4::Error;
    use std::collections::HashMap;

    fn generic(name: &str) -> Entry {
        let mut e = Entry::new(
            name.as_bytes().to_vec(),
            node_type::FILE,
            meta_kind::GENERIC,
        );
        e.mtime = Some(1);
        e
    }

    fn posix_file(name: &str) -> Entry {
        let mut e = Entry::new(name.as_bytes().to_vec(), node_type::FILE, meta_kind::POSIX);
        e.mode = Some(0o100644);
        e.uid = Some(0);
        e.gid = Some(0);
        e.mtime = Some(1);
        e
    }

    fn fake_id(bytes: &[u8]) -> MetaId {
        MetaId::from_bytes(*blake3::hash(bytes).as_bytes())
    }

    /// 測試用的切段條件：名稱最後一個 byte 是 `X`。
    fn ends_x(n: &[u8]) -> bool {
        n.last() == Some(&b'X')
    }

    fn never(_: &[u8]) -> bool {
        false
    }

    #[test]
    fn an_entry_encodes_in_table_order() {
        // 手寫的預期值：{n: h'61', t: 0, mk: 3, mtime: 1}。
        let bytes = cbor::encode(&generic("a")).unwrap();
        assert_eq!(
            hex::encode(&bytes),
            "a4616e4161617400626d6b03656d74696d6501"
        );
        let mut e = posix_file("b");
        e.size = 5;
        e.chunks = vec![ItemId::from_bytes([1; 32])];
        e.ctime = Some(2);
        let bytes = cbor::encode(&e).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        let keys = ["size", "chunks", "mode", "uid", "gid", "mtime", "ctime"];
        let pos: Vec<usize> = keys.iter().map(|k| text.find(k).unwrap()).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");
        assert_eq!(cbor::decode::<Entry>(&bytes).unwrap(), e);
    }

    #[test]
    fn entry_rules_reject_every_bad_shape() {
        let mut cases: Vec<(&str, Entry)> = Vec::new();
        let mut e = posix_file("a");
        e.mode = None;
        cases.push(("posix without mode", e));
        let mut e = Entry::new(b"a".to_vec(), node_type::FILE, meta_kind::SFTP);
        e.mtime = Some(1);
        e.ctime = Some(1);
        cases.push(("sftp with ctime", e));
        let mut e = Entry::new(b"a".to_vec(), node_type::FILE, meta_kind::SFTP);
        cases.push(("sftp without mtime", e.clone()));
        e.mtime = Some(1);
        e.etag = Some(ByteBuf::from(vec![1]));
        cases.push(("sftp with etag", e));
        let mut e = Entry::new(b"a".to_vec(), node_type::FILE, meta_kind::S3);
        e.uid = Some(1);
        cases.push(("s3 with uid", e));
        let mut e = Entry::new(b"a".to_vec(), node_type::FILE, meta_kind::S3);
        e.etag = Some(ByteBuf::from(vec![0; MAX_ETAG_VER_BYTES + 1]));
        cases.push(("s3 etag too long", e));
        let mut e = generic("a");
        e.vern = Some(ByteBuf::from(vec![1]));
        cases.push(("generic with vern", e));
        let mut e = generic("a");
        e.mk = 4;
        cases.push(("unknown mk", e));
        let mut e = generic("a");
        e.t = node_type::DIR;
        cases.push(("dir without tree", e.clone()));
        e.tree = Some(MetaId::from_bytes([1; 32]));
        e.chunks = vec![ItemId::from_bytes([1; 32])];
        cases.push(("dir with chunks", e));
        let mut e = generic("a");
        e.t = node_type::SYMLINK;
        cases.push(("symlink without target", e));
        let mut e = generic("a");
        e.tree = Some(MetaId::from_bytes([1; 32]));
        cases.push(("file with tree", e));
        let mut e = generic("a");
        e.t = 3;
        cases.push(("unknown t", e));
        for name in ["", ".", "..", "a/b", "a\0b"] {
            let mut e = generic("x");
            e.n = name.as_bytes().to_vec();
            cases.push(("bad name", e));
        }
        let mut e = generic("a");
        e.ct = 2;
        cases.push(("unknown ct", e));
        let mut e = generic("a");
        e.chunks = vec![ItemId::from_bytes([1; 32]); MAX_INLINE_CHUNKS + 1];
        cases.push(("too many inline chunks", e));
        let mut e = generic("a");
        e.ct = content_type::INDIRECT;
        cases.push(("indirect without a list", e.clone()));
        e.chunks = vec![ItemId::from_bytes([1; 32]); 2];
        cases.push(("indirect with two lists", e));
        let mut e = posix_file("a");
        let mut x = BTreeMap::new();
        x.insert(
            ByteBuf::from(b"user.a".to_vec()),
            ByteBuf::from(vec![0; MAX_XATTR_BYTES]),
        );
        e.xattrs = Some(x);
        cases.push(("xattrs over 256 KiB", e));
        for (what, e) in cases {
            assert!(matches!(e.validate(), Err(Error::Corrupt { .. })), "{what}");
        }
        // 邊界上的值是合法的。
        let mut e = generic("a");
        e.chunks = vec![ItemId::from_bytes([1; 32]); MAX_INLINE_CHUNKS];
        e.validate().unwrap();
        let mut e = posix_file("a");
        let mut x = BTreeMap::new();
        x.insert(
            ByteBuf::from(b"k".to_vec()),
            ByteBuf::from(vec![0; MAX_XATTR_BYTES - 1]),
        );
        e.xattrs = Some(x);
        e.validate().unwrap();
    }

    #[test]
    fn duplicate_xattr_keys_are_rejected() {
        // {n: h'61', t: 0, mk: 0, mode: 0, uid: 0, gid: 0, mtime: 0, xattrs: {h'6b': h'', h'6b': h''}}
        let hex_text = [
            "a8",
            "616e4161",       // n: h'61'
            "617400",         // t: 0
            "626d6b00",       // mk: 0
            "646d6f646500",   // mode: 0
            "6375696400",     // uid: 0
            "6367696400",     // gid: 0
            "656d74696d6500", // mtime: 0
            "66786174747273", // xattrs:
            "a2416b40416b40", // {h'6b': h'', h'6b': h''}
        ]
        .concat();
        let bytes = hex::decode(&hex_text).unwrap();
        assert!(cbor::decode::<Entry>(&bytes).is_err());
        // 同樣的結構，key 不重複就解得開。
        let ok = hex_text.replace("a2416b40416b40", "a2416a40416b40");
        assert!(cbor::decode::<Entry>(&hex::decode(ok).unwrap()).is_ok());
    }

    #[test]
    fn items_round_trip_and_bad_items_are_rejected() {
        let seg = Item::Seg(Seg::new(vec![generic("a"), generic("b")]));
        let dir = Item::Dir(Dir::new(
            1,
            vec![DirSeg {
                first: b"a".to_vec(),
                id: MetaId::from_bytes([1; 32]),
            }],
        ));
        let list = Item::ChunkList(ChunkList::new(vec![ChunkId::from_bytes([2; 32])]));
        let index = Item::ChunkIndex(ChunkIndex::new(
            1,
            vec![ChunkPart {
                size: 9,
                id: MetaId::from_bytes([3; 32]),
            }],
        ));
        for item in [seg, dir, list, index] {
            let bytes = item.encode().unwrap();
            assert_eq!(decode_item(&bytes).unwrap(), item);
        }
        let mut s = Seg::new(vec![generic("a")]);
        s.v = 3;
        assert!(matches!(
            decode_item(&cbor::encode(&s).unwrap()),
            Err(Error::Version { version: 3, .. })
        ));
        let mut s = Seg::new(vec![generic("a")]);
        s.k = 9;
        assert!(decode_item(&cbor::encode(&s).unwrap()).is_err());
        let mut s = Seg::new(vec![generic("a")]);
        s.k = kind::DIR; // 段的 bytes 標成 Dir
        assert!(decode_item(&cbor::encode(&s).unwrap()).is_err());
        let unsorted = Seg::new(vec![generic("b"), generic("a")]);
        assert!(decode_item(&cbor::encode(&unsorted).unwrap()).is_err());
        let dup = Seg::new(vec![generic("a"), generic("a")]);
        assert!(decode_item(&cbor::encode(&dup).unwrap()).is_err());
        assert!(decode_item(&vec![0u8; MAX_ITEM_BYTES + 1]).is_err());
        assert!(Item::ChunkList(ChunkList::new(Vec::new()))
            .encode()
            .is_err());
        assert!(Item::Dir(Dir::new(1, Vec::new())).encode().is_err());
        for level in [0u8, 4] {
            let d = Dir::new(
                level,
                vec![DirSeg {
                    first: b"a".to_vec(),
                    id: MetaId::from_bytes([1; 32]),
                }],
            );
            assert!(decode_item(&cbor::encode(&d).unwrap()).is_err(), "{level}");
        }
        let zero = ChunkIndex::new(
            1,
            vec![ChunkPart {
                size: 0,
                id: MetaId::from_bytes([3; 32]),
            }],
        );
        assert!(decode_item(&cbor::encode(&zero).unwrap()).is_err());
    }

    fn names(n: usize) -> Vec<Entry> {
        let mut v: Vec<Entry> = (0..n).map(|i| generic(&format!("e{i:06}"))).collect();
        v.sort_by(|a, b| a.n.cmp(&b.n));
        v
    }

    fn seg_of(entries: Vec<Entry>, limits: Limits, cut: &dyn Fn(&[u8]) -> bool) -> Vec<Vec<Entry>> {
        let base = base_len(&Seg::new(Vec::new())).unwrap();
        segment(
            entries,
            base,
            limits,
            |e| Ok(cbor::encode(e)?.len()),
            |e| e.n.as_slice(),
            cut,
        )
        .unwrap()
    }

    #[test]
    fn the_cut_rule_needs_min_items_first() {
        let limits = Limits {
            min_items: 3,
            max_bytes: MAX_ITEM_BYTES,
        };
        // 名稱：a0 a1X a2 a3X a4 a5 a6X a7
        let es: Vec<Entry> = ["a0", "a1X", "a2", "a3X", "a4", "a5", "a6X", "a7"]
            .iter()
            .map(|n| generic(n))
            .collect();
        let segs = seg_of(es, limits, &ends_x);
        let got: Vec<Vec<String>> = segs
            .iter()
            .map(|s| {
                s.iter()
                    .map(|e| String::from_utf8(e.n.clone()).unwrap())
                    .collect()
            })
            .collect();
        // a1X 之前只有 1 項，不切；a3X 之前有 3 項，在它之後切；a6X 之前只有 2 項（a4 a5），不切。
        assert_eq!(
            got,
            vec![
                vec!["a0", "a1X", "a2", "a3X"],
                vec!["a4", "a5", "a6X", "a7"]
            ]
        );
        // 沒有任何項：一個空段。
        assert_eq!(
            seg_of(Vec::new(), limits, &ends_x),
            vec![Vec::<Entry>::new()]
        );
    }

    #[test]
    fn the_size_rule_is_exact() {
        let es = names(400);
        let one = cbor::encode(&es[0]).unwrap().len();
        let base = base_len(&Seg::new(Vec::new())).unwrap();
        for max in [
            base + 2 + one * 10,
            base + 2 + one * 10 + 1,
            base + 3 + one * 30 - 1,
        ] {
            let limits = Limits {
                min_items: 1_000_000,
                max_bytes: max,
            };
            let segs = seg_of(es.clone(), limits, &never);
            assert_eq!(segs.iter().map(Vec::len).sum::<usize>(), 400);
            for (i, s) in segs.iter().enumerate() {
                let len = Item::Seg(Seg::new(s.clone())).encode().unwrap().len();
                assert!(len <= max, "segment {i} is {len} > {max}");
                // 不是最後一段時，再加下一項就會超過：切在最晚的位置。
                if i + 1 < segs.len() {
                    let mut more = s.clone();
                    more.push(segs[i + 1][0].clone());
                    let over = cbor::encode(&Seg::new(more)).unwrap().len();
                    assert!(
                        over > max,
                        "segment {i} could have held one more ({over} <= {max})"
                    );
                }
            }
        }
        // 一項自己就放不進任何一段。
        let limits = Limits {
            min_items: 1,
            max_bytes: base + one,
        };
        let base_seg = base_len(&Seg::new(Vec::new())).unwrap();
        assert!(segment(
            names(1),
            base_seg,
            limits,
            |e| Ok(cbor::encode(e)?.len()),
            |e| e.n.as_slice(),
            &never
        )
        .is_err());
    }

    /// 讀取端：從根走到每一段，逐層檢查，收集全部 entries。
    fn read_dir(root: MetaId, items: &HashMap<MetaId, Item>) -> Vec<Entry> {
        fn walk(id: MetaId, items: &HashMap<MetaId, Item>, out: &mut Vec<Entry>) {
            match &items[&id] {
                Item::Seg(s) => out.extend(s.entries.iter().cloned()),
                Item::Dir(d) => {
                    for (i, s) in d.segs.iter().enumerate() {
                        check_dir_child(d, i, &items[&s.id], kind::SEG).unwrap();
                        walk(s.id, items, out);
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let mut out = Vec::new();
        walk(root, items, &mut out);
        out
    }

    fn decode_all(built: &[(MetaId, Vec<u8>)]) -> HashMap<MetaId, Item> {
        built
            .iter()
            .map(|(id, b)| {
                assert_eq!(fake_id(b), *id);
                (*id, decode_item(b).unwrap())
            })
            .collect()
    }

    #[test]
    fn build_dir_makes_up_to_three_levels_that_read_back() {
        // 上限 180 bytes：每段 6 項（一項 25 bytes）、每個 Dir 3 段（一段 52 bytes），150 項剛好三層。
        let es = names(150);
        let limits = Limits {
            min_items: 1_000_000,
            max_bytes: 180,
        };
        let built = build_dir(es.clone(), limits, &never, &fake_id).unwrap();
        let items = decode_all(&built.items);
        let Item::Dir(root) = &items[&built.root] else {
            panic!("root is not a Dir")
        };
        assert_eq!(root.level, 3);
        assert_eq!(read_dir(built.root, &items), es);
        assert!(built.items.iter().all(|(_, b)| b.len() <= limits.max_bytes));
        // 200 項需要第四層：寫不進去。
        assert!(matches!(
            build_dir(names(200), limits, &never, &fake_id),
            Err(Error::Invalid { .. })
        ));
        // 一段放得下就沒有 Dir。
        let small = build_dir(names(3), Limits::V4, &never, &fake_id).unwrap();
        assert_eq!(small.items.len(), 1);
        assert!(matches!(
            decode_item(&small.items[0].1).unwrap(),
            Item::Seg(_)
        ));
        // 寫入端拒絕沒排好的目錄。
        let mut bad = names(3);
        bad.swap(0, 1);
        assert!(build_dir(bad, Limits::V4, &never, &fake_id).is_err());
    }

    #[test]
    fn changing_one_entry_rewrites_only_its_segment_and_the_index_path() {
        let mut es = names(5000);
        // 稀疏、像雜湊一樣的切點（約 1/85），每段至少 50 項。週期性的切點（例如每 10 項一個）會讓插入一項之後
        // 每一段的切點都往前錯開一個週期、對不回來；真正的切點是 keyed hash，稀疏而不規則。
        let cut = |n: &[u8]| blake3::hash(n).as_bytes()[0] < 3;
        let limits = Limits {
            min_items: 50,
            max_bytes: MAX_ITEM_BYTES,
        };
        let before = build_dir(es.clone(), limits, &cut, &fake_id).unwrap();
        es[2500].mtime = Some(99);
        let after = build_dir(es.clone(), limits, &cut, &fake_id).unwrap();
        let ids = |b: &Built| {
            b.items
                .iter()
                .map(|(id, _)| *id)
                .collect::<std::collections::HashSet<_>>()
        };
        let (a, b) = (ids(&before), ids(&after));
        let changed = b.difference(&a).count();
        // 改了一項：那一段、加上往上的每一層 Dir（這裡只有一層）。
        assert_eq!(changed, 2, "{changed} items changed");
        // 插入一項：切點由名稱決定，後面的段很快就對齊回來。
        let mut ins = es.clone();
        ins.insert(1000, generic("e000999a"));
        let inserted = build_dir(ins, limits, &cut, &fake_id).unwrap();
        let changed = ids(&inserted).difference(&b).count();
        assert!(changed <= 3, "{changed} items changed after one insert");
    }

    fn chunks(n: usize) -> Vec<(ChunkId, u64)> {
        (0..n)
            .map(|i| {
                let id = ChunkId::from_bytes(*blake3::hash(&(i as u32).to_le_bytes()).as_bytes());
                (id, 1000 + i as u64)
            })
            .collect()
    }

    /// 讀取端：走 ChunkIndex 的每一層，檢查 size，收集 chunk ID。
    fn read_chunks(
        root: MetaId,
        items: &HashMap<MetaId, Item>,
        sizes: &HashMap<ChunkId, u64>,
    ) -> (Vec<ChunkId>, u64) {
        fn walk(
            id: MetaId,
            items: &HashMap<MetaId, Item>,
            sizes: &HashMap<ChunkId, u64>,
            out: &mut Vec<ChunkId>,
        ) -> u64 {
            match &items[&id] {
                Item::ChunkList(l) => {
                    out.extend(l.chunks.iter().copied());
                    l.chunks.iter().map(|c| sizes[c]).sum()
                }
                Item::ChunkIndex(x) => {
                    let mut total = 0;
                    for (i, p) in x.parts.iter().enumerate() {
                        let mut sub = Vec::new();
                        let covered = walk(p.id, items, sizes, &mut sub);
                        let chunk_bytes =
                            matches!(items[&p.id], Item::ChunkList(_)).then_some(covered);
                        check_index_child(x, i, &items[&p.id], chunk_bytes).unwrap();
                        out.extend(sub);
                        total += covered;
                    }
                    assert_eq!(total, x.total_size().unwrap());
                    total
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let mut out = Vec::new();
        let total = walk(root, items, sizes, &mut out);
        (out, total)
    }

    #[test]
    fn build_chunks_picks_direct_list_or_index() {
        let direct = build_chunks(chunks(MAX_INLINE_CHUNKS), Limits::V4, &never, &fake_id).unwrap();
        assert_eq!(direct.ct, content_type::DIRECT);
        assert_eq!(direct.chunks.len(), MAX_INLINE_CHUNKS);
        assert!(direct.items.is_empty());

        let one =
            build_chunks(chunks(MAX_INLINE_CHUNKS + 1), Limits::V4, &never, &fake_id).unwrap();
        assert_eq!(
            (one.ct, one.chunks.len(), one.items.len()),
            (content_type::INDIRECT, 1, 1)
        );
        assert!(matches!(
            decode_item(&one.items[0].1).unwrap(),
            Item::ChunkList(_)
        ));

        // 上限 356 bytes：每段 10 個 chunk（一個 34 bytes）、每個 ChunkIndex 6 到 7 段（一段 46 到 48 bytes），
        // 2000 個剛好三層。
        let cs = chunks(2000);
        let sizes: HashMap<ChunkId, u64> = cs.iter().copied().collect();
        let limits = Limits {
            min_items: 1_000_000,
            max_bytes: 356,
        };
        let built = build_chunks(cs.clone(), limits, &never, &fake_id).unwrap();
        assert_eq!((built.ct, built.chunks.len()), (content_type::INDIRECT, 1));
        let items = decode_all(&built.items);
        let root = built.chunks[0].to_meta();
        let Item::ChunkIndex(x) = &items[&root] else {
            panic!("root is not a ChunkIndex")
        };
        assert_eq!(x.level, 3);
        let (ids, total) = read_chunks(root, &items, &sizes);
        assert_eq!(ids, cs.iter().map(|(c, _)| *c).collect::<Vec<_>>());
        let mut e = generic("f");
        e.size = cs.iter().map(|(_, s)| s).sum();
        check_file_size(&e, total).unwrap();
        e.size += 1;
        assert!(check_file_size(&e, total).is_err());
        // 3000 個需要第四層：寫不進去。
        assert!(matches!(
            build_chunks(chunks(3000), limits, &never, &fake_id),
            Err(Error::Invalid { .. })
        ));
    }

    fn dir_with(firsts: &[&str], level: u8) -> Dir {
        Dir::new(
            level,
            firsts
                .iter()
                .map(|f| DirSeg {
                    first: f.as_bytes().to_vec(),
                    id: fake_id(f.as_bytes()),
                })
                .collect(),
        )
    }

    #[test]
    fn reader_checks_reject_every_bad_index() {
        // Dir 的 first 不是嚴格遞增。
        for firsts in [["b", "a"], ["a", "a"]] {
            let d = dir_with(&firsts, 1);
            assert!(
                decode_item(&cbor::encode(&d).unwrap()).is_err(),
                "{firsts:?}"
            );
        }
        let d = dir_with(&["a", "m"], 1);
        let seg = |names: &[&str]| Item::Seg(Seg::new(names.iter().map(|n| generic(n)).collect()));
        check_dir_child(&d, 0, &seg(&["a", "b"]), kind::SEG).unwrap();
        check_dir_child(&d, 1, &seg(&["m", "z"]), kind::SEG).unwrap();
        // 段是空的、第一項不是 first、伸進下一段、種類與 level 不符、沒有那一段。
        assert!(check_dir_child(&d, 0, &seg(&[]), kind::SEG).is_err());
        assert!(check_dir_child(&d, 0, &seg(&["b"]), kind::SEG).is_err());
        assert!(check_dir_child(&d, 0, &seg(&["a", "m"]), kind::SEG).is_err());
        assert!(check_dir_child(&d, 0, &seg(&["a", "n"]), kind::SEG).is_err());
        assert!(check_dir_child(&d, 0, &Item::Dir(dir_with(&["a"], 1)), kind::SEG).is_err());
        assert!(check_dir_child(&d, 2, &seg(&["z"]), kind::SEG).is_err());
        let d2 = dir_with(&["a", "m"], 2);
        assert!(check_dir_child(&d2, 0, &seg(&["a"]), kind::SEG).is_err());
        assert!(check_dir_child(&d2, 0, &Item::Dir(dir_with(&["a"], 2)), kind::SEG).is_err());
        check_dir_child(&d2, 0, &Item::Dir(dir_with(&["a", "c"], 1)), kind::SEG).unwrap();

        // ChunkIndex：size 不符、種類與 level 不符。
        let x = ChunkIndex::new(
            1,
            vec![ChunkPart {
                size: 10,
                id: MetaId::from_bytes([1; 32]),
            }],
        );
        let list = Item::ChunkList(ChunkList::new(vec![ChunkId::from_bytes([2; 32])]));
        check_index_child(&x, 0, &list, Some(10)).unwrap();
        assert!(check_index_child(&x, 0, &list, Some(9)).is_err());
        assert!(check_index_child(&x, 0, &list, None).is_err());
        assert!(check_index_child(&x, 1, &list, Some(10)).is_err());
        let x2 = ChunkIndex::new(2, x.parts.clone());
        assert!(check_index_child(&x2, 0, &list, Some(10)).is_err());
        check_index_child(&x2, 0, &Item::ChunkIndex(x.clone()), None).unwrap();
        let wrong = ChunkIndex::new(
            1,
            vec![ChunkPart {
                size: 11,
                id: MetaId::from_bytes([1; 32]),
            }],
        );
        assert!(check_index_child(&x2, 0, &Item::ChunkIndex(wrong), None).is_err());
    }
}
