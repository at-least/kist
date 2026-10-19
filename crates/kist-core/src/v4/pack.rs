//! §6：pack 與 mpack 的寫入，與讀取一項時的驗證。
//!
//! 寫入端 [`PackWriter`] 把一項壓縮（省不到 1/16 就存原文）、加密後接在 buffer 後面，寫滿 target 或
//! backup 結束時 [`PackWriter::finish`] 接上 trailer、算出名稱；上傳由呼叫端負責。資料 pack 只放 chunk
//! （`K_data`），mpack 只放 metadata item（`K_meta`），AAD 都是那一項自己的 ID。
//!
//! 讀取一項（[`open_chunk`]、[`open_meta`]）：解密、拆 `alg`、解壓——輸出上限是 chunk 的 `chunker.max`
//! 或 metadata 的 2 MiB，與 trailer 宣稱的 `r` 無關，先限制輸出再比對長度——最後重算 ID。任何一步失敗
//! 都是這份副本不可信，呼叫端換下一個持有者（§10）。

use std::cell::RefCell;
use std::io::Read;
use std::sync::Arc;

use kist_crypto::v4::Keys;
use kist_format::v4::ids::{ChunkId, ItemId, MetaId, ObjectId};
use kist_format::v4::keys::Key;
use kist_format::v4::pack::{self, keep_compressed, Algorithm, Kind, PackEntry, PackTrailer};

use crate::{CoreError, Result};

/// zstd 的等級（§6：level 3）。
pub(crate) const ZSTD_LEVEL: i32 = 3;

// 每條執行緒一個 zstd context，每一項重用：每一項新建一個時，zstd 每次以 C 的 malloc 配一塊 3.5 MiB 的
// workspace（串流的寫法不知道大小）；100 萬個檔案就是 100 萬次。輸出與每次新建的 `zstd::encode_all` 相同。
thread_local! {
    static CCTX: RefCell<zstd::zstd_safe::CCtx<'static>> = RefCell::new(zstd::zstd_safe::CCtx::create());
    static DCTX: RefCell<zstd::zstd_safe::DCtx<'static>> = RefCell::new(zstd::zstd_safe::DCtx::create());
}

fn zstd_error(code: zstd::zstd_safe::ErrorCode) -> std::io::Error {
    std::io::Error::other(zstd::zstd_safe::get_error_name(code))
}

/// metadata item 解壓後的上限（§6）。
pub const MAX_META: u64 = pack::MAX_META_ITEM;

/// 一項的明文：`alg byte ‖ data`。壓縮省下的 bytes × 16 ≥ 原文長度才留壓縮版（§6）。
pub fn compress(raw: &[u8]) -> Result<Vec<u8>> {
    let z = CCTX
        .with(|c| -> std::io::Result<Vec<u8>> {
            use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective::{ZSTD_e_continue, ZSTD_e_end};
            use zstd::zstd_safe::{InBuffer, OutBuffer};
            let mut c = c.borrow_mut();
            // 前一項失敗在一半時，context 裡留著那一項的狀態
            c.reset(zstd::zstd_safe::ResetDirective::SessionOnly)
                .map_err(zstd_error)?;
            c.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(ZSTD_LEVEL))
                .map_err(zstd_error)?;
            // 直接寫進要交出去的 Vec（`zstd::stream` 的 wrapper 每次另配 32 KiB 的輸出 buffer）。先 continue
            // 再 end，同 `encode_all`：zstd 不知道大小，輸出才與它相同
            let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(raw.len()));
            let mut input = InBuffer::around(raw);
            let mut end = ZSTD_e_continue;
            loop {
                if out.len() == out.capacity() {
                    out.reserve(out.capacity().max(64));
                }
                let pos = out.len();
                let left = c
                    .compress_stream2(&mut OutBuffer::around_pos(&mut out, pos), &mut input, end)
                    .map_err(zstd_error)?;
                if end == ZSTD_e_continue {
                    if input.pos() == raw.len() {
                        end = ZSTD_e_end;
                    }
                } else if left == 0 {
                    return Ok(out);
                }
            }
        })
        .map_err(|e| CoreError::corrupt("<pack entry>", format!("zstd failed: {e}")))?;
    Ok(if keep_compressed(raw.len(), z.len()) {
        pack::frame(Algorithm::Zstd, &z)
    } else {
        pack::frame(Algorithm::Raw, raw)
    })
}

