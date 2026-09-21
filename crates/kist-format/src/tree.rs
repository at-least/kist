//! tree 物件：一個目錄的內容（v3）。
//!
//! - 每個目錄一個（或一串）tree，內容是**依名稱 bytes 排序**的節點清單。
//! - tree 是 content-addressed，名稱 = keyed BLAKE3(**明文 CBOR**)：
//!   目錄沒變，bytes 就沒變、名稱就沒變，與加密或壓縮的偶然無關。
//! - 超大目錄用 `prev` 串接：每滿 [`MAX_NODES_PER_TREE`] 個節點就先寫出一個
//!   tree，下一個 tree 的 `prev` 指向它。父目錄記錄的是**最後**一段的名稱；
//!   讀取時沿 `prev` 往回收集所有段，再從最舊的一段開始依序讀。
//! - 大檔案的 chunk 清單超過 [`MAX_INLINE_CHUNKS`] 個時改用間接（`ct` = 1）：
//!   清單本身編成 CBOR 的 [`ChunkList`]，當作一般資料切 chunk 存進 pack；
//!   tree 裡的 `chunks` 指向這些資料 chunk。
//!
//! v3 的 metadata 是 **kind 聯集**（`mk` 欄位）：posix / sftp / s3 / generic
//! 各有自己的欄位組（必填／選填／必須缺席，見 [`Entry::validate`]）。設計
//! 軸是「來源能證明什麼就記什麼」：posix 有 kernel 維護的 ctime/inode、
//! s3 有來源計算的 etag 與 version id、sftp/generic 只有來源聲稱的 mtime。
//! 檔名以 bytes 存放（Unix = 原 OS bytes；Windows = UTF-8），**一律是單一
//! 路徑元件**——v2 的合成根（絕對路徑節點名）已淘汰，根目錄的子女直接
//! 就是根 tree 的 entries（見 snapshot::Root）。

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

use crate::{ChunkId, FormatError, Result, TreeId, FORMAT_VERSION};

/// 單一 tree 物件最多放幾個節點，超過就切段。
pub const MAX_NODES_PER_TREE: usize = 10_000;

/// §8.1：s3 entry 的 etag／vern 各自的位元組上限。
pub const MAX_ETAG_VER_BYTES: usize = 1024;
/// 檔案的 chunk 清單超過這個數量就改用間接。
pub const MAX_INLINE_CHUNKS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    #[serde(rename = "v")]
    pub version: u32,
    /// 依 `n` 的 bytes 升冪排序（讀取端驗證，見 [`Tree::validate`]）。
    #[serde(rename = "entries")]
    pub entries: Vec<Entry>,
    /// 前一段（見模組說明）。
    #[serde(rename = "prev", default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<TreeId>,
}

impl Tree {
    pub fn new(entries: Vec<Entry>, prev: Option<TreeId>) -> Self {
        Self {
            version: FORMAT_VERSION,
            entries,
            prev,
        }
    }

    /// 結構驗證：版本、排序/唯一性、每個 entry 的 kind 規則。
    /// 讀取端在解碼後必須呼叫（format.md §8.1「讀取端強制」）。
    pub fn validate(&self) -> Result<()> {
        if self.version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: "tree",
                version: self.version,
            });
        }
        let mut prev_name: Option<&[u8]> = None;
        for e in &self.entries {
            e.validate()?;
            if let Some(p) = prev_name {
                if p >= e.name.as_slice() {
                    return Err(FormatError::InvalidEntry(
                        "entries must be sorted by name with no duplicates".to_owned(),
                    ));
                }
            }
            prev_name = Some(e.name.as_slice());
        }
        Ok(())
    }
}

/// 節點類型（`t` 欄位的值）。
pub mod node_type {
    pub const FILE: u8 = 0;
    pub const DIR: u8 = 1;
    pub const SYMLINK: u8 = 2;
}

/// 間接內容標記（`ct` 欄位的值）。
pub mod content_type {
    /// `chunks` 直接就是檔案內容的 chunk 清單（欄位省略時也是這個意思）。
    pub const DIRECT: u8 = 0;
    /// `chunks` 指向 `ChunkList` 資料塊：串起來解開才是真正的清單。
    pub const INDIRECT: u8 = 1;
}

