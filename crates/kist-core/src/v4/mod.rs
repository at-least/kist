//! v4（`docs/format-v4.md`）：與 v3 的模組並存。實作第 4 段是 init、open 與 backup（B1–B5），讀取路徑
//! （restore、mount、check）在第 5 段、prune 與維護命令在第 6 段；CLI、kist-app、kist-mount 在 v3 移除時
//! 一起換過來。章節號碼（§n）指 `docs/format-v4.md`。
//!
//! 模組一覽：
//! - [`pack`]：pack 與 mpack 的寫入、讀取一項並驗證、trailer（§6）。
//! - [`repo`]：init（config、金鑰槽、head 0）與 open（§3、§10）。
//! - [`cache`]：本地快取：看過的世代、分片、delta、mpack、驗證過的持有者（§3、B2）。
//! - [`state`]：讀取端的世代：最大世代的 head、map、delta，與一個 ID 的持有者（§10、B1）。
//! - [`read`]：從持有者讀一項（依序嘗試、驗證失敗換下一個）與 metadata 的讀取（§10）。
//! - [`live`]：restore、mount、check 的讀取端：退回較舊的世代、持有者全部失敗時重新載入一次（§10）。
//! - [`backup`]：B1–B5（§11.2）。
//! - [`snapshots`]：列出、選取與讀取 snapshot（主體不行讀 `.r1`，§9）。
//! - [`restore`]：restore 與 dry run 的走訪（§9 的落點）；檔案系統那一層與 v3 共用。
//! - [`check`]：check、`--read-data` 與只在不存在時建立的 `--repair`（§13.1）。
//! - [`maint`]：維護角色的共用步驟（檔案鎖、P0、P1、柵欄、兩個時鐘，§11.1、§11.3）與改 settings（§11.4）。

pub mod backup;
pub mod cache;
pub mod check;
pub mod live;
pub mod maint;
pub mod pack;
pub mod read;
pub mod repo;
pub mod restore;
pub mod snapshots;
pub mod state;

pub use backup::{BackupOptions, BackupReport, BackupSummary, Clock, PreparedBackup, Reading};
pub use live::Live;
pub use maint::{MaintOptions, SettingsChange, SettingsReport};
pub use repo::{InitOptions, Repo};
pub use snapshots::SnapshotInfo;
