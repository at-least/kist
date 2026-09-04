//! 設定檔、排程、daemon（run --once 與秒級 cron）、webhook。

use std::path::Path;
use std::sync::{Arc, Mutex};

use kist_app::{Config, Daemon, JobKind, JobStatus};
use kist_core::{InitOptions, Repository};
use time::macros::datetime;

const PASSWORD: &str = "app test password";

fn write_password(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("password");
    std::fs::write(&p, format!("{PASSWORD}\n")).unwrap();
    p
}

async fn init_repo(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    let backend = kist_backend::Backend::local(&repo).unwrap();
    let opts = InitOptions {
        kdf_cost: kist_crypto::KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
        ..InitOptions::default()
    };
    Repository::init(backend, PASSWORD.as_bytes(), opts)
        .await
        .unwrap();
    repo
}

fn config_text(repo: &Path, password: &Path, dir: &Path, extra: &str) -> String {
    format!(
        r#"
repo = {repo:?}
password_file = {password:?}
client_id_file = {cid:?}
cache_dir = {cache:?}
timezone = "utc"
{extra}
"#,
        repo = repo.display().to_string(),
        password = password.display().to_string(),
        cid = dir.join("client-id").display().to_string(),
        cache = dir.join("cache").display().to_string(),
    )
}

#[test]
fn config_parses_and_validates() {
    let good = r#"
repo = "/tmp/x"
password_file = "/tmp/p"
[backup]
paths = ["/etc"]
schedule = "0 2 * * *"
gc_grace = "48h"
[forget]
keep_daily = 7
keep_within = "2w"
schedule = "0 4 * * *"
[prune]
grace = "3d"
repack_below = 40
[notify]
webhook_url = "https://example.com/hook"
on = ["failure", "success"]
"#;
    let cfg = Config::parse(good).unwrap();
    cfg.validate().unwrap();
    assert_eq!(
        cfg.backup.as_ref().unwrap().gc_grace,
        Some(std::time::Duration::from_secs(48 * 3600))
    );
    assert_eq!(
        cfg.forget.as_ref().unwrap().policy().keep_within,
        Some(std::time::Duration::from_secs(14 * 86_400))
    );
    assert_eq!(
        cfg.prune.as_ref().unwrap().options().grace,
        std::time::Duration::from_secs(3 * 86_400)
    );
    assert_eq!(
        cfg.prune.as_ref().unwrap().options().repack_below_percent,
        40
    );

    let bad = [
        ("repo = \"/x\"\npassword_file = \"/p\"\n", "nothing to do"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = []\n", "paths must not be empty"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nschedule = \"every day\"\n", "schedule"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[forget]\nschedule = \"0 4 * * *\"\n", "keep_*"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[forget]\nkeep_last = 0\n", "refusing"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[prune]\nrepack_below = 101\n", "0..=100"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n[notify]\nwebhook_url = \"ftp://x\"\n", "http"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n[notify]\nwebhook_url = \"https://x\"\non = [\"sometimes\"]\n", "unknown event"),
        ("repo = \"/x\"\npassword_file = \"/p\"\npassword = \"inline\"\n[backup]\npaths = [\"/e\"]\n", "unknown field"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\ngc_grace = \"3 days\"\n", "unknown unit"),
    ];
    for (text, needle) in bad {
        let err = Config::parse(text)
            .and_then(|c| c.validate())
            .expect_err(text);
        assert!(err.to_string().contains(needle), "{text}\n→ {err}");
    }
}

