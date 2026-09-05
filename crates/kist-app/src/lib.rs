//! kist 的應用層：設定檔、排程、工作執行、通知、（之後）metrics 與 Web UI。
//! `kist-core` 只懂 repo；這裡把「每天幾點備份哪些路徑、結果通知到哪」串起來，
//! CLI 的 `kist run` / `kist serve` 只是薄薄一層。

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod client_id;
pub mod config;
pub mod daemon;
pub mod duration;
pub mod jobs;
pub mod jobstate;
pub mod metrics;
pub mod notify;
pub mod schedule;
pub mod server;

pub use config::Config;
pub use daemon::{Daemon, DaemonHandle, DaemonState, RunningJob, TriggerError};
pub use jobs::{JobKind, JobOutcome, JobStatus};
pub use metrics::Metrics;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("config: {0}")]
    Config(String),
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Core(#[from] kist_core::CoreError),
    #[error(transparent)]
    Backend(#[from] kist_backend::BackendError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