/// metadata 種類（`mk` 欄位的值）：決定哪些欄位必填／選填／必須缺席。
pub mod meta_kind {
    /// 本機 POSIX 檔案系統：mode/uid/gid/mtime 必填（uid 0 = root 是真實值，
    /// 不是「沒記錄」）；ctime/dev/ino/nlink/xattrs 選填。
    pub const POSIX: u8 = 0;
    /// SFTP 來源：mtime 必填；mode/uid/gid 選填（缺席＝來源未知）。
    pub const SFTP: u8 = 1;
    /// S3 來源：mtime/etag/vern 選填；沒有 mode/uid 概念（缺席≠0）。
    pub const S3: u8 = 2;
    /// 未來後端的保守kind：只有 mtime。
    pub const GENERIC: u8 = 3;

    pub fn is_valid(v: u8) -> bool {
        v <= GENERIC
    }
}

/// 目錄裡的一個名字。wire 上的欄位順序＝下面的宣告順序（＝規格 §4.1 的
/// 欄位表）；per-kind 的必填/缺席規則由 [`Entry::validate`] 强制，序列化
/// 端寫出的欄位集合由 skip 條件決定（Option＝缺席、零值＝省略）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 檔名（原始 OS bytes）。**一律單一路徑元件**（任何來源，含根目錄）。
    #[serde(rename = "n", with = "serde_bytes")]
    pub name: Vec<u8>,
    /// [`node_type`]。
    #[serde(rename = "t")]
    pub kind: u8,
    /// [`meta_kind`]。
    #[serde(rename = "mk")]
    pub meta_kind: u8,
    /// 檔案內容長度（僅檔案；0 = 省略）。
    #[serde(rename = "size", default, skip_serializing_if = "is_zero_u64")]
    pub size: u64,
    /// 符號連結的目標（僅符號連結；空 = 省略）。
    #[serde(
        rename = "target",
        default,
        with = "serde_bytes",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub target: Vec<u8>,
    /// [`content_type`]；省略 = 直接。
    #[serde(rename = "ct", default, skip_serializing_if = "is_zero_u8")]
    pub content: u8,
    /// 檔案內容（或間接清單，見 `ct`）的 chunk。
    #[serde(rename = "chunks", default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChunkId>,
    /// 子目錄的 tree（目錄分段時 = 最後一段）。省略 = 全零。
    #[serde(rename = "tree", default, skip_serializing_if = "TreeId::is_zero")]
    pub subtree: TreeId,
    /// POSIX mode bits（含檔案類型位元）。posix/sftp 選填；缺席＝來源未知。
    #[serde(rename = "mode", default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// 擁有者。posix 必填（uid 0 = root）；sftp 選填；s3/generic 必須缺席。
    #[serde(rename = "uid", default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(rename = "gid", default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// 修改時間（奈秒）。posix/sftp 必填；s3/generic 選填。
    /// s3/sftp 來源只有秒精度（奈秒補 0）。
    #[serde(rename = "mtime", default, skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<i64>,
    /// inode 變更時間（奈秒，僅 posix）。kernel 在任何寫入時更新、無法偽造，
    /// 快速路徑用。
    #[serde(rename = "ctime", default, skip_serializing_if = "Option::is_none")]
    pub ctime_ns: Option<i64>,
    /// 硬連結識別（僅 posix；`nlink` > 1 才記）。
    #[serde(rename = "dev", default, skip_serializing_if = "Option::is_none")]
    pub dev: Option<u64>,
    #[serde(rename = "ino", default, skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    #[serde(rename = "nlink", default, skip_serializing_if = "Option::is_none")]
    pub nlink: Option<u64>,
    /// 擴充屬性（僅 posix；鍵與值都是 bytes，規範編碼排序）。
    #[serde(
        rename = "xattrs",
        default,
        skip_serializing_if = "xattrs_is_absent",
        deserialize_with = "de_xattrs_strict"
    )]
    pub xattrs: Option<std::collections::BTreeMap<ByteBuf, ByteBuf>>,
    /// 來源提供的內容 hash（s3 單段上傳的 ETag＝MD5；多段/KMS 仍是來源
    /// 定義的確定性指紋）。快速路徑的「內容可證明」依據。
    #[serde(
        rename = "etag",
        default,
        with = "serde_bytes",
        skip_serializing_if = "opt_bytes_is_absent"
    )]
    pub etag: Option<ByteBuf>,
    /// 來源物件版本 ID（S3 versioning；point-in-time 一致性用）。
    #[serde(
        rename = "vern",
        default,
        with = "serde_bytes",
        skip_serializing_if = "opt_bytes_is_absent"
    )]
    pub vern: Option<ByteBuf>,
}