/// 拆開 `alg byte ‖ data` 並解壓，輸出最多 `limit` bytes：超過就是腐壞，不是內容。
fn decompress(framed: &[u8], limit: u64, what: &str) -> Result<Vec<u8>> {
    let (alg, data) = pack::unframe(framed).map_err(|e| CoreError::corrupt(what, e.to_string()))?;
    match alg {
        Algorithm::Raw => {
            if data.len() as u64 > limit {
                return Err(CoreError::corrupt(
                    what,
                    format!("{} bytes, over the {limit} limit", data.len()),
                ));
            }
            Ok(data.to_vec())
        }
        Algorithm::Zstd => {
            let mut out = Vec::new();
            DCTX.with(|d| -> std::io::Result<()> {
                let mut d = d.borrow_mut();
                d.reset(zstd::zstd_safe::ResetDirective::SessionOnly)
                    .map_err(zstd_error)?;
                zstd::stream::read::Decoder::with_context(data, &mut d)
                    .take(limit.saturating_add(1))
                    .read_to_end(&mut out)?;
                Ok(())
            })
            .map_err(|e| CoreError::corrupt(what, format!("zstd decode failed: {e}")))?;
            if out.len() as u64 > limit {
                return Err(CoreError::corrupt(
                    what,
                    format!("decompresses past the {limit} limit"),
                ));
            }
            Ok(out)
        }
    }
}

fn check_len(what: &str, plain: &[u8], r: u64) -> Result<()> {
    if plain.len() as u64 == r {
        Ok(())
    } else {
        Err(CoreError::corrupt(
            what,
            format!("{} plaintext bytes, the holder says {r}", plain.len()),
        ))
    }
}

/// 讀取一個 chunk：`K_data` 解密（AAD 是 chunk ID）、解壓（上限 `max` ＝ `chunker.max`）、長度等於 `r`、
/// 重算 chunk ID。
pub fn open_chunk(keys: &Keys, id: &ChunkId, sealed: &[u8], r: u64, max: u64) -> Result<Vec<u8>> {
    let what = format!("chunk {id}");
    let framed = keys
        .open_chunk(id, sealed)
        .map_err(|e| CoreError::corrupt(&what, e.to_string()))?;
    let plain = decompress(&framed, max, &what)?;
    check_len(&what, &plain, r)?;
    let actual = keys.chunk_id(&plain);
    if actual != *id {
        return Err(CoreError::corrupt(
            &what,
            format!("content hashes to {actual}"),
        ));
    }
    Ok(plain)
}

/// 讀取一個 metadata item：`K_meta` 解密（AAD 是 meta ID）、解壓（上限 2 MiB）、長度等於 `r`、重算 meta
/// ID。回傳規範 CBOR（解碼與結構檢查由呼叫端以 `decode_item` 做）。
pub fn open_meta(keys: &Keys, id: &MetaId, sealed: &[u8], r: u64) -> Result<Vec<u8>> {
    let what = format!("metadata item {id}");
    let framed = keys
        .open_meta(id, sealed)
        .map_err(|e| CoreError::corrupt(&what, e.to_string()))?;
    let plain = decompress(&framed, MAX_META, &what)?;
    check_len(&what, &plain, r)?;
    let actual = keys.meta_id(&plain);
    if actual != *id {
        return Err(CoreError::corrupt(
            &what,
            format!("content hashes to {actual}"),
        ));
    }
    Ok(plain)
}

