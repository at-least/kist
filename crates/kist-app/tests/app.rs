//! 設定檔、排程、daemon（run --once 與秒級 cron）、webhook。

use std::path::Path;
use std::sync::{Arc, Mutex};

use kist_app::{Config, Daemon, JobKind, JobStatus};
use kist_core::v4::{InitOptions, Repo};
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
        kdf: kist_crypto::KdfCost {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        },
        ..InitOptions::default()
    };
    Repo::init(backend, PASSWORD.as_bytes(), opts)
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
    // one_file_system 只有 unix 支援；其他平台載入時就報錯（下面的 bad）。
    let good = format!(
        r#"
repo = "/tmp/x"
password_file = "/tmp/p"
[backup]
paths = ["/etc"]
schedule = "0 2 * * *"
exclude = ["/home/*", "!/home/me"]
iexclude = ["*.BAK"]
exclude_if_present = [".nobackup"]
exclude_caches = true
exclude_larger_than = "100M"
one_file_system = {}
[forget]
keep_daily = 7
keep_within = "2w"
schedule = "0 4 * * *"
[prune]
repack_below = 40
[notify]
webhook_url = "https://example.com/hook"
on = ["failure", "success"]
"#,
        cfg!(unix)
    );
    let cfg = Config::parse(&good).unwrap();
    cfg.validate().unwrap();
    let exclusions = cfg.backup.as_ref().unwrap().exclusions().unwrap();
    assert_eq!(exclusions.larger_than, Some(100 << 20));
    assert_eq!(
        exclusions.if_present,
        [
            kist_core::Marker::parse(".nobackup").unwrap(),
            kist_core::Marker::cachedir_tag()
        ]
    );
    assert_eq!(exclusions.one_file_system, cfg!(unix));
    // 大小也可以直接寫 bytes 的整數（審查修正：原本只收字串，整數是載入錯誤）。
    let sized = Config::parse(
        "repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nexclude_larger_than = 1048576\n",
    )
    .unwrap();
    sized.validate().unwrap();
    assert_eq!(
        sized
            .backup
            .as_ref()
            .unwrap()
            .exclusions()
            .unwrap()
            .larger_than,
        Some(1 << 20)
    );
    assert_eq!(exclusions.patterns.len(), 3, "exclude 兩條、iexclude 一條");
    assert_eq!(
        cfg.forget.as_ref().unwrap().policy().keep_within,
        Some(std::time::Duration::from_secs(14 * 86_400))
    );
    assert_eq!(cfg.prune.as_ref().unwrap().options().repack_below, 40);

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
        ("repo = \"/x\"\npassword_file = \"/p\"\n[forget]\nkeep_within = \"3 days\"\n", "unknown unit"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\ngc_grace = \"72h\"\n", "unknown field"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[prune]\ngrace = \"72h\"\n", "unknown field"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nexclude = [\"*.tmp\", \"[bad\"]\n", "[backup] exclude: invalid pattern \"[bad\""),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nexclude_if_present = [\"a/b\"]\n", "[backup] exclude marker \"a/b\""),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nexclude_larger_than = \"1MB\"\n", "unknown unit"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nexclude_larger_than = -1\n", "a size"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\nexclude_larger_than = 1.5\n", "a size"),
        ("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\niexclude = [\"a**\"]\n", "[backup] iexclude"),
    ];
    let mut bad = bad.to_vec();
    if cfg!(not(unix)) {
        // 審查修正：原本載入時照收，每次排程的 backup 才失敗。
        bad.push((
            "repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\none_file_system = true\n",
            "not supported on this platform",
        ));
    }
    for (text, needle) in bad {
        let err = Config::parse(text)
            .and_then(|c| c.validate())
            .expect_err(text);
        assert!(err.to_string().contains(needle), "{text}\n→ {err}");
    }
}

