//! §6：pack 與 mpack 的排版。
//!
//! ```text
//! +-------------------+----------------------+------------------+--------------------+-------------------+
//! | magic 8B          | entry × N            | trailer (sealed) | trailer len u64 BE | magic 8B          |
//! +-------------------+----------------------+------------------+--------------------+-------------------+
//! entry = nonce(24) ‖ AEAD(alg byte ‖ data) ‖ tag(16)
//! ```
//!
//! - magic ＝ `"kistpk"` ‖ u16 big-endian `0x0004`，頭尾一致；trailer 的 `v` ＝ 4。
//! - trailer `{v, kind, entries}` 以 `K_index` 密封（AAD `"kist/v4/pack-trailer"`）：`kind` 0 ＝資料 pack
//!   （只放 chunk，在 `packs/`）、1 ＝ mpack（只放 metadata item，在 `mpacks/`）。`entries` 每項
//!   `{i, o, l, r}`：ID、在檔內的位移、sealed 長度、明文長度（壓縮前）。
//! - 讀取端檢查（[`PackTrailer::check`]）：命名空間與 `kind` 相符、連續排列、完整覆蓋、沒有重複 ID、
//!   長度不超過上限。
//! - `alg`：0 原文、1 zstd；壓縮省不到 1/16 就存原文（[`keep_compressed`]）。zstd 本身與解壓上限（chunk 是
//!   `chunker.max`、metadata item 是 [`MAX_META_ITEM`]，與 `r` 無關、先限制輸出再比對長度）在讀寫路徑的
//!   實作段落。
//! - 名稱＝BLAKE3(整檔)（[`ObjectId::of`](super::ids::ObjectId::of)）。

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use crate::Algorithm;

use super::ids::ItemId;
use super::keys::Key;
use super::{check_version, corrupt, invalid, Result, VERSION};
use crate::cbor;

/// 頭尾的 magic：`"kistpk"` ‖ `0x0004`。
pub const MAGIC: [u8; 8] = *b"kistpk\x00\x04";
pub const HEADER_LEN: usize = 8;
/// 檔尾固定部分：u64 trailer 長度 + magic。
pub const FOOTER_LEN: usize = 8 + 8;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
/// 一項至少有 nonce、`alg` byte 與 tag。
pub const MIN_ENTRY_LEN: u64 = (NONCE_LEN + 1 + TAG_LEN) as u64;
/// metadata item 明文（解壓後）的上限。
pub const MAX_META_ITEM: u64 = 2 * 1024 * 1024;

/// pack 的種類，CBOR 裡是 u8。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// 資料 pack：只放 chunk，在 `packs/`。
    Data = 0,
    /// mpack：只放 metadata item，在 `mpacks/`。
    Meta = 1,
}

impl Kind {
    /// key 所在的命名空間對應的種類；不是 pack 或 mpack 的 key 回 `None`。
    pub fn of_key(key: &Key) -> Option<Kind> {
        match key {
            Key::Pack { .. } => Some(Kind::Data),
            Key::Mpack { .. } => Some(Kind::Meta),
            _ => None,
        }
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match u8::deserialize(d)? {
            0 => Ok(Kind::Data),
            1 => Ok(Kind::Meta),
            other => Err(serde::de::Error::custom(format!(
                "unknown pack kind {other}"
            ))),
        }
    }
}

/// trailer 裡的一項，map 的記錄與 delta 也用同一個結構：§4 的欄位順序 i, o, l, r。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    pub i: ItemId,
    /// 在檔內的位移（含 nonce）。
    pub o: u64,
    /// sealed 長度：nonce + 密文 + tag。
    pub l: u64,
    /// 明文長度（壓縮前）。
    pub r: u64,
}

/// trailer：§4 的欄位順序 v, kind, entries。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackTrailer {
    pub v: u32,
    pub kind: Kind,
    pub entries: Vec<PackEntry>,
}

impl PackTrailer {
    pub fn new(kind: Kind, entries: Vec<PackEntry>) -> Self {
        Self {
            v: VERSION,
            kind,
            entries,
        }
    }

