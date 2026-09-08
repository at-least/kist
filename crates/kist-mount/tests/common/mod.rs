//! 整合測試共用：小參數的 repo（對齊 kist-core tests/common 的做法）。

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use kist_backend::Backend;
use kist_core::{BackupOptions, InitOptions, Repository};
use kist_crypto::KdfCost;
use kist_format::config::ChunkerParams;

pub const PASSWORD: &str = "mount test password";

pub const CLIENT: &str = "22222222222222222222222222222222";

/// 小 chunk、小 pack：幾百 KB 就能跑出多 chunk；8 MiB 隨機檔會變 indirect content。
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
        // 副本行為由專門測試覆蓋；這裡的斷言不預期 `.r1` 物件。
        replicas: Some(0),
    }
}

pub fn backup_options() -> BackupOptions {
    BackupOptions {
        client_id: [0x22; 16],
        hostname: "mounthost".to_owned(),
        username: "tester".to_owned(),
        now: None,
        gc_grace: kist_core::DEFAULT_GC_GRACE,
        parity: 0,
        progress: None,
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

    /// 備份 `src`，回傳 snapshot 的 client hex（這組測試固定 0x22…）。
    pub async fn backup(&self, src: &Path) {
        let repo = self.open().await;
        repo.backup(std::slice::from_ref(&PathBuf::from(src)), backup_options())
            .await
            .unwrap();
    }

    /// 打一個 session：`kist_mount::FsCore`（不掛載、直接驅動核心邏輯）。
    pub async fn fs_core(&self) -> kist_mount::FsCore {
        let repo = self.open().await;
        kist_mount::FsCore::new(repo, kist_mount::MountConfig::default())
            .await
            .unwrap()
    }
}