#[test]
fn schedule_next_occurrence() {
    use kist_app::schedule::{Schedule, Timezone};
    let s = Schedule::parse("0 2 * * *", Timezone::Utc).unwrap();
    let next = s.next_after(datetime!(2026-09-05 10:00:00 UTC)).unwrap();
    assert_eq!(next, datetime!(2026-09-06 02:00:00 UTC));
    // 6 欄：秒
    let s = Schedule::parse("*/15 * * * * *", Timezone::Utc).unwrap();
    let next = s.next_after(datetime!(2026-09-05 10:00:07 UTC)).unwrap();
    assert_eq!(next, datetime!(2026-09-05 10:00:15 UTC));
    // 嚴格晚於
    let next = s.next_after(datetime!(2026-09-05 10:00:15 UTC)).unwrap();
    assert_eq!(next, datetime!(2026-09-05 10:00:30 UTC));
    assert!(Schedule::parse("0 2 * *", Timezone::Utc).is_err());
}

#[tokio::test]
async fn run_once_executes_backup_forget_prune_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path()).await;
    let password = write_password(dir.path());
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"hello").unwrap();
    let text = config_text(
        &repo,
        &password,
        dir.path(),
        &format!(
            "[backup]\npaths = [{:?}]\n[forget]\nkeep_last = 1\n[prune]\ngrace = \"0s\"\n",
            src.display().to_string()
        ),
    );
    let cfg = Config::parse(&text).unwrap();
    let daemon = Daemon::new(cfg).unwrap();
    let mut seen = Vec::new();
    let outcomes = daemon.run_once(|o| seen.push(o.job)).await;
    assert_eq!(seen, [JobKind::Backup, JobKind::Forget, JobKind::Prune]);
    for o in &outcomes {
        assert_eq!(o.status, JobStatus::Success, "{o:?}");
    }
    assert!(
        outcomes[0].detail["stats"]["files"].as_u64() == Some(1),
        "{:?}",
        outcomes[0].detail
    );
    // 第二次：forget --keep-last 1 會刪掉舊的那個
    let outcomes = daemon.run_once(|_| {}).await;
    assert_eq!(
        outcomes[1].detail["removed"].as_array().map(|a| a.len()),
        Some(1)
    );
}

#[tokio::test]
async fn failure_is_reported_not_raised() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path()).await;
    let password = dir.path().join("password");
    std::fs::write(&password, "wrong\n").unwrap();
    let text = config_text(
        &repo,
        &password,
        dir.path(),
        "[backup]\npaths = [\"/nonexistent\"]\n",
    );
    let daemon = Daemon::new(Config::parse(&text).unwrap()).unwrap();
    let outcomes = daemon.run_once(|_| {}).await;
    assert_eq!(outcomes[0].status, JobStatus::Failure);
    assert!(
        outcomes[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("password"),
        "{:?}",
        outcomes[0].error
    );
}

/// 秒級 cron：跑到兩次 backup 就關掉；webhook 收到每一次的 JSON。
#[tokio::test]
async fn scheduled_runs_and_webhook() {
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::clone(&received);
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let store = Arc::clone(&store);
            async move {
                store.lock().unwrap().push(v);
                "ok"
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path()).await;
    let password = write_password(dir.path());
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"hello").unwrap();
    let text = config_text(
        &repo,
        &password,
        dir.path(),
        &format!(
            "[backup]\npaths = [{:?}]\nschedule = \"* * * * * *\"\n[notify]\nwebhook_url = \"http://{addr}/hook\"\non = [\"success\", \"failure\"]\n",
            src.display().to_string()
        ),
    );
    let daemon = Daemon::new(Config::parse(&text).unwrap()).unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let mut n = 0;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        daemon.run(rx, |o| {
            assert_eq!(o.status, JobStatus::Success, "{o:?}");
            n += 1;
            if n == 2 {
                let _ = tx.send(true);
            }
        }),
    )
    .await
    .expect("daemon did not stop in time");
    result.unwrap();
    assert_eq!(n, 2);
    // webhook 是在工作結束後送的；給它一點時間
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0]["event"], "backup.success");
    assert_eq!(got[0]["job"], "backup");
    assert!(got[0]["detail"]["snapshot_key"]
        .as_str()
        .unwrap()
        .starts_with("snapshots/"));
}
