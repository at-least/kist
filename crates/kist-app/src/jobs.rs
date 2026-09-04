//! 一件工作 = 對設定檔描述的 repo 做一次 backup / forget / prune，回傳結果（給通知、metrics、UI 用）。

use std::path::PathBuf;

use kist_backend::Backend;
use kist_core::{BackupOptions, ForgetOptions, Repository};
use serde::Serialize;
use time::OffsetDateTime;

use crate::config::Config;
use crate::{client_id, AppError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Backup,
    Forget,
    Prune,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            JobKind::Backup => "backup",
            JobKind::Forget => "forget",
            JobKind::Prune => "prune",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Success,
    /// 做完了但有項目被略過（backup 有讀不到的檔）或刪不掉（prune）。
    Incomplete,
    Failure,
}

impl JobStatus {
    pub fn name(&self) -> &'static str {
        match self {
            JobStatus::Success => "success",
            JobStatus::Incomplete => "incomplete",
            JobStatus::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobOutcome {
    pub job: JobKind,
    pub status: JobStatus,
    pub repo: String,
    /// RFC 3339。
    pub started: String,
    pub duration_secs: f64,
    /// 成功時的摘要（backup 的 stats、prune 的 report…），失敗時的錯誤訊息。
    pub detail: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 執行一件工作。永遠回 `Ok(outcome)`：失敗記在 outcome 裡（排程器與通知要看得到）。
pub async fn run_job(cfg: &Config, kind: JobKind) -> JobOutcome {
    let started = OffsetDateTime::now_utc();
    let t0 = std::time::Instant::now();
    let result = run_job_inner(cfg, kind).await;
    let (status, detail, error) = match result {
        Ok((incomplete, detail)) => (
            if incomplete {
                JobStatus::Incomplete
            } else {
                JobStatus::Success
            },
            detail,
            None,
        ),
        Err(e) => (
            JobStatus::Failure,
            serde_json::Value::Null,
            Some(format!("{e:#}")),
        ),
    };
    JobOutcome {
        job: kind,
        status,
        repo: cfg.repo.clone(),
        started: started
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        duration_secs: t0.elapsed().as_secs_f64(),
        detail,
        error,
    }
}

async fn open(cfg: &Config) -> Result<Repository> {
    let backend = Backend::from_url(&cfg.repo)?;
    let password = cfg.read_password()?;
    Ok(Repository::open_with_cache(backend, password.as_bytes(), cfg.cache_dir.clone()).await?)
}

/// 回傳 (是否不完整, 摘要)。
async fn run_job_inner(cfg: &Config, kind: JobKind) -> Result<(bool, serde_json::Value)> {
    match kind {
        JobKind::Backup => {
            let b = cfg
                .backup
                .as_ref()
                .ok_or_else(|| AppError::Config("no [backup] section".to_owned()))?;
            let repo = open(cfg).await?;
            let id = client_id::load_or_create(cfg.client_id_file.as_deref())?;
            let _lock = client_id::lock(cfg.client_id_file.as_deref())?;
            let opts = BackupOptions {
                client_id: id,
                hostname: client_id::hostname(),
                username: client_id::username(),
                now: None,
                gc_grace: b.gc_grace.unwrap_or(kist_core::DEFAULT_GC_GRACE),
            };
            let paths: Vec<PathBuf> = b.paths.clone();
            let summary = repo.backup(&paths, opts).await?;
            let incomplete = summary.stats.errors > 0;
            Ok((
                incomplete,
                serde_json::to_value(&summary).unwrap_or_default(),
            ))
        }
        JobKind::Forget => {
            let f = cfg
                .forget
                .as_ref()
                .ok_or_else(|| AppError::Config("no [forget] section".to_owned()))?;
            let repo = open(cfg).await?;
            let summary = repo
                .forget(ForgetOptions {
                    snapshots: vec![],
                    policy: f.policy(),
                    dry_run: false,
                })
                .await?;
            Ok((false, serde_json::to_value(&summary).unwrap_or_default()))
        }
        JobKind::Prune => {
            let p = cfg
                .prune
                .as_ref()
                .ok_or_else(|| AppError::Config("no [prune] section".to_owned()))?;
            let repo = open(cfg).await?;
            let report = repo.prune(p.options()).await?;
            let incomplete = !report.skipped.is_empty();
            Ok((
                incomplete,
                serde_json::to_value(&report).unwrap_or_default(),
            ))
        }
    }
}
