//! 排程器：依設定檔裡各節的 cron 在對的時間跑對的工作，一次只跑一件（backup、forget、prune 串行，
//! 不會跟自己重疊；跟別台機器的重疊由 M3 的 GC 設計負責）。

use std::collections::HashMap;

use time::OffsetDateTime;
use tokio::sync::watch;

use crate::config::Config;
use crate::jobs::{run_job, JobKind, JobOutcome};
use crate::notify::Notifier;
use crate::schedule::Schedule;
use crate::Result;

pub struct Daemon {
    cfg: Config,
    schedules: HashMap<JobKind, Schedule>,
    notifier: Option<Notifier>,
}

impl Daemon {
    pub fn new(cfg: Config) -> Result<Self> {
        cfg.validate()?;
        let mut schedules = HashMap::new();
        if let Some(b) = &cfg.backup {
            if let Some(s) = cfg.schedule_of(b.schedule.as_deref(), "[backup]")? {
                schedules.insert(JobKind::Backup, s);
            }
        }
        if let Some(f) = &cfg.forget {
            if let Some(s) = cfg.schedule_of(f.schedule.as_deref(), "[forget]")? {
                schedules.insert(JobKind::Forget, s);
            }
        }
        if let Some(p) = &cfg.prune {
            if let Some(s) = cfg.schedule_of(p.schedule.as_deref(), "[prune]")? {
                schedules.insert(JobKind::Prune, s);
            }
        }
        let notifier = cfg.notify.as_ref().map(Notifier::new);
        Ok(Self {
            cfg,
            schedules,
            notifier,
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// 有排程的工作與各自的下一次時間。
    pub fn next_runs(&self, after: OffsetDateTime) -> Vec<(JobKind, Option<OffsetDateTime>)> {
        let mut v: Vec<_> = self
            .schedules
            .iter()
            .map(|(k, s)| (*k, s.next_after(after)))
            .collect();
        v.sort_by_key(|(k, _)| k.name());
        v
    }

    /// 設定裡有的工作各跑一次（backup → forget → prune），不看排程。給 `run --once` 與外部 cron 用。
    pub async fn run_once(&self, mut on_outcome: impl FnMut(&JobOutcome)) -> Vec<JobOutcome> {
        let mut out = Vec::new();
        for kind in [JobKind::Backup, JobKind::Forget, JobKind::Prune] {
            if !self.has(kind) {
                continue;
            }
            let o = self.execute(kind).await;
            on_outcome(&o);
            out.push(o);
        }
        out
    }

    /// 常駐：直到 `shutdown` 變成 true。每件工作結束呼叫 `on_outcome`。
    pub async fn run(
        &self,
        mut shutdown: watch::Receiver<bool>,
        mut on_outcome: impl FnMut(&JobOutcome),
    ) -> Result<()> {
        if self.schedules.is_empty() {
            return Err(crate::AppError::Config(
                "no section has a schedule; use `run --once` or add schedule = \"...\"".to_owned(),
            ));
        }
        loop {
            let now = OffsetDateTime::now_utc();
            // 下一件：時間最早的；同一時間依 backup → forget → prune
            let mut due: Vec<(OffsetDateTime, JobKind)> = self
                .schedules
                .iter()
                .filter_map(|(k, s)| s.next_after(now).map(|t| (t, *k)))
                .collect();
            due.sort_by_key(|(t, k)| (*t, order(*k)));
            let Some((at, kind)) = due.first().copied() else {
                return Err(crate::AppError::Config(
                    "no schedule has a next occurrence".to_owned(),
                ));
            };
            let wait = (at - now).max(time::Duration::ZERO);
            tracing::info!("next job: {} at {at} (in {wait})", kind.name());
            let sleep = tokio::time::sleep(std::time::Duration::try_from(wait).unwrap_or_default());
            tokio::select! {
                _ = sleep => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return Ok(()); }
                }
            }
            if *shutdown.borrow() {
                return Ok(());
            }
            // 同一秒到期的其他工作也一起跑（依序）
            let mut batch: Vec<JobKind> = due
                .iter()
                .filter(|(t, _)| *t == at)
                .map(|(_, k)| *k)
                .collect();
            batch.sort_by_key(|k| order(*k));
            for kind in batch {
                let o = self.execute(kind).await;
                on_outcome(&o);
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }

    fn has(&self, kind: JobKind) -> bool {
        match kind {
            JobKind::Backup => self.cfg.backup.is_some(),
            JobKind::Forget => self.cfg.forget.is_some(),
            JobKind::Prune => self.cfg.prune.is_some(),
        }
    }

    async fn execute(&self, kind: JobKind) -> JobOutcome {
        tracing::info!("{}: starting", kind.name());
        let o = run_job(&self.cfg, kind).await;
        match &o.error {
            Some(e) => tracing::error!("{}: {}: {e}", kind.name(), o.status.name()),
            None => tracing::info!(
                "{}: {} in {:.1}s",
                kind.name(),
                o.status.name(),
                o.duration_secs
            ),
        }
        if let Some(n) = &self.notifier {
            match n.send(&o).await {
                Ok(true) => tracing::info!("{}: webhook sent", kind.name()),
                Ok(false) => {}
                Err(e) => tracing::warn!("{}: webhook failed: {e}", kind.name()),
            }
        }
        o
    }
}

fn order(k: JobKind) -> u8 {
    match k {
        JobKind::Backup => 0,
        JobKind::Forget => 1,
        JobKind::Prune => 2,
    }
}
