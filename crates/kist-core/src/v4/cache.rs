//! v4 的本地快取：`<cache_root>/<keyed BLAKE3(K_cache, repo 位置) 的前 16 bytes 的 hex>/`（§3）。
//!
//! - `gen`：看過的最大世代（B1 從它開始以 start-after 列出 `maint/gen/`）。
//! - `last-snapshot`：自己最後一個 snapshot 的 key（ADR 026 第 8 層：消失時警告）。
//! - `shards/<hex>`、`deltas/<hex>`、`mpacks/<hex>`：以內容命名的物件的 bytes（寫進 repo 的原樣，還是
//!   密封的）。讀出時重算 BLAKE3，不等於名稱就刪掉、當成沒有。
//! - `verified`：這台 client 取回並驗證過的 (item ID, pack 名稱)，每筆 64 bytes（B2 的第三種可信）。pack
//!   以內容命名、不會被覆寫，所以驗證過一次就一直成立；那個 pack 被刪掉之後，這一筆由 [`Cache::compact_verified`]
//!   移除。
//!
//! 快取只省時間：讀不到就當沒有、寫失敗只警告。`verified` 是例外的那一個——它讓 backup 不重新下載就
//! 沿用別台的副本；它在本機磁碟上，能改它的人本來就控制了這台 client。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use kist_format::v4::ids::{ItemId, ObjectId};
use kist_format::v4::keys::Gen;

/// 以內容命名的物件種類（快取裡的子目錄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Shard,
    Delta,
    Mpack,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Shard => "shards",
            Kind::Delta => "deltas",
            Kind::Mpack => "mpacks",
        }
    }
}

const VERIFIED_RECORD: usize = 64;

#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

