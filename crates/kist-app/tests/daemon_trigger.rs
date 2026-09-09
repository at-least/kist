//! daemon 的手動觸發（Web UI 的「Run backup now」走這條路）：沒有排程也能常駐、
//! 佇列容量 1、狀態（running / queued / history）更新、關機後觸發回 Stopped。

use std::path::Path;

use kist_app::{Config, Daemon, JobKind, JobStatus, TriggerError};
use kist_core::{InitOptions, Repository};

const PASSWORD: &str = "trigger test password";

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

/// 沒有排程的 daemon：閒置等觸發；觸發一次 backup → history 有一筆成功、running 清空；
/// 佇列滿了回 Queued；沒設定的工作回 NotConfigured；關機後回 Stopped；run 兩次是錯誤。
#[tokio::test]
async fn trigger_runs_backup_without_schedules() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let password = write_password(dir.path());
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("b.txt"), b"world").unwrap();
        let text = config_text(
            &repo,
            &password,
            dir.path(),
            &format!("[backup]\npaths = [{:?}]\n", src.display().to_string()),
        );
        let mut daemon = Daemon::new(Config::parse(&text).unwrap()).unwrap();
        assert!(!daemon.has_schedules());
        let handle = daemon.handle();
        assert!(handle.schedules().is_empty());
        assert!(handle.next_runs(time::OffsetDateTime::now_utc()).is_empty());
        assert_eq!(handle.config().repo, repo.display().to_string());

        // daemon 還沒開始收：第一個觸發進佇列（容量 1），第二個被拒
        handle.trigger(JobKind::Backup).unwrap();
        assert_eq!(handle.state().queued, [JobKind::Backup]);
        assert!(matches!(
            handle.trigger(JobKind::Backup),
            Err(TriggerError::Queued)
        ));
        assert!(matches!(
            handle.trigger(JobKind::Prune),
            Err(TriggerError::NotConfigured("prune"))
        ));

        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let result = daemon.run(rx, |_| {}).await;
            (result, daemon)
        });

        // 等 backup 跑完
        let state = loop {
            let st = handle.state();
            if st.history.len() == 1 {
                break st;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert!(state.running.is_none(), "{state:?}");
        assert!(state.queued.is_empty(), "{state:?}");
        let o = &state.history[0];
        assert_eq!(o.job, JobKind::Backup);
        assert_eq!(o.status, JobStatus::Success, "{o:?}");
        assert_eq!(o.detail["stats"]["files"].as_u64(), Some(2), "{o:?}");
        assert!(matches!(
            handle.trigger(JobKind::Prune),
            Err(TriggerError::NotConfigured(_))
        ));

        // 關機、收尾
        tx.send(true).unwrap();
        let (result, mut daemon) = task.await.unwrap();
        result.unwrap();
        assert!(matches!(
            handle.trigger(JobKind::Backup),
            Err(TriggerError::Stopped)
        ));
        let (_tx2, rx2) = tokio::sync::watch::channel(false);
        let err = daemon.run(rx2, |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");
    })
    .await
    .expect("test timed out");
}

/// 進度 callback：backup 跑的時候 `running.progress` 會被寫；跑完清掉。
/// 用一個較大的來源目錄，讓輪詢有機會看到進行中的狀態；看不到也不算失敗
/// （時序相關），但 history 一定要有結果。
#[tokio::test]
async fn running_job_exposes_progress() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path()).await;
        let password = write_password(dir.path());
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..200 {
            std::fs::write(src.join(format!("f{i}.txt")), vec![b'x'; 4096]).unwrap();
        }
        let text = config_text(
            &repo,
            &password,
            dir.path(),
            &format!("[backup]\npaths = [{:?}]\n", src.display().to_string()),
        );
        let mut daemon = Daemon::new(Config::parse(&text).unwrap()).unwrap();
        let handle = daemon.handle();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { daemon.run(rx, |_| {}).await });
        handle.trigger(JobKind::Backup).unwrap();

        let mut saw_running = false;
        let mut saw_progress = false;
        let state = loop {
            let st = handle.state();
            if let Some(r) = &st.running {
                saw_running = true;
                assert_eq!(r.job, JobKind::Backup);
                assert!(r.started_unix > 1_700_000_000.0, "{r:?}");
                assert!(!r.started.is_empty());
                if let Some(p) = &r.progress {
                    saw_progress = true;
                    assert!(
                        matches!(p.phase, "files" | "flush" | "verify" | "commit"),
                        "{p:?}"
                    );
                }
            }
            if st.history.len() == 1 {
                break st;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        eprintln!("saw_running={saw_running} saw_progress={saw_progress}");
        assert!(state.running.is_none());
        assert_eq!(state.history[0].status, JobStatus::Success);
        assert_eq!(
            state.history[0].detail["stats"]["files"].as_u64(),
            Some(200)
        );
        // JSON 形狀（給 /ui/status 用）
        let json = serde_json::to_value(&state).unwrap();
        assert!(json["running"].is_null());
        assert_eq!(json["history"][0]["job"], "backup");

        tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    })
    .await
    .expect("test timed out");
}