/// 一個 pack 的種類的明文上限：資料是 `chunker.max`，metadata 是 2 MiB（§6）。
pub fn max_r(kind: Kind, chunk_max: u32) -> u64 {
    match kind {
        Kind::Data => u64::from(chunk_max),
        Kind::Meta => MAX_META,
    }
}

/// 整份 pack 的 bytes → 驗證過的 trailer：頭尾 magic、`K_index` 解密、§6 的一致性檢查、種類與命名空間相符。
pub fn trailer_of(keys: &Keys, key: &Key, bytes: &[u8], chunk_max: u32) -> Result<PackTrailer> {
    let path = key.path();
    let kind =
        Kind::of_key(key).ok_or_else(|| CoreError::corrupt(&path, "not a pack or mpack key"))?;
    let (start, sealed) =
        pack::split_trailer(bytes).map_err(|e| CoreError::corrupt(&path, e.to_string()))?;
    decode_trailer(keys, key, start, sealed, max_r(kind, chunk_max))
}

/// 已密封的 trailer → 驗證過的 trailer（`start` 是 trailer 在檔內的起點）。
pub fn decode_trailer(
    keys: &Keys,
    key: &Key,
    start: u64,
    sealed: &[u8],
    max_r: u64,
) -> Result<PackTrailer> {
    let path = key.path();
    let plain = keys
        .open_trailer(sealed)
        .map_err(|e| CoreError::corrupt(&path, e.to_string()))?;
    // 版本不認得的留著原本的錯誤：prune 把它記成 unknown（不使用、不刪除），不是 unreadable（§11.3 P4）
    PackTrailer::decode(&plain, key, start, max_r).map_err(|e| match e {
        kist_format::v4::Error::Version { .. } => CoreError::FormatV4(e),
        e => CoreError::corrupt(&path, e.to_string()),
    })
}

/// 一項的密文：壓縮（[`compress`]）再以種類的金鑰加密，AAD 是它的 ID。
fn seal_item(keys: &Keys, kind: Kind, id: ItemId, plain: &[u8]) -> Result<Vec<u8>> {
    let framed = compress(plain)?;
    Ok(match kind {
        Kind::Data => keys.seal_chunk(&id.to_chunk(), &framed)?,
        Kind::Meta => keys.seal_meta(&id.to_meta(), &framed)?,
    })
}

/// trailer 的密文：先照 §6 檢查（`len` 是 trailer 之前的 bytes），不一致就不寫。
fn seal_trailer(
    keys: &Keys,
    kind: Kind,
    entries: &[PackEntry],
    len: u64,
    max_r: u64,
) -> Result<Vec<u8>> {
    let trailer = PackTrailer::new(kind, entries.to_vec())
        .encode(len, max_r)
        .map_err(|e| CoreError::corrupt("<pack trailer>", e.to_string()))?;
    Ok(keys.seal_trailer(&trailer)?)
}

/// 一個已封好、待上傳的 pack 或 mpack。
#[derive(Debug)]
pub struct FinishedPack {
    pub id: ObjectId,
    pub kind: Kind,
    pub bytes: Vec<u8>,
    pub entries: Vec<PackEntry>,
}

/// 依序累積一種 pack 的項目。
pub struct PackWriter {
    keys: Arc<Keys>,
    kind: Kind,
    target: usize,
    max_r: u64,
    buf: Vec<u8>,
    entries: Vec<PackEntry>,
}

