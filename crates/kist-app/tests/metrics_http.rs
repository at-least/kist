//! metrics 模組與 `serve_http`（/metrics、/healthz）的測試。

use std::sync::Arc;

use kist_app::server::{serve_http, ui_config, ServeState};
use kist_app::{JobKind, JobOutcome, JobStatus, Metrics};
use serde_json::json;
use tokio::sync::watch;

fn outcome(job: JobKind, status: JobStatus, detail: serde_json::Value) -> JobOutcome {
    JobOutcome {
        job,
        status,
        repo: "test-repo".to_owned(),
        started: "2026-09-05T00:00:00Z".to_owned(),
        duration_secs: 1.5,
        detail,
        error: None,
    }
}

#[test]
fn metrics_render_after_jobs() {
    // jobs.rs 真實跑工作時放進 detail 的是 v2 BackupSummary 的序列化
    // （stats 欄位名已是 "bytes"/"bytes_stored"），兩邊對不上——見
    let m = Metrics::new();
    m.record(&outcome(
        JobKind::Backup,
        JobStatus::Success,
        json!({"stats": {"files": 3, "bytes": 100, "bytes_stored": 50, "chunks_new": 2, "errors": 0}}),
    ));
    m.record(&outcome(
        JobKind::Prune,
        JobStatus::Incomplete,
        json!({"deleted_bytes": 1024, "marked": 3, "live_packs": 5}),
    ));
    // 失敗也要算進 runs_total，但不能更新 last_success
    m.record(&outcome(JobKind::Backup, JobStatus::Failure, json!(null)));

    let text = m.render();
    for want in [
        "kist_job_runs_total{job=\"backup\",status=\"success\"} 1",
        "kist_job_runs_total{job=\"backup\",status=\"failure\"} 1",
        "kist_job_runs_total{job=\"prune\",status=\"incomplete\"} 1",
        "kist_job_last_duration_seconds{job=\"backup\"} 1.5",
        "kist_job_last_duration_seconds{job=\"prune\"} 1.5",
        "kist_backup_files 3",
        "kist_backup_bytes 100",
        "kist_backup_bytes_new 50",
        "kist_backup_chunks_new 2",
        "kist_backup_skipped_items 0",
        "kist_prune_deleted_bytes_total 1024",
        "kist_prune_marked_objects 3",
        "kist_prune_live_packs 5",
    ] {
        assert!(
            text.contains(want),
            "metric text should contain {want:?}:\n{text}"
        );
    }
    let line = text
        .lines()
        .find(|l| l.starts_with("kist_job_last_success_timestamp_seconds{job=\"backup\"}"))
        .unwrap_or_else(|| panic!("no last_success line:\n{text}"));
    let value: f64 = line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(
        value > 1_700_000_000.0,
        "last_success should be a recent unix time: {line}"
    );
    assert!(text.ends_with("# EOF\n"), "OpenMetrics 結尾:\n{text}");
}

/// 沒跑過任何工作也要能輸出（註冊過的 metric 全部以 0/預設出現）。
#[test]
fn metrics_render_empty() {
    let text = Metrics::new().render();
    assert!(text.contains("kist_backup_files 0"), "{text}");
    assert!(text.contains("kist_prune_live_packs 0"), "{text}");
    assert!(!text.contains("job_runs_total"), "{text}");
    assert!(
        text.contains("kist_process_start_time_seconds 17"),
        "{text}"
    );
}

/// daemon 啟動時要從 jobstate 檔種回上次成功時間（只對自己的 repo；看不懂的略過）。
#[test]
fn daemon_seeds_last_success_from_jobstate() {
    use kist_app::jobstate;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let repo = dir.path().join("repo").display().to_string();
    jobstate::write_last_success(&cache, &repo, kist_app::JobKind::Backup, 1700000000.0);
    // 別的 repo 的檔案、以及看不懂的內容都不能進到這個 daemon 的 metrics
    std::fs::write(
        jobstate::path(&cache, "other-repo", kist_app::JobKind::Backup),
        "1700000001.0",
    )
    .unwrap();
    std::fs::write(
        jobstate::path(&cache, &repo, kist_app::JobKind::Prune),
        "not json",
    )
    .unwrap();
    let cfg = kist_app::Config::parse(&format!(
        "repo = \"{repo}\"\npassword_file = \"{}\"\ncache_dir = \"{}\"\n\n[backup]\npaths = [\"{}\"]\n",
        dir.path().join("pw").display(),
        cache.display(),
        dir.path().join("src").display(),
    ))
    .unwrap();
    let daemon = kist_app::Daemon::new(cfg).unwrap();
    let text = daemon.metrics().render();
    let line = text
        .lines()
        .find(|l| l.starts_with("kist_job_last_success_timestamp_seconds{job=\"backup\"}"))
        .unwrap_or_else(|| panic!("no seeded last_success line:\n{text}"));
    assert!(line.contains("1700000000"), "{line}");
    assert!(
        !text.contains("job=\"prune\"} 17"),
        "別的 repo 或壞掉的檔案不能被種回:\n{text}"
    );
}

/// 非 async 的 client 會把 #[tokio::test] 預設的單執行緒 runtime 擋死（server task 排不到、
/// read 永遠等不到回應），所以用 tokio 的非同步 TcpStream。
async fn get(addr: std::net::SocketAddr, path: &str, host: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn serve_http_serves_metrics_and_healthz() {
    let metrics = Arc::new(Metrics::new());
    metrics.record(&outcome(
        JobKind::Backup,
        JobStatus::Success,
        json!({"stats": {"files": 7}}),
    ));
    // UI 需要一個 daemon handle；最小設定（沒有 [serve] → 唯讀 UI、不用登入）
    let dir = tempfile::tempdir().unwrap();
    let cfg = kist_app::Config::parse(&format!(
        "repo = \"{}\"\npassword_file = \"{}\"\n\n[backup]\npaths = [\"{}\"]\n",
        dir.path().join("repo").display(),
        dir.path().join("pw").display(),
        dir.path().join("src").display(),
    ))
    .unwrap();
    let daemon = kist_app::Daemon::new(cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ui = ui_config(daemon.config(), addr).unwrap();
    let state = ServeState {
        metrics,
        daemon: daemon.handle(),
        ui: Arc::new(ui),
    };
    let (tx, rx) = watch::channel(false);
    let server = tokio::spawn(serve_http(state, listener, rx));

    // /metrics 與 /healthz 不看 Host（scrape 設定裡的 Host 可能是任何東西）
    let resp = get(addr, "/metrics", "test").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("application/openmetrics-text"), "{resp}");
    assert!(resp.contains("kist_backup_files 7"), "{resp}");
    assert!(get(addr, "/healthz", "test").await.contains("200 OK"));
    // 首頁走 UI 規則：Host 要對；沒設密碼就不用登入
    let root = get(addr, "/", &addr.to_string()).await;
    assert!(root.starts_with("HTTP/1.1 200 OK"), "{root}");
    assert!(root.contains("kist"), "{root}");
    assert!(get(addr, "/", "test").await.contains("421"));

    tx.send(true).unwrap();
    server.await.unwrap().unwrap();
}
