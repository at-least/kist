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
    pub fn new(section: &NotifySection) -> std::result::Result<Self, String> {
        // reqwest 的 rustls 不帶 provider；由 kist-backend 安裝（冪等）。本機 repo 不會建 S3 後端，所以這裡也要叫。
        kist_backend::install_tls_provider();
        Ok(Self {
            url: section.webhook_url.clone(),
            on: section.on.iter().cloned().collect(),
            // 不跟隨 redirect：端點（或入侵它的人）不能把工作資料轉投到
            // 內網任意 URL；3xx 走底下的非成功狀態回報。
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| e.to_string())?,
        })
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod redirect_tests {
    use super::*;
    use crate::config::NotifySection;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 302 不能跟：webhook 端點（或入侵它的人）不該能把工作資料
    /// （repo 位置、主機名、錯誤訊息）轉投到內網任意 URL。跟隨被關掉後，
    /// 3xx 以非成功狀態回報（send 回 Err 且指名狀態碼）。
    #[tokio::test]
    async fn webhook_does_not_follow_redirects() {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits_in_thread = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = stream.unwrap();
                let _ = hits_in_thread.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf); // 讀掉請求再回
                let _ = s.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/hook\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });

        let section = NotifySection {
            webhook_url: format!("http://{addr}/hook"),
            on: vec!["failure".to_owned()],
        };
        let notifier = Notifier::new(&section).unwrap();
        let outcome = JobOutcome {
            job: crate::jobs::JobKind::Backup,
            status: crate::jobs::JobStatus::Failure,
            repo: "repo".to_owned(),
            started: String::new(),
            duration_secs: 0.0,
            detail: serde_json::Value::Null,
            error: None,
        };
        let err = notifier.send(&outcome).await.unwrap_err();
        assert!(
            err.contains("302"),
            "3xx 要以錯誤回報並指名狀態碼，得到：{err}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "只能打第一個端點一次");
    }
}