    /// 寫入端：`trailer_start` 是 trailer 在檔內的起點（最後一項的結尾）。不一致就不寫。
    pub fn encode(&self, trailer_start: u64, max_r: u64) -> Result<Vec<u8>> {
        self.check(trailer_start, max_r)
            .map_err(|e| invalid("pack trailer", e.to_string()))?;
        Ok(cbor::encode(self)?)
    }

    /// 讀取端：解開 trailer 後檢查。`key` 是 pack 所在的 key，`trailer_start` 由檔尾算出，`max_r` 是這個
    /// 種類的明文上限（資料是 `chunker.max`、metadata 是 [`MAX_META_ITEM`]）。
    pub fn decode(bytes: &[u8], key: &Key, trailer_start: u64, max_r: u64) -> Result<Self> {
        let t: PackTrailer = cbor::decode(bytes)?;
        t.check(trailer_start, max_r)?;
        match Kind::of_key(key) {
            Some(k) if k == t.kind => Ok(t),
            _ => Err(corrupt(
                "pack trailer",
                format!("kind {:?} does not belong under {}", t.kind, key.path()),
            )),
        }
    }

    /// 連續排列、完整覆蓋、沒有重複 ID、長度不超過上限。
    pub fn check(&self, trailer_start: u64, max_r: u64) -> Result<()> {
        check_version("pack trailer", self.v)?;
        let bad = |msg: String| Err(corrupt("pack trailer", msg));
        let max_l = max_r + MIN_ENTRY_LEN;
        let mut pos = HEADER_LEN as u64;
        let mut seen = HashSet::with_capacity(self.entries.len());
        for (n, e) in self.entries.iter().enumerate() {
            if e.o != pos {
                return bad(format!("entry {n} starts at {}, expected {pos}", e.o));
            }
            if e.l < MIN_ENTRY_LEN || e.l > max_l {
                return bad(format!(
                    "entry {n} is {} bytes, outside {MIN_ENTRY_LEN}..={max_l}",
                    e.l
                ));
            }
            if e.r > max_r {
                return bad(format!(
                    "entry {n} claims {} plaintext bytes, over {max_r}",
                    e.r
                ));
            }
            if !seen.insert(e.i) {
                return bad(format!("entry {n} repeats id {}", e.i));
            }
            pos = match e.o.checked_add(e.l) {
                Some(p) => p,
                None => return bad(format!("entry {n} overflows")),
            };
        }
        if pos != trailer_start {
            return bad(format!(
                "entries end at {pos}, but the trailer starts at {trailer_start}"
            ));
        }
        Ok(())
    }
}

/// 開新 pack：回傳已含 magic 的 buffer。
pub fn begin() -> Vec<u8> {
    MAGIC.to_vec()
}

/// 接上已密封的 trailer 與檔尾，pack 的 bytes 就完成了。
pub fn finish(mut pack: Vec<u8>, trailer_sealed: &[u8]) -> Vec<u8> {
    pack.extend_from_slice(trailer_sealed);
    pack.extend_from_slice(&(trailer_sealed.len() as u64).to_be_bytes());
    pack.extend_from_slice(&MAGIC);
    pack
}

/// 從檔尾（至少最後 16 bytes）讀出 trailer 的長度，並驗證檔尾的 magic。
pub fn parse_footer(footer: &[u8]) -> Result<u64> {
    if footer.len() < FOOTER_LEN {
        return Err(corrupt(
            "pack footer",
            format!("{} bytes, need {FOOTER_LEN}", footer.len()),
        ));
    }
    let footer = &footer[footer.len() - FOOTER_LEN..];
    if footer[8..] != MAGIC {
        return Err(corrupt("pack footer", "bad magic"));
    }
    let mut len = [0u8; 8];
    len.copy_from_slice(&footer[..8]);
    Ok(u64::from_be_bytes(len))
}

/// 整個 pack 的 bytes → (trailer 的起點, 已密封的 trailer)。驗證頭尾的 magic 與長度。
pub fn split_trailer(pack: &[u8]) -> Result<(u64, &[u8])> {
    if pack.len() < HEADER_LEN + FOOTER_LEN {
        return Err(corrupt(
            "pack",
            format!("{} bytes is too short", pack.len()),
        ));
    }
    if pack[..HEADER_LEN] != MAGIC {
        return Err(corrupt("pack header", "bad magic"));
    }
    let trailer_len = parse_footer(pack)?;
    let end = pack.len() - FOOTER_LEN;
    let start = usize::try_from(trailer_len)
        .ok()
        .and_then(|l| end.checked_sub(l))
        .filter(|s| *s >= HEADER_LEN)
        .ok_or_else(|| corrupt("pack", format!("trailer length {trailer_len} does not fit")))?;
    Ok((start as u64, &pack[start..end]))
}