impl Entry {
    /// 結構與 per-kind 驗證（format.md §8.1；讀取端强制）。
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: String| Err(FormatError::InvalidEntry(msg));
        // 名稱：一律單一路徑元件（v3 沒有合成根的例外）。
        if self.name.is_empty()
            || self.name.contains(&b'/')
            || self.name.contains(&0)
            || self.name == b"."
            || self.name == b".."
        {
            return bad(format!(
                "name must be a single clean path component, got {:?}",
                self.name
            ));
        }
        if !matches!(
            self.kind,
            node_type::FILE | node_type::DIR | node_type::SYMLINK
        ) {
            return bad(format!("unknown node type {}", self.kind));
        }
        if !meta_kind::is_valid(self.meta_kind) {
            return bad(format!("unknown metadata kind {}", self.meta_kind));
        }
        // 節點類型 × 內容欄位。
        match self.kind {
            node_type::FILE => {
                if !self.target.is_empty() || !self.subtree.is_zero() {
                    return bad("file entry must not carry target/subtree".to_owned());
                }
            }
            node_type::DIR => {
                if self.size != 0
                    || !self.target.is_empty()
                    || !self.chunks.is_empty()
                    || self.subtree.is_zero()
                {
                    return bad("dir entry must carry subtree only".to_owned());
                }
            }
            node_type::SYMLINK => {
                if self.target.is_empty()
                    || self.size != 0
                    || !self.chunks.is_empty()
                    || !self.subtree.is_zero()
                {
                    return bad("symlink entry must carry target only".to_owned());
                }
            }
            _ => unreachable!("kind checked above"),
        }
        if !matches!(self.content, content_type::DIRECT | content_type::INDIRECT) {
            return bad(format!("unknown content type {}", self.content));
        }
        // 直接內容的 inline chunk 數上限（§8：超過就必須轉間接）。
        // 寫入端在 backup 決定；讀取端同樣拒絕——超標的直接 entry 不是
        // 任何正確 writer 會產出的形狀（Go 端 Validate 同一檢查）。
        if self.content == content_type::DIRECT && self.chunks.len() > MAX_INLINE_CHUNKS {
            return bad(format!(
                "file {:?} has {} inline chunks, over the {} limit (must be indirect)",
                self.name,
                self.chunks.len(),
                MAX_INLINE_CHUNKS
            ));
        }
        if self.kind != node_type::FILE && !self.chunks.is_empty() {
            return bad("chunks only on file entries".to_owned());
        }
        // metadata 種類 × 欄位在場性（§8.1 表）。
        match self.meta_kind {
            meta_kind::POSIX => {
                if self.mode.is_none()
                    || self.uid.is_none()
                    || self.gid.is_none()
                    || self.mtime_ns.is_none()
                {
                    return bad("posix entry requires mode/uid/gid/mtime".to_owned());
                }
            }
            meta_kind::SFTP => {
                if self.mtime_ns.is_none() {
                    return bad("sftp entry requires mtime".to_owned());
                }
                if self.ctime_ns.is_some()
                    || self.dev.is_some()
                    || self.inode.is_some()
                    || self.nlink.is_some()
                    || self.xattrs.is_some()
                    || self.etag.is_some()
                    || self.vern.is_some()
                {
                    return bad("sftp entry must not carry posix/s3 fields".to_owned());
                }
            }
            meta_kind::S3 => {
                // etag/vern 是來源聲稱的位元組，不得無界進記憶體與 repo
                // （§8.1：各上限 1 KiB）。SFTP 讀取端早已封頂，這裡是
                // 格式級的同一把尺；Go 端 Validate 同款。
                if self
                    .etag
                    .as_ref()
                    .is_some_and(|e| e.len() > MAX_ETAG_VER_BYTES)
                {
                    return bad(format!("s3 etag exceeds {MAX_ETAG_VER_BYTES} bytes"));
                }
                if self
                    .vern
                    .as_ref()
                    .is_some_and(|v| v.len() > MAX_ETAG_VER_BYTES)
                {
                    return bad(format!("s3 vern exceeds {MAX_ETAG_VER_BYTES} bytes"));
                }
                if self.mode.is_some()
                    || self.uid.is_some()
                    || self.gid.is_some()
                    || self.ctime_ns.is_some()
                    || self.dev.is_some()
                    || self.inode.is_some()
                    || self.nlink.is_some()
                    || self.xattrs.is_some()
                {
                    return bad("s3 entry must not carry posix fields".to_owned());
                }
            }
            meta_kind::GENERIC => {
                if self.mode.is_some()
                    || self.uid.is_some()
                    || self.gid.is_some()
                    || self.ctime_ns.is_some()
                    || self.dev.is_some()
                    || self.inode.is_some()
                    || self.nlink.is_some()
                    || self.xattrs.is_some()
                    || self.etag.is_some()
                    || self.vern.is_some()
                {
                    return bad("generic entry carries mtime only".to_owned());
                }
            }
            _ => unreachable!("meta_kind checked above"),
        }
        Ok(())
    }
}

