//! 「上次成功時間」的持久化：一個 repo + 一種工作一個小檔，內容只有一個 Unix 秒數。
//!
//! 沒有它，daemon 重啟後 `job_last_success_timestamp_seconds` 會消失，
//! 「多久沒成功」的告警就失去判準。放 cache_dir；檔案掉了或太舊只是退回
//! 「重啟歸零」的行為，不值得 fsync。同一個 cache 目錄可能被多個設定共用
//! （同一台機器備份兩個 repo），所以檔名帶 repo 的 hash，寫入是單值覆蓋、
//! 沒有 read-modify-write，兩個 process 同時寫也只是後寫的贏（時間較新）。

use std::path::{Path, PathBuf};

use crate::jobs::JobKind;

/// FNV-1a：跨版本穩定的簡單 hash，檔名只用，不是安全機制。
fn repo_hash(repo: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in repo.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 這個 repo + 工作的狀態檔位置。
pub fn path(cache_dir: &Path, repo: &str, job: JobKind) -> PathBuf {
    cache_dir.join(format!(
        "jobstate-{:016x}-{}.json",
        repo_hash(repo),
        job.name()
    ))
}

/// 讀回上次成功的時間；檔案不存在或看不懂就 `None`（視同沒有資料）。
pub fn read(cache_dir: &Path, repo: &str, job: JobKind) -> Option<f64> {
    let text = std::fs::read_to_string(path(cache_dir, repo, job)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 記下上次成功的時間（暫存檔 + rename；暫存檔帶 pid，兩個 process 不會撞名）。
/// 失敗只記警告：metrics 退回不持久化的行為，不值得讓工作失敗。
pub fn write_last_success(cache_dir: &Path, repo: &str, job: JobKind, at: f64) {
    let text = match serde_json::to_string(&at) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("cannot encode job state: {e}");
            return;
        }
    };
    let target = path(cache_dir, repo, job);
    let tmp = target.with_extension(format!("json.{}.tmp", std::process::id()));
    let write = std::fs::create_dir_all(cache_dir)
        .and_then(|_| std::fs::write(&tmp, text))
        .and_then(|_| std::fs::rename(&tmp, &target));
    if let Err(e) = write {
        tracing::warn!("cannot persist job state to {}: {e}", target.display());
    }
}