/// 寫入一律先寫暫存檔再 rename：中途當掉只會少一個檔案。暫存檔名帶 pid 與亂數，同時開著這個 repo 的
/// 兩個程序不會寫到同一個暫存檔。
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let nonce = kist_crypto::random_bytes::<8>().map_err(std::io::Error::other)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        hex::encode(nonce)
    ));
    let tmp = dir.join(name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn warn_write(what: &str, path: &Path, e: &std::io::Error) {
    tracing::warn!(
        "cannot update the local cache ({what}, {}): {e}",
        path.display()
    );
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn read_text(&self, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(name)).ok()?;
        Some(text.trim().to_owned())
    }

    fn write_text(&self, name: &str, text: &str) {
        let path = self.dir.join(name);
        if let Err(e) = write_atomic(&path, text.as_bytes()) {
            warn_write(name, &path, &e);
        }
    }

    /// 看過的最大世代；沒有或讀不懂就是 `None`。
    pub fn seen_gen(&self) -> Option<Gen> {
        self.read_text("gen")?.parse().ok()
    }

    pub fn set_seen_gen(&self, g: Gen) {
        self.write_text("gen", &g.to_string());
    }

    /// 自己最後一個 snapshot 的 key。
    pub fn last_snapshot(&self) -> Option<String> {
        self.read_text("last-snapshot").filter(|s| !s.is_empty())
    }

    pub fn set_last_snapshot(&self, key: &str) {
        self.write_text("last-snapshot", key);
    }

    fn object_path(&self, kind: Kind, id: &ObjectId) -> PathBuf {
        self.dir.join(kind.dir()).join(id.to_hex())
    }

    /// 以內容命名的物件；BLAKE3 不等於名稱的檔案刪掉、當成沒有。
    pub fn get(&self, kind: Kind, id: &ObjectId) -> Option<Vec<u8>> {
        let path = self.object_path(kind, id);
        let bytes = std::fs::read(&path).ok()?;
        if ObjectId::of(&bytes) == *id {
            return Some(bytes);
        }
        tracing::warn!(
            "local cache file {} does not match its name; dropping it",
            path.display()
        );
        let _ = std::fs::remove_file(&path);
        None
    }

    pub fn put(&self, kind: Kind, id: &ObjectId, bytes: &[u8]) {
        let path = self.object_path(kind, id);
        if let Err(e) = write_atomic(&path, bytes) {
            warn_write(kind.dir(), &path, &e);
        }
    }

    /// 只留 `keep` 裡的物件：分片跟著 head、delta 跟著列表、mpack 跟著 map 與 delta。
    pub fn retain(&self, kind: Kind, keep: &HashSet<ObjectId>) {
        let dir = self.dir.join(kind.dir());
        let Ok(read) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // 別的程序寫到一半的暫存檔不動
            if name.contains(".tmp-") {
                continue;
            }
            let known = ObjectId::from_hex(name).is_ok_and(|id| keep.contains(&id));
            if !known {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    fn verified_path(&self) -> PathBuf {
        self.dir.join("verified")
    }

    /// 驗證過的 (item, pack)。最後一筆寫到一半（當掉）就不算。
    pub fn verified(&self) -> HashSet<(ItemId, ObjectId)> {
        let Ok(bytes) = std::fs::read(self.verified_path()) else {
            return HashSet::new();
        };
        bytes
            .as_chunks::<VERIFIED_RECORD>()
            .0
            .iter()
            .map(|r| {
                let mut item = [0u8; 32];
                let mut pack = [0u8; 32];
                item.copy_from_slice(&r[..32]);
                pack.copy_from_slice(&r[32..]);
                (ItemId::from_bytes(item), ObjectId::from_bytes(pack))
            })
            .collect()
    }

    /// 接在 `verified` 後面。
    pub fn add_verified(&self, pairs: &[(ItemId, ObjectId)]) {
        if pairs.is_empty() {
            return;
        }
        let path = self.verified_path();
        let result = (|| {
            std::fs::create_dir_all(&self.dir)?;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            let mut buf = Vec::with_capacity(pairs.len() * VERIFIED_RECORD);
            for (item, pack) in pairs {
                buf.extend_from_slice(item.as_bytes());
                buf.extend_from_slice(pack.as_bytes());
            }
            f.write_all(&buf)
        })();
        if let Err(e) = result {
            warn_write("verified", &path, &e);
        }
    }

    /// 移除 pack 已經不在的記錄（`live` 回 false 的）；沒有要移除的就不動檔案。
    pub fn compact_verified(&self, live: impl Fn(&ObjectId) -> bool) {
        let all = self.verified();
        let kept: Vec<_> = all.iter().filter(|(_, p)| live(p)).copied().collect();
        if kept.len() == all.len() {
            return;
        }
        let mut buf = Vec::with_capacity(kept.len() * VERIFIED_RECORD);
        for (item, pack) in &kept {
            buf.extend_from_slice(item.as_bytes());
            buf.extend_from_slice(pack.as_bytes());
        }
        let path = self.verified_path();
        if let Err(e) = write_atomic(&path, &buf) {
            warn_write("verified", &path, &e);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn objects_are_checked_against_their_names() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path().join("c"));
        let bytes = b"shard bytes".to_vec();
        let id = ObjectId::of(&bytes);
        assert!(c.get(Kind::Shard, &id).is_none());
        c.put(Kind::Shard, &id, &bytes);
        assert_eq!(c.get(Kind::Shard, &id).unwrap(), bytes);
        std::fs::write(c.object_path(Kind::Shard, &id), b"tampered").unwrap();
        assert!(c.get(Kind::Shard, &id).is_none());
        assert!(
            !c.object_path(Kind::Shard, &id).exists(),
            "a bad file is dropped"
        );
    }

    #[test]
    fn retain_drops_what_is_no_longer_referenced() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path().join("c"));
        let a = ObjectId::of(b"a");
        let b = ObjectId::of(b"b");
        c.put(Kind::Mpack, &a, b"a");
        c.put(Kind::Mpack, &b, b"b");
        c.retain(Kind::Mpack, &HashSet::from([a]));
        assert!(c.get(Kind::Mpack, &a).is_some());
        assert!(c.get(Kind::Mpack, &b).is_none());
    }

    #[test]
    fn verified_pairs_append_survive_a_torn_tail_and_compact() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path().join("c"));
        let p1 = ObjectId::from_bytes([1; 32]);
        let p2 = ObjectId::from_bytes([2; 32]);
        let i = |b: u8| ItemId::from_bytes([b; 32]);
        c.add_verified(&[(i(1), p1), (i(2), p2)]);
        c.add_verified(&[(i(3), p1)]);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(c.verified_path())
            .unwrap();
        f.write_all(&[9; 40]).unwrap();
        assert_eq!(
            c.verified(),
            HashSet::from([(i(1), p1), (i(2), p2), (i(3), p1)])
        );
        c.compact_verified(|p| *p == p1);
        assert_eq!(c.verified(), HashSet::from([(i(1), p1), (i(3), p1)]));
    }

    #[test]
    fn generation_and_last_snapshot_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path().join("c"));
        assert_eq!(c.seen_gen(), None);
        c.set_seen_gen(17);
        assert_eq!(c.seen_gen(), Some(17));
        c.set_last_snapshot("snapshots/x/y");
        assert_eq!(c.last_snapshot().as_deref(), Some("snapshots/x/y"));
    }
}
