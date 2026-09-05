//! Prometheus metrics：daemon 每件工作結束就更新，`kist serve` 的 `/metrics` 輸出。
//! counter/gauge 只在記憶體（daemon 重啟歸零，Prometheus 端本來就有歷史）；
//! 但 `job_last_success_timestamp_seconds` 會持久化到 `cache_dir/jobstate-<repo hash>-<job>.json`、
//! 啟動時種回——否則「多久沒成功」的告警在 daemon 重啟後就失去判準。監控端請用
//! 「多久沒看到成功」而不是「有沒有成功過」來告警，或搭配 webhook。

use std::sync::atomic::AtomicU64;

use prometheus_client::encoding::{text::encode, EncodeLabelSet};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::jobs::{JobKind, JobOutcome, JobStatus};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct JobLabel {
    job: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct JobStatusLabel {
    job: String,
    status: String,
}

pub struct Metrics {
    registry: Registry,
    runs_total: Family<JobStatusLabel, Counter>,
    last_success_timestamp: Family<JobLabel, Gauge<f64, AtomicU64>>,
    last_duration: Family<JobLabel, Gauge<f64, AtomicU64>>,
    backup_files: Gauge,
    backup_bytes_total: Gauge,
    backup_bytes_new: Gauge,
    backup_chunks_new: Gauge,
    backup_errors: Gauge,
    prune_deleted_bytes_total: Counter,
    prune_marked: Gauge,
    prune_live_packs: Gauge,
}

/// 現在的 Unix 時間（秒）；系統時鐘倒退等異常時給 0。
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("kist");
        let runs_total = Family::<JobStatusLabel, Counter>::default();
        registry.register(
            "job_runs",
            "Number of finished jobs by kind and status",
            runs_total.clone(),
        );
        let last_success_timestamp = Family::<JobLabel, Gauge<f64, AtomicU64>>::default();
        registry.register(
            "job_last_success_timestamp_seconds",
            "Unix time of the last fully successful run of each job (persisted across restarts; incomplete runs do not count)",
            last_success_timestamp.clone(),
        );
        let last_duration = Family::<JobLabel, Gauge<f64, AtomicU64>>::default();
        registry.register(
            "job_last_duration_seconds",
            "Duration of the last run of each job",
            last_duration.clone(),
        );
        let process_start_time = Gauge::<f64, AtomicU64>::default();
        process_start_time.set(unix_now());
        registry.register(
            "process_start_time_seconds",
            "Unix time when this daemon started",
            process_start_time,
        );
        let backup_files = Gauge::default();
        registry.register(
            "backup_files",
            "Files in the last snapshot this daemon completed (0 until the first backup finishes)",
            backup_files.clone(),
        );
        let backup_bytes_total = Gauge::default();
        registry.register(
            "backup_bytes_total",
            "Total file bytes in the last snapshot this daemon completed",
            backup_bytes_total.clone(),
        );
        let backup_bytes_new = Gauge::default();
        registry.register(
            "backup_bytes_new",
            "Bytes uploaded by the last backup this daemon ran",
            backup_bytes_new.clone(),
        );
        let backup_chunks_new = Gauge::default();
        registry.register(
            "backup_chunks_new",
            "Chunks uploaded by the last backup this daemon ran",
            backup_chunks_new.clone(),
        );
        let backup_errors = Gauge::default();
        registry.register(
            "backup_skipped_items",
            "Items the last backup this daemon ran could not read",
            backup_errors.clone(),
        );
        let prune_deleted_bytes_total = Counter::default();
        registry.register(
            "prune_deleted_bytes",
            "Bytes deleted by prune since daemon start",
            prune_deleted_bytes_total.clone(),
        );
        let prune_marked = Gauge::default();
        registry.register(
            "prune_marked_objects",
            "Objects marked for deletion by the last prune this daemon ran",
            prune_marked.clone(),
        );
        let prune_live_packs = Gauge::default();
        registry.register(
            "prune_live_packs",
            "Live packs seen by the last prune this daemon ran",
            prune_live_packs.clone(),
        );
        Self {
            registry,
            runs_total,
            last_success_timestamp,
            last_duration,
            backup_files,
            backup_bytes_total,
            backup_bytes_new,
            backup_chunks_new,
            backup_errors,
            prune_deleted_bytes_total,
            prune_marked,
            prune_live_packs,
        }
    }

    pub fn record(&self, o: &JobOutcome) {
        let job = o.job.name().to_owned();
        self.runs_total
            .get_or_create(&JobStatusLabel {
                job: job.clone(),
                status: o.status.name().to_owned(),
            })
            .inc();
        self.last_duration
            .get_or_create(&JobLabel { job: job.clone() })
            .set(o.duration_secs);
        // 只有全然的 Success 算成功：incomplete（有略過/刪不掉）不更新，
        // 否則「永遠 incomplete」的工作不會觸發「多久沒成功」的告警。
        if o.status == JobStatus::Success {
            self.last_success_timestamp
                .get_or_create(&JobLabel { job })
                .set(unix_now());
        }
        let get = |path: &[&str]| -> Option<i64> {
            let mut v = &o.detail;
            for p in path {
                v = v.get(p)?;
            }
            v.as_u64().map(|n| n as i64)
        };
        match o.job {
            crate::jobs::JobKind::Backup => {
                if let Some(n) = get(&["stats", "files"]) {
                    self.backup_files.set(n);
                }
                if let Some(n) = get(&["stats", "bytes_total"]) {
                    self.backup_bytes_total.set(n);
                }
                if let Some(n) = get(&["stats", "bytes_new"]) {
                    self.backup_bytes_new.set(n);
                }
                if let Some(n) = get(&["stats", "chunks_new"]) {
                    self.backup_chunks_new.set(n);
                }
                if let Some(n) = get(&["stats", "errors"]) {
                    self.backup_errors.set(n);
                }
            }
            crate::jobs::JobKind::Prune => {
                if let Some(n) = get(&["deleted_bytes"]) {
                    self.prune_deleted_bytes_total.inc_by(n as u64);
                }
                if let Some(n) = get(&["marked"]) {
                    self.prune_marked.set(n);
                }
                if let Some(n) = get(&["live_packs"]) {
                    self.prune_live_packs.set(n);
                }
            }
            crate::jobs::JobKind::Forget => {}
        }
    }

    /// daemon 啟動時從 jobstate 檔種回「上次成功」的時間：
    /// 沒有的話，重啟後這個 gauge 會消失，外部告警（多久沒成功）就失去了判準。
    pub fn seed_last_success(&self, job: JobKind, unix_secs: f64) {
        self.last_success_timestamp
            .get_or_create(&JobLabel {
                job: job.name().to_owned(),
            })
            .set(unix_secs);
    }

    /// OpenMetrics 文字格式。
    pub fn render(&self) -> String {
        let mut out = String::new();
        if encode(&mut out, &self.registry).is_err() {
            out.clear();
        }
        out
    }
}
