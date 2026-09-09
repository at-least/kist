//! webhook 通知：每件工作結束後 POST 一個 JSON。失敗只記警告，不影響工作本身。

use std::collections::HashSet;

use crate::config::NotifySection;
use crate::jobs::JobOutcome;

pub struct Notifier {
    url: String,
    on: HashSet<String>,
    client: reqwest::Client,
}

impl Notifier {
    pub fn new(section: &NotifySection) -> Self {
        // reqwest 的 rustls 不帶 provider；由 kist-backend 安裝（冪等）。本機 repo 不會建 S3 後端，所以這裡也要叫。
        kist_backend::install_tls_provider();
        Self {
            url: section.webhook_url.clone(),
            on: section.on.iter().cloned().collect(),
            client: reqwest::Client::new(),
        }
    }

    pub fn wants(&self, outcome: &JobOutcome) -> bool {
        self.on.contains(outcome.status.name())
    }

    /// 送出通知；`Ok(false)` = 這種結果不用通知。
    pub async fn send(&self, outcome: &JobOutcome) -> Result<bool, String> {
        if !self.wants(outcome) {
            return Ok(false);
        }
        let body = serde_json::json!({
            "event": format!("{}.{}", outcome.job.name(), outcome.status.name()),
            "job": outcome.job,
            "status": outcome.status,
            "repo": outcome.repo,
            "started": outcome.started,
            "duration_secs": outcome.duration_secs,
            "detail": outcome.detail,
            "error": outcome.error,
        });
        let resp = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).map_err(|e| e.to_string())?)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("webhook returned {}", resp.status()));
        }
        Ok(true)
    }
}