/// 一項的明文：`alg byte ‖ data`。
pub fn frame(alg: Algorithm, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + data.len());
    out.push(alg as u8);
    out.extend_from_slice(data);
    out
}

/// 拆開 `alg byte ‖ data`；空的或 `alg` 不認得都是腐壞。
pub fn unframe(plain: &[u8]) -> Result<(Algorithm, &[u8])> {
    let (&a, data) = plain
        .split_first()
        .ok_or_else(|| corrupt("pack entry", "empty plaintext"))?;
    let alg = Algorithm::from_u8(a).map_err(|e| corrupt("pack entry", e.to_string()))?;
    Ok((alg, data))
}

/// 壓縮省不到 1/16 就存原文：省下的 bytes × 16 ≥ 原文長度才留下壓縮版（以實數比較，原文很短時也不會因為
/// 整數除法而留下沒省到的壓縮版）。
pub fn keep_compressed(raw_len: usize, compressed_len: usize) -> bool {
    let saved = raw_len.saturating_sub(compressed_len) as u64;
    saved > 0 && saved * 16 >= raw_len as u64
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::v4::ids::{ClientId, ObjectId};
    use crate::v4::Error;

    fn entry(b: u8, o: u64, l: u64, r: u64) -> PackEntry {
        PackEntry {
            i: ItemId::from_bytes([b; 32]),
            o,
            l,
            r,
        }
    }

    fn two() -> PackTrailer {
        PackTrailer::new(Kind::Meta, vec![entry(1, 8, 41, 10), entry(2, 49, 50, 100)])
    }

    fn mpack_key() -> Key {
        Key::Mpack {
            client: ClientId::from_bytes([3; 16]),
            id: ObjectId::from_bytes([4; 32]),
            replica: false,
        }
    }

    #[test]
    fn the_trailer_encodes_as_specified() {
        // 手寫的預期值：{v:4, kind:1, entries:[{i, o:8, l:41, r:10}]}。
        let t = PackTrailer::new(Kind::Meta, vec![entry(0xaa, 8, 41, 10)]);
        let id = "aa".repeat(32);
        let expected = [
            "a3",
            "617604",           // v: 4
            "646b696e6401",     // kind: 1
            "67656e7472696573", // entries
            "81a4",
            "61695820",
            id.as_str(),
            "616f08",   // o: 8
            "616c1829", // l: 41
            "61720a",   // r: 10
        ]
        .concat();
        assert_eq!(hex::encode(t.encode(49, MAX_META_ITEM).unwrap()), expected);
    }

    #[test]
    fn a_consistent_trailer_round_trips() {
        let t = two();
        let bytes = t.encode(99, MAX_META_ITEM).unwrap();
        assert_eq!(
            PackTrailer::decode(&bytes, &mpack_key(), 99, MAX_META_ITEM).unwrap(),
            t
        );
    }

    #[test]
    fn inconsistent_trailers_are_rejected() {
        let cases: Vec<(&str, PackTrailer, u64)> = vec![
            (
                "gap",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, 41, 10), entry(2, 50, 50, 1)]),
                100,
            ),
            (
                "overlap",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, 41, 10), entry(2, 48, 50, 1)]),
                98,
            ),
            (
                "not from the header",
                PackTrailer::new(Kind::Meta, vec![entry(1, 9, 41, 10)]),
                50,
            ),
            (
                "short of the trailer",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, 41, 10)]),
                50,
            ),
            (
                "duplicate id",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, 41, 10), entry(1, 49, 41, 1)]),
                90,
            ),
            (
                "tiny entry",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, 40, 0)]),
                48,
            ),
            (
                "r over the cap",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, 41, MAX_META_ITEM + 1)]),
                49,
            ),
            (
                "l over the cap",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, MAX_META_ITEM + 42, 1)]),
                MAX_META_ITEM + 50,
            ),
            (
                "overflow",
                PackTrailer::new(Kind::Meta, vec![entry(1, 8, u64::MAX, 1)]),
                0,
            ),
        ];
        for (name, t, start) in cases {
            let bytes = cbor::encode(&t).unwrap();
            assert!(
                matches!(
                    PackTrailer::decode(&bytes, &mpack_key(), start, MAX_META_ITEM),
                    Err(Error::Corrupt { .. })
                ),
                "{name}"
            );
            assert!(
                matches!(t.encode(start, MAX_META_ITEM), Err(Error::Invalid { .. })),
                "{name}"
            );
        }
    }

    #[test]
    fn the_kind_must_match_the_namespace() {
        let bytes = two().encode(99, MAX_META_ITEM).unwrap();
        let data_key = Key::Pack {
            client: ClientId::from_bytes([3; 16]),
            id: ObjectId::from_bytes([4; 32]),
        };
        assert!(PackTrailer::decode(&bytes, &data_key, 99, MAX_META_ITEM).is_err());
        assert!(PackTrailer::decode(&bytes, &Key::Config, 99, MAX_META_ITEM).is_err());
        // 種類不認得、版本不是 4 也拒絕。
        let mut t = two();
        t.v = 5;
        assert!(matches!(
            PackTrailer::decode(&cbor::encode(&t).unwrap(), &mpack_key(), 99, MAX_META_ITEM),
            Err(Error::Version { version: 5, .. })
        ));
        let raw = cbor::encode(&two()).unwrap();
        let kind_at = raw.windows(5).position(|w| w == b"kind\x01").unwrap() + 4;
        let mut unknown = raw.clone();
        unknown[kind_at] = 0x02;
        assert!(PackTrailer::decode(&unknown, &mpack_key(), 99, MAX_META_ITEM).is_err());
    }

    #[test]
    fn layout_frames_entries_and_the_trailer() {
        let mut p = begin();
        p.extend_from_slice(&[0x01; 41]);
        p.extend_from_slice(&[0x02; 50]);
        let trailer = [0xee; 30];
        let p = finish(p, &trailer);
        // 手寫的預期值。
        let mut expected = b"kistpk\x00\x04".to_vec();
        expected.extend_from_slice(&[0x01; 41]);
        expected.extend_from_slice(&[0x02; 50]);
        expected.extend_from_slice(&[0xee; 30]);
        expected.extend_from_slice(&30u64.to_be_bytes());
        expected.extend_from_slice(b"kistpk\x00\x04");
        assert_eq!(p, expected);
        assert_eq!(split_trailer(&p).unwrap(), (99, &trailer[..]));
        assert_eq!(parse_footer(&p[p.len() - 16..]).unwrap(), 30);

        let mut bad_head = p.clone();
        bad_head[7] = 3; // v3 的 magic
        assert!(split_trailer(&bad_head).is_err());
        let mut bad_tail = p.clone();
        let n = bad_tail.len();
        bad_tail[n - 1] = 3;
        assert!(split_trailer(&bad_tail).is_err());
        let mut too_long = p.clone();
        too_long[n - 16..n - 8].copy_from_slice(&1000u64.to_be_bytes());
        assert!(split_trailer(&too_long).is_err());
        assert!(split_trailer(&p[..23]).is_err());
    }

    #[test]
    fn entry_plaintext_is_alg_then_data() {
        let f = frame(Algorithm::Zstd, b"abc");
        assert_eq!(f, b"\x01abc");
        assert_eq!(unframe(&f).unwrap(), (Algorithm::Zstd, &b"abc"[..]));
        assert!(unframe(b"").is_err());
        assert!(unframe(b"\x02abc").is_err());
    }

    #[test]
    fn compression_is_kept_only_when_it_saves_a_sixteenth() {
        assert!(keep_compressed(1600, 1500));
        assert!(!keep_compressed(1600, 1501));
        assert!(keep_compressed(16, 15));
        assert!(!keep_compressed(16, 16));
        assert!(!keep_compressed(10, 10)); // 原文很短時，沒省到就不留
        assert!(keep_compressed(10, 9));
        assert!(!keep_compressed(0, 0));
        assert!(!keep_compressed(100, 200));
    }
}