/// xattrs 的嚴格解碼：重複的 key 是偽造或損壞（format.md §4），拒絕——
/// serde 的 `BTreeMap` 訪問器會默默 last-wins，不能依賴。
/// （`std::result::Result`：crate 的 `Result` 別名只有一個泛型參數。）
fn de_xattrs_strict<'de, D>(
    d: D,
) -> std::result::Result<Option<std::collections::BTreeMap<ByteBuf, ByteBuf>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Option<std::collections::BTreeMap<ByteBuf, ByteBuf>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a map of xattrs")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut map = std::collections::BTreeMap::new();
            while let Some((k, v)) = access.next_entry::<ByteBuf, ByteBuf>()? {
                if map.insert(k, v).is_some() {
                    return Err(serde::de::Error::custom("duplicate xattr key"));
                }
            }
            Ok(Some(map))
        }
    }
    d.deserialize_map(Visitor)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// 偽造的 tree 帶重複的 xattr key：必須被拒（format.md §4）。
    #[test]
    fn duplicate_xattr_keys_are_rejected() {
        #[derive(Deserialize, Debug)]
        struct Probe {
            // 欄位值不被讀——存在只是為了觸發 de_xattrs_strict 的重複 key 偵測。
            #[expect(dead_code)]
            #[serde(rename = "x", deserialize_with = "de_xattrs_strict")]
            x: Option<std::collections::BTreeMap<ByteBuf, ByteBuf>>,
        }
        // {"x": {h"aa": 1, h"aa": 2}} —— map 內兩個相同的 key
        let bytes = [
            0xa1, 0x61, 0x78, 0xa2, 0x42, 0x61, 0x61, 0x01, 0x42, 0x61, 0x61, 0x02,
        ];
        let v: Result<Probe> = crate::cbor::decode(&bytes);
        assert!(v.is_err(), "重複的 xattr key 必須被拒");
    }

    fn posix_file(name: &[u8]) -> Entry {
        Entry {
            name: name.to_vec(),
            kind: node_type::FILE,
            meta_kind: meta_kind::POSIX,
            size: 0,
            target: Vec::new(),
            content: content_type::DIRECT,
            chunks: Vec::new(),
            subtree: TreeId::ZERO,
            mode: Some(0o644),
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

    #[test]
    fn posix_uid_zero_is_a_real_value() {
        // v3 釘死：posix 的 uid 0（root）是必填的真實值，不是「沒記錄」。
        let e = posix_file(b"a");
        assert!(e.validate().is_ok());
        let bytes = crate::cbor::encode(&e).unwrap();
        assert!(bytes.windows(3).any(|w| w == b"uid"));
        let back: Entry = crate::cbor::decode(&bytes).unwrap();
        assert_eq!(back.uid, Some(0));
        assert!(back.validate().is_ok());
    }

    #[test]
    fn names_are_always_single_components() {
        // v2 的合成根例外（絕對路徑節點名）在 v3 是非法的。
        for name in [
            b"/tmp/data".to_vec(),
            b"a/b".to_vec(),
            b".".to_vec(),
            b"..".to_vec(),
        ] {
            let mut e = posix_file(b"ok");
            e.name = name;
            assert!(e.validate().is_err(), "多元件/相對名稱必須被拒");
        }
    }

    #[test]
    fn per_kind_field_matrix_is_enforced() {
        // s3 entry 帶 mode → 拒。
        let mut e = posix_file(b"a");
        e.meta_kind = meta_kind::S3;
        assert!(e.validate().is_err(), "s3 不得帶 mode/uid/gid");
        // s3 乾淨 entry（只有 etag）→ 過。
        e.mode = None;
        e.uid = None;
        e.gid = None;
        e.mtime_ns = None;
        e.etag = Some(ByteBuf::from(b"\"etag\"".as_slice()));
        assert!(e.validate().is_ok());
        // 敵意 S3 listing 的 etag/vern 必須有上限（§8.1：1 KiB）：SFTP
        // 讀取端早就封頂，這個兄弟輸入沒有——記憶體放大與 repo 膨脹。
        e.meta_kind = meta_kind::S3;
        e.etag = Some(ByteBuf::from(vec![b'x'; 1025]));
        assert!(e.validate().is_err(), "etag 上限 1 KiB");
        e.etag = Some(ByteBuf::from(vec![b'x'; 1024]));
        e.vern = Some(ByteBuf::from(vec![b'v'; 1025]));
        assert!(e.validate().is_err(), "vern 上限 1 KiB");
        e.vern = Some(ByteBuf::from(vec![b'v'; 1024]));
        assert!(e.validate().is_ok());
        // generic 帶 etag → 拒。
        e.meta_kind = meta_kind::GENERIC;
        assert!(e.validate().is_err(), "generic 只有 mtime");
        // sftp 缺 mtime → 拒（vern 是 s3 欄位，一併清掉）。
        e.etag = None;
        e.vern = None;
        e.meta_kind = meta_kind::SFTP;
        assert!(e.validate().is_err(), "sftp 必填 mtime");
        e.mtime_ns = Some(1_700_000_000_000_000_000);
        assert!(e.validate().is_ok());
    }

    #[test]
    fn dir_needs_subtree_symlink_needs_target() {
        let mut d = posix_file(b"d");
        d.kind = node_type::DIR;
        assert!(d.validate().is_err(), "目錄必須帶 subtree");
        d.subtree = TreeId::from_bytes([1u8; 32]);
        assert!(d.validate().is_ok());

        let mut s = posix_file(b"s");
        s.kind = node_type::SYMLINK;
        assert!(s.validate().is_err(), "符號連結必須帶 target");
        s.target = b"dst".to_vec();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn tree_entries_must_be_sorted_and_unique() {
        let a = posix_file(b"a");
        let mut b0 = posix_file(b"b");
        b0.size = 1;
        assert!(Tree::new(vec![a, b0.clone()], None).validate().is_ok());
        // 重複名稱（同 bytes）必須被拒。
        let mut b1 = posix_file(b"b");
        b1.size = 2;
        assert!(
            Tree::new(vec![b0, b1], None).validate().is_err(),
            "名稱重複"
        );
        // 未排序必須被拒。
        let z = posix_file(b"z");
        let y = posix_file(b"y");
        assert!(Tree::new(vec![z, y], None).validate().is_err(), "未排序");
    }

    /// §8：直接內容的 inline chunk 數 ≤ 256，超過必須轉間接。寫入端在
    /// backup 決定；讀取端同樣拒絕——超標的直接 entry 不是任何正確
    /// writer 會產出的形狀。
    #[test]
    fn direct_content_over_the_inline_limit_is_rejected() {
        let mut e = posix_file(b"big");
        e.chunks = vec![ChunkId::ZERO; MAX_INLINE_CHUNKS + 1];
        assert!(e.validate().is_err(), "超標的直接 entry 必須被拒");
        e.chunks.truncate(MAX_INLINE_CHUNKS);
        assert!(e.validate().is_ok(), "恰好在上限內的直接 entry 合法");
        // 間接內容的 chunks 指向清單 chunk，不受這條限制。
        e.content = content_type::INDIRECT;
        e.chunks = vec![ChunkId::ZERO; MAX_INLINE_CHUNKS + 1];
        assert!(
            e.validate().is_ok(),
            "間接 entry 的清單 chunk 數不是 inline 數"
        );
    }
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

fn xattrs_is_absent(v: &Option<std::collections::BTreeMap<ByteBuf, ByteBuf>>) -> bool {
    match v {
        None => true,
        Some(m) => m.is_empty(),
    }
}

fn opt_bytes_is_absent(v: &Option<ByteBuf>) -> bool {
    match v {
        None => true,
        Some(b) => b.is_empty(),
    }
}

impl TreeId {
    pub fn is_zero(&self) -> bool {
        self.as_bytes() == &[0u8; 32]
    }
}

/// 間接內容的明文：chunk 清單本身。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkList {
    #[serde(rename = "v")]
    pub version: u32,
    #[serde(rename = "chunks")]
    pub chunks: Vec<ChunkId>,
}

impl ChunkList {
    pub fn new(chunks: Vec<ChunkId>) -> Self {
        Self {
            version: FORMAT_VERSION,
            chunks,
        }
    }
}
