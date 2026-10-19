//! B2 的「這次已經寫過」（M12 第 7 段之四第四步）：這次 backup 寫進 pack 的 item ID。
//!
//! 記憶體裡最多 [`MEM_CAP`] 個；滿了就排好寫成一段暫存檔（一個 ID 32 bytes），記一個 bloom filter（每個 ID 10 bits、
//! 7 個 hash）。查的時候先查記憶體、再看每一段的 bloom，說可能有才到那一段的檔案裡二分搜尋。記憶體是記憶體裡的那一段
//! 加上 bloom：原本的 `HashSet` 一個 ID 約 35 bytes（100 萬個 chunk 33 MiB），這裡多出來的只有每個 ID 1.25 bytes。
//!
//! 暫存檔放在 `parent` 下的一個新目錄（本地快取的目錄；沒有快取時是系統的暫存目錄），第一次寫出時才建，這個值 drop 的
//! 時候刪掉。暫存目錄在 tmpfs 上時，寫出去的還是在記憶體裡（cgroup 照算）。

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use kist_format::v4::ids::ItemId;

use crate::{CoreError, Result};

/// 記憶體裡最多幾個：hashbrown 的負載上限是 7/8，這個數剛好是 2^18 個 bucket（約 8.6 MiB）。
const MEM_CAP: usize = (1 << 18) / 8 * 7;
const BITS_PER_ID: u64 = 10;
const HASHES: u64 = 7;
const ID_LEN: usize = 32;

pub(crate) struct WrittenSet {
    mem: HashSet<ItemId>,
    cap: usize,
    parent: PathBuf,
    dir: Option<tempfile::TempDir>,
    runs: Vec<Run>,
}

/// 寫出去的一段：排好的 ID 與它的 bloom filter。
struct Run {
    path: PathBuf,
    file: File,
    ids: u64,
    bloom: Vec<u64>,
}

fn io_error(path: &Path, source: std::io::Error) -> CoreError {
    CoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// ID 是 keyed BLAKE3 的輸出，本身就是均勻的：前 16 bytes 當兩個 hash（double hashing）。
fn bloom_bits(id: &ItemId, nbits: u64) -> impl Iterator<Item = u64> {
    let b = id.as_bytes();
    let mut h1 = [0u8; 8];
    let mut h2 = [0u8; 8];
    h1.copy_from_slice(&b[0..8]);
    h2.copy_from_slice(&b[8..16]);
    let (h1, h2) = (u64::from_le_bytes(h1), u64::from_le_bytes(h2) | 1);
    (0..HASHES).map(move |j| h1.wrapping_add(j.wrapping_mul(h2)) % nbits)
}

impl Run {
    fn may_contain(&self, id: &ItemId) -> bool {
        let nbits = self.bloom.len() as u64 * 64;
        bloom_bits(id, nbits).all(|b| self.bloom[(b / 64) as usize] >> (b % 64) & 1 == 1)
    }

    fn find(&self, id: &ItemId) -> Result<bool> {
        let mut f = &self.file;
        let (mut lo, mut hi) = (0u64, self.ids);
        let mut rec = [0u8; ID_LEN];
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            f.seek(SeekFrom::Start(mid * ID_LEN as u64))
                .and_then(|_| f.read_exact(&mut rec))
                .map_err(|e| io_error(&self.path, e))?;
            match rec.cmp(id.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(true),
            }
        }
        Ok(false)
    }
}

impl WrittenSet {
    pub(crate) fn new(parent: PathBuf) -> Self {
        Self::with_cap(parent, MEM_CAP)
    }

    fn with_cap(parent: PathBuf, cap: usize) -> Self {
        Self {
            mem: HashSet::new(),
            cap,
            parent,
            dir: None,
            runs: Vec::new(),
        }
    }