/// `[serve]`：選填的 UI 密碼檔與額外允許的 Host；沒有排程也算合法設定。
#[test]
fn serve_section_parses_and_reads_ui_password() {
    let dir = tempfile::tempdir().unwrap();
    let ui_pw = dir.path().join("ui-pw");
    std::fs::write(&ui_pw, "ui secret\r\nsecond line\n").unwrap();
    let text = format!(
        "repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n[serve]\npassword_file = {:?}\nallowed_hosts = [\"backup.example.internal:9898\"]\n",
        ui_pw.display().to_string()
    );
    let cfg = Config::parse(&text).unwrap();
    cfg.validate().unwrap();
    let serve = cfg.serve.as_ref().unwrap();
    assert_eq!(serve.password_file.as_deref(), Some(ui_pw.as_path()));
    assert_eq!(serve.allowed_hosts, ["backup.example.internal:9898"]);
    // 第一行、去掉 \r
    let pw = cfg.read_ui_password().unwrap().unwrap();
    assert_eq!(pw.as_str(), "ui secret");

    // 空的 [serve]：兩個欄位都選填
    let cfg = Config::parse(
        "repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n[serve]\n",
    )
    .unwrap();
    cfg.validate().unwrap();
    let serve = cfg.serve.as_ref().unwrap();
    assert!(serve.password_file.is_none());
    assert!(serve.allowed_hosts.is_empty());
    assert!(cfg.read_ui_password().unwrap().is_none());

    // 沒有 [serve]：沒有 UI 密碼
    let cfg = Config::parse("repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n")
        .unwrap();
    assert!(cfg.serve.is_none());
    assert!(cfg.read_ui_password().unwrap().is_none());

    // 不認識的欄位要擋
    let err = Config::parse(
        "repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n[serve]\nport = 1\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");

    // 空密碼檔是設定錯誤
    let empty = dir.path().join("empty");
    std::fs::write(&empty, "\n").unwrap();
    let cfg = Config::parse(&format!(
        "repo = \"/x\"\npassword_file = \"/p\"\n[backup]\npaths = [\"/e\"]\n[serve]\npassword_file = {:?}\n",
        empty.display().to_string()
    ))
    .unwrap();
    let err = cfg.read_ui_password().unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
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
            "[backup]\npaths = [{:?}]\n[forget]\nkeep_last = 1\n[prune]\n",
            src.display().to_string()
        ),
    );
    let cfg = Config::parse(&text).unwrap();
    let daemon = Daemon::new(cfg).unwrap();
    let mut seen = Vec::new();
    let outcomes = daemon.run_once(None, |o| seen.push(o.job)).await;
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
    let outcomes = daemon.run_once(None, |_| {}).await;
    assert_eq!(
        outcomes[1].detail["removed"].as_array().map(|a| a.len()),
        Some(1)
    );
}

/// `[backup] exclude` 與 CLI 的 `--exclude` 同一套樣式（ADR 020）：比對到的不進 snapshot，
/// 計數出現在工作結果的 report 裡。
#[tokio::test]
async fn run_once_backup_honours_exclude() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path()).await;
    let password = write_password(dir.path());
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("cache")).unwrap();
    std::fs::write(src.join("a.txt"), b"keep").unwrap();
    std::fs::write(src.join("b.tmp"), b"drop").unwrap();
    std::fs::write(src.join("cache/c.txt"), b"drop").unwrap();
    let text = config_text(
        &repo,
        &password,
        dir.path(),
        &format!(
            "[backup]\npaths = [{:?}]\nexclude = [\"*.tmp\", \"cache\"]\n",
            src.display().to_string()
        ),
    );
    let cfg = Config::parse(&text).unwrap();
    cfg.validate().unwrap();
    let outcomes = Daemon::new(cfg).unwrap().run_once(None, |_| {}).await;
    assert_eq!(outcomes[0].status, JobStatus::Success, "{:?}", outcomes[0]);
    assert_eq!(outcomes[0].detail["stats"]["files"].as_u64(), Some(1));
    assert_eq!(outcomes[0].detail["report"]["excluded"].as_u64(), Some(2));
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
    let outcomes = daemon.run_once(None, |_| {}).await;
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
    let mut daemon = Daemon::new(Config::parse(&text).unwrap()).unwrap();
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
    assert!(got[0]["detail"]["snapshot"]
        .as_str()
        .unwrap()
        .starts_with("snapshots/"));
}

// 收到 shutdown 訊號後，剩餘的工作不再開跑（與 `run` 的「停在本輪工作
// 後」同一約定）；已跑完的 outcome 照常回報。
#[tokio::test]
async fn run_once_skips_remaining_jobs_after_shutdown() {
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
            "[backup]\npaths = [{:?}]\nschedule = \"@daily\"\n[forget]\nschedule = \"@daily\"\nkeep_last = 1\n[prune]\nschedule = \"@daily\"\n",
            src.display().to_string()
        ),
    );
    let cfg = Config::parse(&text).unwrap();
    let daemon = Daemon::new(cfg).unwrap();

    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).unwrap(); // 訊號先於 run_once：一件都不該開跑
    let mut seen = Vec::new();
    let outcomes = daemon.run_once(Some(rx), |o| seen.push(o.job)).await;
    assert!(outcomes.is_empty(), "shutdown 後不得再開工作：{outcomes:?}");
    assert!(seen.is_empty());
}
