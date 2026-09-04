//! chunk index：chunk ID → 在哪個 pack 的哪裡。
//!
//! 兩層：
//! - **overlay**（記憶體 `HashMap`）：這次 backup 新寫的 chunk（pack 還沒 flush 前先用零值
//!   pack 名稱佔位，flush 後補上），以及沒有快取時從 repo 讀進來的全部內容。
//! - **base**（[`DiskTable`]，可選）：本地快取檔，依 chunk ID 排序的固定長度紀錄，
//!   查詢用二分搜尋、每次只讀一筆（`read_exact_at`），不整份載入記憶體；
//!   熱資料由 OS page cache 負責。這是 PLAN 說的「mmap 的 sorted table」，但不用 mmap
//!   （`memmap2` 的 API 是 `unsafe fn`，而所有 crate 都 `forbid(unsafe_code)`），
//!   效果相同、零 unsafe。

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::Arc;

use kist_format::index::IndexPack;
use kist_format::pack::PackEntry;
use kist_format::{ChunkId, ObjectId};

use crate::{CoreError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLocation {
    pub pack: ObjectId,
    pub offset: u64,
    pub length: u64,
    pub raw_len: u64,
    pub flags: u8,
}

/// 尚未 flush 的 pack 用這個名稱佔位。
pub const PENDING_PACK: ObjectId = ObjectId::from_bytes([0; 32]);

/// 磁碟表的一筆紀錄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableRecord {
    pub id: ChunkId,
    pub location: ChunkLocation,
}

const TABLE_MAGIC: &[u8; 8] = b"KISTIDX1";
const TABLE_VERSION: u32 = 1;
const HEADER_LEN: u64 = 32;
const RECORD_LEN: u64 = 96;

impl TableRecord {
    fn encode(&self) -> [u8; RECORD_LEN as usize] {
        let mut b = [0u8; RECORD_LEN as usize];
        b[0..32].copy_from_slice(self.id.as_bytes());
        b[32..64].copy_from_slice(self.location.pack.as_bytes());
        b[64..72].copy_from_slice(&self.location.offset.to_le_bytes());
        b[72..80].copy_from_slice(&self.location.length.to_le_bytes());
        b[80..88].copy_from_slice(&self.location.raw_len.to_le_bytes());
        b[88] = self.location.flags;
        b
    }

    fn decode(b: &[u8; RECORD_LEN as usize]) -> Self {
        let mut id = [0u8; 32];
        id.copy_from_slice(&b[0..32]);
        let mut pack = [0u8; 32];
        pack.copy_from_slice(&b[32..64]);
        let u = |r: std::ops::Range<usize>| {
            let mut x = [0u8; 8];
            x.copy_from_slice(&b[r]);
            u64::from_le_bytes(x)
        };
        Self {
            id: ChunkId::from_bytes(id),
            location: ChunkLocation {
                pack: ObjectId::from_bytes(pack),
                offset: u(64..72),
                length: u(72..80),
                raw_len: u(80..88),
                flags: b[88],
            },
        }
    }
}

/// 依 chunk ID 排序的固定長度紀錄表（本地快取檔）。
///
/// ```text
/// 0   8  magic "KISTIDX1"
/// 8   4  版本 = 1
/// 12  4  保留
/// 16  8  紀錄數
/// 24  8  保留
/// 32  …  紀錄 × N，每筆 96 bytes：id 32 ‖ pack 32 ‖ offset 8 ‖ length 8 ‖ raw_len 8 ‖ flags 1 ‖ pad 7
/// ```
/// 這是本機快取的格式，不是 repo 格式（v1 凍結不受影響）。
#[derive(Debug)]
pub struct DiskTable {
    file: File,
    len: u64,
}