impl PackWriter {
    /// `target` 是 head 的 `pack_target`（資料）或 `mpack_target`（metadata）；`chunk_max` 是 config 的
    /// `chunker.max`，資料 pack 的 `r` 上限。
    pub fn new(keys: Arc<Keys>, kind: Kind, target: u64, chunk_max: u32) -> Self {
        let target = usize::try_from(target).unwrap_or(usize::MAX);
        Self {
            keys,
            kind,
            target,
            max_r: max_r(kind, chunk_max),
            buf: pack::begin(),
            entries: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.target
    }

    /// 壓縮、加密並接在後面。`plain` 是 chunk 的明文或 item 的規範 CBOR。
    pub fn add(&mut self, id: ItemId, plain: &[u8]) -> Result<PackEntry> {
        let sealed = seal_item(&self.keys, self.kind, id, plain)?;
        let entry = PackEntry {
            i: id,
            o: self.buf.len() as u64,
            l: sealed.len() as u64,
            r: plain.len() as u64,
        };
        self.buf.extend_from_slice(&sealed);
        self.entries.push(entry);
        Ok(entry)
    }

    /// 接上 trailer，交出完整的 pack，自己重置成空的；沒有任何項目時回 `None`。trailer 先照 §6 檢查，
    /// 不一致就不寫。
    pub fn finish(&mut self) -> Result<Option<FinishedPack>> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        let entries = std::mem::take(&mut self.entries);
        let buf = std::mem::replace(&mut self.buf, pack::begin());
        let sealed = seal_trailer(
            &self.keys,
            self.kind,
            &entries,
            buf.len() as u64,
            self.max_r,
        )?;
        let bytes = pack::finish(buf, &sealed);
        let id = ObjectId::of(&bytes);
        Ok(Some(FinishedPack {
            id,
            kind: self.kind,
            bytes,
            entries,
        }))
    }
}

fn spool_error(path: &std::path::Path, source: std::io::Error) -> CoreError {
    CoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// 資料 pack 的寫入端，項目一邊加一邊寫進本機的暫存檔（M12 第 7 段之四第五步）：累積中的 pack 不在記憶體裡
/// （[`PackWriter`] 的 buffer 是 `pack_target` 那麼大，預設 64 MiB）。名稱的 BLAKE3 邊寫邊算。暫存檔在 `dir`
/// 下（本地快取的目錄；沒有快取時是系統的暫存目錄），[`FinishedSpool`] drop 時刪掉。
pub struct SpoolWriter {
    keys: Arc<Keys>,
    kind: Kind,
    target: u64,
    max_r: u64,
    dir: std::path::PathBuf,
    open: Option<Spool>,
    entries: Vec<PackEntry>,
}

/// 累積中的那一個 pack 的暫存檔。
struct Spool {
    out: std::io::BufWriter<tempfile::NamedTempFile>,
    hasher: blake3::Hasher,
    len: u64,
}

impl Spool {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        self.out
            .write_all(bytes)
            .map_err(|e| spool_error(self.out.get_ref().path(), e))?;
        self.hasher.update(bytes);
        self.len += bytes.len() as u64;
        Ok(())
    }
}

/// 寫完的資料 pack：在暫存檔裡。
pub struct FinishedSpool {
    pub id: ObjectId,
    pub kind: Kind,
    pub len: u64,
    pub entries: Vec<PackEntry>,
    file: tempfile::NamedTempFile,
}

impl FinishedSpool {
    /// 整份讀回記憶體，重算名稱：暫存檔在寫完之後壞了，pack 的名稱就與內容不符（要到 check 才看得出來）。
    pub fn into_pack(self) -> Result<FinishedPack> {
        let path = self.file.path().to_path_buf();
        let bytes = std::fs::read(&path).map_err(|e| spool_error(&path, e))?;
        if bytes.len() as u64 != self.len || ObjectId::of(&bytes) != self.id {
            return Err(CoreError::corrupt(
                path.display().to_string(),
                format!("the spooled pack {} changed after it was written", self.id),
            ));
        }
        Ok(FinishedPack {
            id: self.id,
            kind: self.kind,
            bytes,
            entries: self.entries,
        })
    }
}