    pub(crate) fn contains(&self, id: &ItemId) -> Result<bool> {
        if self.mem.contains(id) {
            return Ok(true);
        }
        for r in &self.runs {
            if r.may_contain(id) && r.find(id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn insert(&mut self, id: ItemId) -> Result<()> {
        self.mem.insert(id);
        if self.mem.len() >= self.cap {
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> Result<()> {
        let dir = match &self.dir {
            Some(d) => d.path().to_path_buf(),
            None => {
                let d = tempfile::Builder::new()
                    .prefix("kist-written-")
                    .tempdir_in(&self.parent)
                    .map_err(|e| io_error(&self.parent, e))?;
                let p = d.path().to_path_buf();
                self.dir = Some(d);
                p
            }
        };
        let mut ids: Vec<ItemId> = self.mem.drain().collect();
        ids.sort_unstable();
        let path = dir.join(format!("run-{}", self.runs.len()));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        let nbits = (ids.len() as u64 * BITS_PER_ID).div_ceil(64) * 64;
        let mut bloom = vec![0u64; (nbits / 64) as usize];
        let mut w = BufWriter::with_capacity(1 << 20, file);
        for id in &ids {
            w.write_all(id.as_bytes()).map_err(|e| io_error(&path, e))?;
            for b in bloom_bits(id, nbits) {
                bloom[(b / 64) as usize] |= 1 << (b % 64);
            }
        }
        let file = w
            .into_inner()
            .map_err(|e| io_error(&path, e.into_error()))?;
        self.runs.push(Run {
            path,
            file,
            ids: ids.len() as u64,
            bloom,
        });
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn id(i: u64) -> ItemId {
        ItemId::from_bytes(*blake3::hash(&i.to_le_bytes()).as_bytes())
    }

    /// 寫出好幾段之後，加過的每一個都查得到、沒加過的都查不到（bloom 誤判的那些到檔案裡確認是沒有）；暫存目錄在
    /// drop 時刪掉。
    #[test]
    fn every_inserted_id_is_found_after_spilling_and_no_other() {
        let parent = tempfile::tempdir().unwrap();
        let mut set = WrittenSet::with_cap(parent.path().to_path_buf(), 100);
        for i in 0..1000 {
            assert!(!set.contains(&id(i)).unwrap(), "{i} before insert");
            set.insert(id(i)).unwrap();
        }
        assert_eq!(set.runs.len(), 10);
        for i in 0..1000 {
            assert!(set.contains(&id(i)).unwrap(), "{i} after insert");
        }
        let mut maybe = 0;
        for i in 1000..21_000 {
            let other = id(i);
            maybe += set.runs.iter().filter(|r| r.may_contain(&other)).count();
            assert!(!set.contains(&other).unwrap(), "{i} never inserted");
        }
        // 每一段誤判約 1%：20 000 個 × 10 段裡有幾百次要到檔案裡確認
        assert!(maybe > 0, "the bloom filters never said maybe");
        let dir = set.dir.as_ref().unwrap().path().to_path_buf();
        assert!(dir.exists());
        drop(set);
        assert!(!dir.exists());
    }

    /// 不滿的時候不碰磁碟。
    #[test]
    fn a_small_set_stays_in_memory() {
        let parent = tempfile::tempdir().unwrap();
        let mut set = WrittenSet::with_cap(parent.path().to_path_buf(), 100);
        for i in 0..99 {
            set.insert(id(i)).unwrap();
        }
        assert!(set.dir.is_none());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    /// 暫存檔讀不了是錯誤，不是「沒寫過」。
    #[test]
    fn a_damaged_run_is_an_error() {
        let parent = tempfile::tempdir().unwrap();
        let mut set = WrittenSet::with_cap(parent.path().to_path_buf(), 10);
        for i in 0..10 {
            set.insert(id(i)).unwrap();
        }
        set.runs[0].file.set_len(0).unwrap();
        let e = set.contains(&id(3)).unwrap_err().to_string();
        assert!(e.contains("run-0"), "{e}");
    }
}