impl DiskTable {
    /// 排序、去重（同 ID 保留先出現的），寫成表。先寫暫存檔再 rename。
    pub fn build(path: &Path, mut records: Vec<TableRecord>) -> Result<Self> {
        records.sort_by_key(|r| r.id);
        records.dedup_by(|later, earlier| later.id == earlier.id);
        let tmp = path.with_extension("tbl.tmp");
        {
            let file = File::create(&tmp).map_err(|e| CoreError::io(&tmp, e))?;
            let mut w = BufWriter::new(file);
            let mut header = [0u8; HEADER_LEN as usize];
            header[0..8].copy_from_slice(TABLE_MAGIC);
            header[8..12].copy_from_slice(&TABLE_VERSION.to_le_bytes());
            header[16..24].copy_from_slice(&(records.len() as u64).to_le_bytes());
            w.write_all(&header).map_err(|e| CoreError::io(&tmp, e))?;
            for r in &records {
                w.write_all(&r.encode())
                    .map_err(|e| CoreError::io(&tmp, e))?;
            }
            w.flush().map_err(|e| CoreError::io(&tmp, e))?;
            w.into_inner()
                .map_err(|e| CoreError::io(&tmp, e.into_error()))?
                .sync_all()
                .map_err(|e| CoreError::io(&tmp, e))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e))?;
        Self::open(path)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let corrupt = |reason: &str| CoreError::Corrupt {
            key: path.display().to_string(),
            reason: reason.to_owned(),
        };
        let mut file = File::open(path).map_err(|e| CoreError::io(path, e))?;
        let mut header = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut header)
            .map_err(|_| corrupt("index cache too short"))?;
        if &header[0..8] != TABLE_MAGIC {
            return Err(corrupt("not an index cache table"));
        }
        let mut v = [0u8; 4];
        v.copy_from_slice(&header[8..12]);
        if u32::from_le_bytes(v) != TABLE_VERSION {
            return Err(corrupt("unsupported index cache version"));
        }
        let mut n = [0u8; 8];
        n.copy_from_slice(&header[16..24]);
        let len = u64::from_le_bytes(n);
        let size = file.metadata().map_err(|e| CoreError::io(path, e))?.len();
        if size != HEADER_LEN + len * RECORD_LEN {
            return Err(corrupt("index cache size does not match its record count"));
        }
        Ok(Self { file, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn read_record(&self, i: u64) -> Result<TableRecord> {
        let mut buf = [0u8; RECORD_LEN as usize];
        read_exact_at(&self.file, &mut buf, HEADER_LEN + i * RECORD_LEN)
            .map_err(|e| CoreError::io("<index cache>", e))?;
        Ok(TableRecord::decode(&buf))
    }

    /// 二分搜尋，每步讀一筆。
    pub fn get(&self, id: &ChunkId) -> Result<Option<ChunkLocation>> {
        let (mut lo, mut hi) = (0u64, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = self.read_record(mid)?;
            match r.id.cmp(id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(Some(r.location)),
            }
        }
        Ok(None)
    }

    /// 依序讀出全部紀錄（重建、合併用）。
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<TableRecord>> + '_> {
        Ok((0..self.len).map(move |i| self.read_record(i)))
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short read",
            ));
        }
        done += n;
    }
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct ChunkIndex {
    overlay: HashMap<ChunkId, ChunkLocation>,
    base: Option<Arc<DiskTable>>,
    /// pack 名稱 → 檔案大小（`check` 用）。
    packs: HashMap<ObjectId, u64>,
}

impl ChunkIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 以本地快取表為底、pack 清單另外給。
    pub fn with_base(table: Arc<DiskTable>, packs: HashMap<ObjectId, u64>) -> Self {
        Self {
            overlay: HashMap::new(),
            base: Some(table),
            packs,
        }
    }

    pub fn is_cached(&self) -> bool {
        self.base.is_some()
    }

    fn base_get(&self, id: &ChunkId) -> Option<ChunkLocation> {
        let table = self.base.as_ref()?;
        match table.get(id) {
            Ok(loc) => loc,
            Err(e) => {
                // 快取檔讀不到就當沒有：頂多多上傳一次，不會少
                tracing::warn!("index cache read failed: {e}");
                None
            }
        }
    }

    pub fn contains(&self, id: &ChunkId) -> bool {
        self.overlay.contains_key(id) || self.base_get(id).is_some()
    }

    pub fn get(&self, id: &ChunkId) -> Option<ChunkLocation> {
        self.overlay.get(id).copied().or_else(|| self.base_get(id))
    }

    /// chunk 總數（overlay 與 base 可能重疊，重疊時算一次）。
    pub fn len(&self) -> usize {
        let base_len = self.base.as_ref().map_or(0, |t| t.len() as usize);
        let overlap = match &self.base {
            Some(_) => self
                .overlay
                .keys()
                .filter(|id| self.base_get(id).is_some())
                .count(),
            None => 0,
        };
        base_len + self.overlay.len() - overlap
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 全部 (id, location)，overlay 優先。base 的部分逐筆從檔案讀。
    pub fn chunks(&self) -> Vec<(ChunkId, ChunkLocation)> {
        let mut out: Vec<(ChunkId, ChunkLocation)> =
            self.overlay.iter().map(|(k, v)| (*k, *v)).collect();
        if let Some(table) = &self.base {
            if let Ok(iter) = table.iter() {
                for r in iter.flatten() {
                    if !self.overlay.contains_key(&r.id) {
                        out.push((r.id, r.location));
                    }
                }
            }
        }
        out
    }

    pub fn packs(&self) -> impl Iterator<Item = (&ObjectId, &u64)> {
        self.packs.iter()
    }

    pub fn pack_count(&self) -> usize {
        self.packs.len()
    }

    /// 把一個 index blob 裡的 pack 加進來。同一個 chunk 出現在多個 pack 時保留先加入的。
    pub fn add_pack(&mut self, pack: &IndexPack) {
        self.packs.insert(pack.pack, pack.size);
        for e in &pack.entries {
            self.overlay.entry(e.id).or_insert(location(pack.pack, e));
        }
    }

    /// backup 進行中：pack 尚未 flush，先佔位。
    pub fn add_pending(&mut self, entry: &PackEntry) {
        self.overlay
            .entry(entry.id)
            .or_insert(location(PENDING_PACK, entry));
    }

    /// pack flush 完成：把佔位的名稱換成真正的 pack 名稱。
    pub fn resolve_pending(&mut self, pack: ObjectId, size: u64, entries: &[PackEntry]) {
        self.packs.insert(pack, size);
        for e in entries {
            if let Some(loc) = self.overlay.get_mut(&e.id) {
                if loc.pack == PENDING_PACK {
                    *loc = location(pack, e);
                }
            }
        }
    }
}

fn location(pack: ObjectId, e: &PackEntry) -> ChunkLocation {
    ChunkLocation {
        pack,
        offset: e.offset,
        length: e.length,
        raw_len: e.raw_len,
        flags: e.flags,
    }
}