impl SpoolWriter {
    /// 參數同 [`PackWriter::new`]，加上暫存檔的目錄。
    pub fn new(
        keys: Arc<Keys>,
        kind: Kind,
        target: u64,
        chunk_max: u32,
        dir: std::path::PathBuf,
    ) -> Self {
        Self {
            keys,
            kind,
            target,
            max_r: max_r(kind, chunk_max),
            dir,
            open: None,
            entries: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.open.as_ref().is_some_and(|s| s.len >= self.target)
    }

    /// 同 [`PackWriter::add`]，寫進暫存檔。
    pub fn add(&mut self, id: ItemId, plain: &[u8]) -> Result<PackEntry> {
        let sealed = seal_item(&self.keys, self.kind, id, plain)?;
        let spool = match &mut self.open {
            Some(spool) => spool,
            None => {
                let file = tempfile::Builder::new()
                    .prefix("kist-pack-")
                    .tempfile_in(&self.dir)
                    .map_err(|e| spool_error(&self.dir, e))?;
                let mut spool = Spool {
                    out: std::io::BufWriter::with_capacity(1 << 20, file),
                    hasher: blake3::Hasher::new(),
                    len: 0,
                };
                spool.write(&pack::begin())?;
                self.open.insert(spool)
            }
        };
        let entry = PackEntry {
            i: id,
            o: spool.len,
            l: sealed.len() as u64,
            r: plain.len() as u64,
        };
        spool.write(&sealed)?;
        self.entries.push(entry);
        Ok(entry)
    }

    /// 同 [`PackWriter::finish`]：接上 trailer，交出暫存檔裡的 pack，自己重置成空的。
    pub fn finish(&mut self) -> Result<Option<FinishedSpool>> {
        let Some(mut spool) = self.open.take() else {
            return Ok(None);
        };
        let entries = std::mem::take(&mut self.entries);
        let sealed = seal_trailer(&self.keys, self.kind, &entries, spool.len, self.max_r)?;
        spool.write(&pack::finish(Vec::new(), &sealed))?;
        let path = spool.out.get_ref().path().to_path_buf();
        let file = spool
            .out
            .into_inner()
            .map_err(|e| spool_error(&path, e.into_error()))?;
        Ok(Some(FinishedSpool {
            id: ObjectId::from_bytes(*spool.hasher.finalize().as_bytes()),
            kind: self.kind,
            len: spool.len,
            entries,
            file,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use kist_crypto::MasterKey;
    use kist_format::v4::ids::ClientId;

    fn sample(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 0x0f) as u8
            })
            .collect()
    }

    #[test]
    fn a_reused_context_compresses_like_a_fresh_one() {
        let sizes = [
            0usize,
            1,
            1000,
            1279,
            70_000,
            1 << 20,
            3 << 20,
            5,
            200_000,
            768,
        ];
        let mut zstd_items = 0;
        for round in 0..3u64 {
            for (k, &n) in sizes.iter().enumerate() {
                let raw = sample(round * 100 + k as u64, n);
                let fresh = zstd::encode_all(&raw[..], ZSTD_LEVEL).unwrap();
                let framed = compress(&raw).unwrap();
                let (alg, data) = pack::unframe(&framed).unwrap();
                if alg == Algorithm::Zstd {
                    assert_eq!(data, &fresh[..], "round {round} size {n}");
                    zstd_items += 1;
                } else {
                    assert!(
                        !keep_compressed(raw.len(), fresh.len()),
                        "round {round} size {n}"
                    );
                }
                assert_eq!(decompress(&framed, n as u64, "t").unwrap(), raw);
            }
        }
        assert!(
            zstd_items >= 21,
            "only {zstd_items} items took the zstd path"
        );
    }

    #[test]
    fn a_decode_that_stops_halfway_does_not_break_the_next_one() {
        let raw = sample(7, 1 << 20);
        let framed = compress(&raw).unwrap();
        assert_eq!(pack::unframe(&framed).unwrap().0, Algorithm::Zstd);
        // 超過上限：讀到一半就停
        assert!(decompress(&framed, 1000, "t").is_err());
        assert_eq!(decompress(&framed, raw.len() as u64, "t").unwrap(), raw);
        // 截斷的 frame
        let cut = &framed[..framed.len() / 2];
        assert!(decompress(cut, raw.len() as u64, "t").is_err());
        assert_eq!(decompress(&framed, raw.len() as u64, "t").unwrap(), raw);
    }

    /// 暫存檔的 pack 與記憶體裡的同一個結構：每一項的位置、長度、整個 pack 的長度相同（nonce 是亂數，bytes
    /// 不同）；讀回的 pack 驗證得過、每一項解得開、名稱是讀回的 bytes 的 BLAKE3；讀回之後暫存檔刪掉；寫完之後再加
    /// 是新的一個。
    #[test]
    fn a_spooled_data_pack_has_the_layout_of_the_in_memory_one() {
        let k = keys();
        let dir = tempfile::tempdir().unwrap();
        let chunks: Vec<Vec<u8>> = vec![
            vec![0u8; 5000],
            (0..=255u8).collect(),
            b"x".to_vec(),
            sample(3, 70_000),
        ];
        let mut m = PackWriter::new(Arc::clone(&k), Kind::Data, 1 << 20, 1024 * 1024);
        let mut w = SpoolWriter::new(
            Arc::clone(&k),
            Kind::Data,
            1 << 20,
            1024 * 1024,
            dir.path().to_path_buf(),
        );
        assert!(w.is_empty() && !w.is_full() && w.finish().unwrap().is_none());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "no file before the first item"
        );
        for c in &chunks {
            let id = k.chunk_id(c).into();
            assert_eq!(m.add(id, c).unwrap(), w.add(id, c).unwrap());
        }
        let mem = m.finish().unwrap().unwrap();
        let spooled = w.finish().unwrap().unwrap();
        assert!(w.is_empty() && w.finish().unwrap().is_none());
        assert_eq!(spooled.entries, mem.entries);
        assert_eq!(spooled.len, mem.bytes.len() as u64);
        let path = spooled.file.path().to_path_buf();
        let p = spooled.into_pack().unwrap();
        assert!(!path.exists(), "the spool file is removed once read back");
        assert_eq!(p.id, ObjectId::of(&p.bytes));
        let t = trailer_of(&k, &data_key(p.id), &p.bytes, 1024 * 1024).unwrap();
        assert_eq!(t.entries, p.entries);
        for (c, e) in chunks.iter().zip(&t.entries) {
            let sealed = &p.bytes[e.o as usize..(e.o + e.l) as usize];
            assert_eq!(
                &open_chunk(&k, &e.i.to_chunk(), sealed, e.r, 1024 * 1024).unwrap(),
                c
            );
        }
        w.add(k.chunk_id(b"again").into(), b"again").unwrap();
        let again = w.finish().unwrap().unwrap();
        assert_ne!(again.file.path(), path);
        assert_eq!(again.entries.len(), 1);
    }

    /// 寫完之後暫存檔被改了：讀回是錯誤，不是一個名稱與內容不符的 pack。
    #[test]
    fn a_spooled_pack_changed_on_disk_is_an_error() {
        let k = keys();
        let dir = tempfile::tempdir().unwrap();
        let mut w = SpoolWriter::new(
            Arc::clone(&k),
            Kind::Data,
            1 << 20,
            1024 * 1024,
            dir.path().to_path_buf(),
        );
        w.add(k.chunk_id(b"abc").into(), b"abc").unwrap();
        let spooled = w.finish().unwrap().unwrap();
        let mut bytes = std::fs::read(spooled.file.path()).unwrap();
        bytes[10] ^= 1;
        std::fs::write(spooled.file.path(), &bytes).unwrap();
        let e = spooled.into_pack().unwrap_err().to_string();
        assert!(e.contains("changed after it was written"), "{e}");
    }

    fn keys() -> Arc<Keys> {
        Arc::new(Keys::derive(&MasterKey::from_bytes([9; 32])))
    }

    fn data_key(id: ObjectId) -> Key {
        Key::Pack {
            client: ClientId::from_bytes([1; 16]),
            id,
        }
    }

    #[test]
    fn a_data_pack_round_trips_and_every_entry_verifies() {
        let k = keys();
        let mut w = PackWriter::new(Arc::clone(&k), Kind::Data, 64 * 1024, 1024 * 1024);
        let chunks: Vec<Vec<u8>> = vec![vec![0u8; 5000], (0..=255u8).collect(), b"x".to_vec()];
        for c in &chunks {
            w.add(k.chunk_id(c).into(), c).unwrap();
        }
        let p = w.finish().unwrap().unwrap();
        assert!(w.is_empty() && w.finish().unwrap().is_none());
        assert_eq!(p.id, ObjectId::of(&p.bytes));
        let t = trailer_of(&k, &data_key(p.id), &p.bytes, 1024 * 1024).unwrap();
        assert_eq!(t.entries, p.entries);
        for (c, e) in chunks.iter().zip(&t.entries) {
            let sealed = &p.bytes[e.o as usize..(e.o + e.l) as usize];
            let got = open_chunk(&k, &e.i.to_chunk(), sealed, e.r, 1024 * 1024).unwrap();
            assert_eq!(&got, c);
        }
        // 5000 個 0 壓得動、256 個不同的 byte 壓不動：存原文
        assert!(t.entries[0].l < 5000);
        assert_eq!(t.entries[1].l, 256 + 1 + 24 + 16);
        // 放在 mpacks/ 底下就是種類不符
        let wrong = Key::Mpack {
            client: ClientId::from_bytes([1; 16]),
            id: p.id,
            replica: false,
        };
        assert!(trailer_of(&k, &wrong, &p.bytes, 1024 * 1024).is_err());
    }

    #[test]
    fn a_tampered_or_misnamed_entry_does_not_verify() {
        let k = keys();
        let mut w = PackWriter::new(Arc::clone(&k), Kind::Meta, 64 * 1024, 1024);
        let item = b"\xa2avbk\x00".to_vec();
        let id = k.meta_id(&item);
        let e = w.add(id.into(), &item).unwrap();
        let p = w.finish().unwrap().unwrap();
        let sealed = p.bytes[e.o as usize..(e.o + e.l) as usize].to_vec();
        assert_eq!(open_meta(&k, &id, &sealed, e.r).unwrap(), item);
        let other = MetaId::from_bytes([7; 32]);
        assert!(open_meta(&k, &other, &sealed, e.r).is_err(), "wrong ID");
        assert!(open_meta(&k, &id, &sealed, e.r + 1).is_err(), "wrong r");
        let mut bad = sealed.clone();
        bad[30] ^= 1;
        assert!(open_meta(&k, &id, &bad, e.r).is_err(), "tampered");
    }

    /// 解壓上限與 `r` 無關（§6）：宣稱很短的 item 解出來超過 2 MiB 就是腐壞。
    #[test]
    fn the_decompression_limit_does_not_trust_r() {
        let k = keys();
        let big = vec![0u8; MAX_META as usize + 1];
        let framed = pack::frame(
            Algorithm::Zstd,
            &zstd::encode_all(&big[..], ZSTD_LEVEL).unwrap(),
        );
        let id = k.meta_id(&big);
        let sealed = k.seal_meta(&id, &framed).unwrap();
        let err = open_meta(&k, &id, &sealed, 10).unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
        let raw = pack::frame(Algorithm::Raw, &big);
        let sealed = k.seal_meta(&id, &raw).unwrap();
        assert!(open_meta(&k, &id, &sealed, big.len() as u64).is_err());
    }
}
