//! kist-mount：把 repo 的 snapshots 掛成**唯讀**檔案系統（FUSE）。
//!
//! 佈局：`<client id hex>/<timestamp>/<備份的樹>`。備份來源是絕對路徑
//! （`/tmp/x/src`），根目錄會展開成虛擬的中介層級讓你一路瀏覽下去
//! （見 [`vpath`]）。頂兩層 volatile（TTL 1 秒，重複 ls 看得到新 snapshot）、
//! snapshot 內容 immutable（TTL 24 小時）。檔案讀取走 index 的
//! `raw_len` 直接定位 chunk（隨機讀不用先解前面的資料），解密後的 chunk
//! 有小 LRU（預設 8 顆 = 8 MiB chunk 時 64 MiB 上限）。
//!
//! 執行模型：fuser 0.18 的 callback 是同步 `&self`，非同步工作用共用的
//! tokio runtime 完成——[`mount`] 把 runtime worker 開成 fuser 執行緒的
//! 兩倍以上，fuser 執行緒 park 在 `block_on`、worker 才跑任務。
//!
//! 平台：Linux / macOS（fuser 支援的）。Windows 沒有這個 crate 的 FUSE 部分
//! （純邏輯模組仍可編譯、可測）。

pub mod corefs;
pub mod offsets;
pub mod vpath;

use std::path::{Path, PathBuf};

use kist_core::Repository;

#[cfg(unix)]
mod fuse;

pub use corefs::{Attr, DirEntryData, FsCore, FsError, Kind, Lookup, MountConfig};

#[cfg(unix)]
pub use fuse::KistFs;

/// 掛起來的檔案系統：drop 前請呼叫 [`Mounted::unmount`]（CLI 收到
/// SIGINT/SIGTERM 時）。檔案還開著時 unmount 會失敗（EBUSY）——那是 FUSE 的
/// 行為，錯誤會原樣回給呼叫端。
#[cfg(unix)]
pub struct Mounted {
    session: Option<fuser::BackgroundSession>,
    dir: PathBuf,
    /// callback 的執行環境：**必須活到卸載**。放進 `Mounted` 才不會在 `mount()`
    /// 結束時被 drop（async context 裡 drop runtime 會 panic，event loop 也會死）。
    _rt: Option<tokio::runtime::Runtime>,
}

#[cfg(unix)]
impl Mounted {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 卸載並等 event loop 收工。有東西還開著 → EBUSY。
    pub fn unmount(mut self) -> std::io::Result<()> {
        match self.session.take() {
            Some(session) => session.umount_and_join(),
            None => Ok(()),
        }
    }
}

#[cfg(unix)]
impl Drop for Mounted {
    fn drop(&mut self) {
        // fuser 的 BackgroundSession 沒有 Drop 實作：欄位 drop 時 Mount 會被
        // 卸載、JoinHandle 直接 detach（event loop 可能還在跑）。runtime **不能**
        // 留在這裡 drop——錯誤路徑上 Mounted 會在 async context 裡被收，tokio 對
        // 「runtime 在 async context 內 drop」直接 panic。丟到獨立執行緒去收；
        // KistFs 裡的 Handle 把 runtime 的 Arc 抓著，不會有 use-after-free。
        if let Some(rt) = self._rt.take() {
            std::thread::spawn(move || drop(rt));
        }
    }
}

/// 把 `repo` 掛到 `mountpoint`（必須已存在、最好是空目錄）。回傳時掛載已生效。
/// **async**：`FsCore::new` 要先載 index——在呼叫端的 runtime 上做（絕不能在
/// 另一個 runtime 的 context 裡 `block_on`，tokio 會直接 panic）。
///
/// fuser event loop 在 Linux 上開 4 條；本函式另建的 tokio runtime worker 開
/// `4 × 2` 條（advisor 的 deadlock 鐵律：block_on park 住的 fuser 執行緒不算
/// 勞動力，worker 必須足夠）。macOS 的 fuser 只支援單一 event loop。
#[cfg(unix)]
pub async fn mount(
    repo: Repository,
    mountpoint: &Path,
    config: MountConfig,
) -> std::io::Result<Mounted> {
    #[cfg(target_os = "linux")]
    let event_loops = 4usize;
    #[cfg(not(target_os = "linux"))]
    let event_loops = 1usize;

    let core = FsCore::new(repo, config)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(event_loops * 2)
        .enable_all()
        .build()?;
    // Config 是 non_exhaustive：只能 Default 再改欄位。
    let mut options = fuser::Config::default();
    options.mount_options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("kist".to_owned()),
        fuser::MountOption::Subtype("kist".to_owned()),
        fuser::MountOption::DefaultPermissions,
    ];
    options.n_threads = Some(event_loops);
    let fs = KistFs {
        core,
        rt: rt.handle().clone(),
    };
    let session = match fuser::Session::new(fs, mountpoint, &options) {
        Ok(session) => session,
        Err(e) => {
            // 錯誤路徑：`?` 會在 async context 裡 drop `rt` → tokio 直接 panic。
            // 先把 runtime 交給背景執行緒收，再回傳錯誤（例如掛載點不存在）。
            rt.shutdown_background();
            return Err(e);
        }
    };
    let session = match session.spawn() {
        Ok(session) => session,
        Err(e) => {
            rt.shutdown_background();
            return Err(e);
        }
    };
    Ok(Mounted {
        session: Some(session),
        dir: mountpoint.to_path_buf(),
        _rt: Some(rt),
    })
}
