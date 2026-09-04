//! 測試共用：小參數的 repo、產生測試資料、比對目錄。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kist_backend::Backend;
use kist_core::{BackupOptions, InitOptions, Repository};
use kist_crypto::KdfCost;
use kist_format::config::ChunkerParams;
use rand::{RngExt, SeedableRng};

pub const PASSWORD: &str = "test password";

/// 小 chunk、小 pack：讓小量資料就能跑出多 chunk、多 pack、indirect content。
pub fn init_options() -> InitOptions {
    InitOptions {
        chunker: ChunkerParams {
            min: 4 * 1024,
            avg: 16 * 1024,
            max: 64 * 1024,
        },
        pack_target_size: 256 * 1024,
        kdf_cost: KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
    }
}

pub fn backup_options() -> BackupOptions {
    BackupOptions {
        client_id: [0x11; 16],
        hostname: "testhost".to_owned(),
        username: "tester".to_owned(),
        now: None,
        gc_grace: kist_core::DEFAULT_GC_GRACE,
    }
}

pub struct TestRepo {
    pub dir: tempfile::TempDir,
    pub backend: Backend,
}

impl TestRepo {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::local(&dir.path().join("repo")).unwrap();
        Repository::init(backend.clone(), PASSWORD.as_bytes(), init_options())
            .await
            .unwrap();
        Self { dir, backend }
    }

    pub async fn open(&self) -> Repository {
        Repository::open(self.backend.clone(), PASSWORD.as_bytes())
            .await
            .unwrap()
    }

    pub fn repo_path(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// 某個 prefix 底下的物件數。
    pub fn count(&self, prefix: &str) -> usize {
        let p = self.repo_path().join(prefix);
        if !p.exists() {
            return 0;
        }
        walk_files(&p).into_iter().filter(|p| p.is_file()).count()
    }
}

pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut v = vec![0u8; len];
    rng.fill(&mut v[..]);
    v
}

/// 建一組有代表性的測試資料：空檔、小檔、多 chunk 檔、可壓縮檔、子目錄、空目錄。
pub fn make_source(root: &Path) {
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    std::fs::create_dir_all(root.join("empty-dir")).unwrap();
    std::fs::write(root.join("empty.txt"), b"").unwrap();
    std::fs::write(root.join("small.txt"), b"hello kist\n").unwrap();
    std::fs::write(root.join("random.bin"), random_bytes(1, 300 * 1024)).unwrap();
    std::fs::write(root.join("zeros.bin"), vec![0u8; 200 * 1024]).unwrap();
    std::fs::write(root.join("sub/a.txt"), b"a").unwrap();
    std::fs::write(root.join("sub/deeper/b.bin"), random_bytes(2, 70 * 1024)).unwrap();
    // 名稱含非 ASCII
    std::fs::write(root.join("sub/中文檔名.txt"), "內容").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("small.txt", root.join("link")).unwrap();
}

/// 遞迴列出所有項目（相對路徑 → 內容），symlink 記錄目標，目錄記錄 `<dir>`。
pub fn snapshot_dir(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    for p in walk_files(root) {
        let rel = p.strip_prefix(root).unwrap().to_path_buf();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        let content = if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&p).unwrap();
            format!("symlink -> {}", target.display()).into_bytes()
        } else if meta.is_dir() {
            b"<dir>".to_vec()
        } else {
            std::fs::read(&p).unwrap()
        };
        out.insert(rel, content);
    }
    out
}

/// 所有檔案、目錄、symlink 的路徑（含空目錄），已排序。
pub fn walk_files(root: &Path) -> Vec<PathBuf> {
    fn rec(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            out.push(p.clone());
            if meta.is_dir() {
                rec(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    rec(root, &mut out);
    out.sort();
    out
}

/// 比對兩個目錄：清單、內容、mtime。
pub fn assert_same_tree(a: &Path, b: &Path) {
    let sa = snapshot_dir(a);
    let sb = snapshot_dir(b);
    let ka: Vec<_> = sa.keys().collect();
    let kb: Vec<_> = sb.keys().collect();
    assert_eq!(ka, kb, "檔案清單不同");
    for (k, va) in &sa {
        assert_eq!(va, &sb[k], "內容不同：{}", k.display());
        let ma = std::fs::symlink_metadata(a.join(k)).unwrap();
        let mb = std::fs::symlink_metadata(b.join(k)).unwrap();
        if !ma.file_type().is_symlink() {
            assert_eq!(
                filetime::FileTime::from_last_modification_time(&ma),
                filetime::FileTime::from_last_modification_time(&mb),
                "mtime 不同：{}",
                k.display()
            );
        }
    }
}
