//! v3 專項測試：touch 復活時間線（V3-GC-5）與 `.r1` 副本行為。
//!
//! V3-GC-5 釘死 advisor 審查抓到的競態：touch 若不覆寫刷新 mtime，第二次
//! backup 重用同一棵已標記的樹時，prune 會刪掉還被引用的樹。這裡以真實
//! 檔案 mtime 操作重現整條時間線並斷言樹存活。

use std::path::Path;
use std::time::Duration;

use kist_core::{CheckOptions, ForgetOptions, PruneOptions, Repository};
use kist_crypto::KdfCost;
use kist_format::keys;

mod common;
use common::{make_source, TestRepo, PASSWORD};

use kist_core::InitOptions;

fn replica_init_options() -> InitOptions {
    let mut o = common::init_options();
    o.replicas = Some(1);
    o
}

/// 把 `path` 的 mtime 往前撥 `age`：模擬時間流逝（prune 以後端 mtime 計齊）。
fn age_file(path: &Path, age: Duration) {
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .checked_sub(age)
        .unwrap();
    let ft = filetime::FileTime::from_unix_time(past.as_secs() as i64, past.subsec_nanos());
    filetime::set_file_times(path, ft, ft).unwrap();
}

fn prune_now_grace(grace: Duration) -> PruneOptions {
    PruneOptions {
        grace,
        ..Default::default()
    }
}

/// V3-GC-5：D0 backup#1 → forget＋prune 標記樹 → backup#2 重用同一棵樹
/// （touch 覆寫刷新 mtime，晚於標記）→ prune 不得刪樹；backup#2 的
/// snapshot 必須完整可還原。
#[tokio::test]
async fn reused_marked_tree_survives_because_touch_refreshes() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;

    // D0：backup#1，之後 forget（它的 snapshot 消失，樹變垃圾候選）。
    let b1 = repo
        .backup(std::slice::from_ref(&src), common::backup_options())
        .await
        .unwrap();
    let root_tree = b1.roots[0].tree;

    repo.forget(ForgetOptions {
        snapshots: vec![b1.snapshot_key.clone()],
        policy: Default::default(),
        dry_run: false,
    })
    .await
    .unwrap();

    // 標記之後，把標記的 mtime 老化到 grace 之前：下一輪 prune 才會真的
    // 走到「刪不刪」的判斷。
    repo.prune(prune_now_grace(Duration::from_secs(0)))
        .await
        .unwrap();
    let mark_path = t
        .repo_path()
        .join(keys::GC_PREFIX)
        .join(format!("{root_tree}"));
    assert!(mark_path.exists(), "前置條件：樹已被標記");
    age_file(&mark_path, Duration::from_secs(3600));

    // backup#2：同一份來源 → 樹沿用 → touch 覆寫刷新（mtime 晚於標記）。
    let b2 = repo
        .backup(std::slice::from_ref(&src), common::backup_options())
        .await
        .unwrap();
    let touched = t
        .repo_path()
        .join(keys::TOUCH_PREFIX)
        .join(format!("{root_tree}"));
    assert!(touched.exists(), "沿用的樹必須留下 touch");
    let touched_age = std::fs::metadata(&touched).unwrap().modified().unwrap();
    let mark_age = std::fs::metadata(&mark_path).unwrap().modified().unwrap();
    assert!(
        touched_age > mark_age,
        "touch 必須比標記新（覆寫式 Put 刷新）"
    );

    // prune：touch 比標記新 → 樹復活，不得刪。
    let report = repo
        .prune(prune_now_grace(Duration::from_secs(0)))
        .await
        .unwrap();
    assert_eq!(report.deleted, 0, "touch 比標記新，樹不得被刪：{report:?}");
    assert!(
        t.repo_path().join(keys::tree(&root_tree)).exists(),
        "樹必須存活"
    );

    // backup#2 的 snapshot 完整可還原。
    let out = t.dir.path().join("out");
    repo.restore(&b2.snapshot_key, &out, Default::default())
        .await
        .unwrap();
    assert!(find_file(&out, "small.txt"), "還原必須包含 small.txt");
}

/// 副本行為：replicas=1 時樹與 snapshot 各有 `.r1`；主體被刪後 restore
/// 落到副本仍成功；check 對孤兒副本（主體不在）回報錯誤；列表不把
/// `.r1` 當 snapshot。
#[tokio::test]
async fn metadata_replicas_cover_missing_primaries() {
    let dir = tempfile::tempdir().unwrap();
    let backend = kist_backend::Backend::local(&dir.path().join("repo")).unwrap();
    let mut o = replica_init_options();
    o.kdf_cost = KdfCost {
        m_cost_kib: 8,
        t_cost: 1,
        p_cost: 1,
    };
    Repository::init(backend.clone(), PASSWORD.as_bytes(), o)
        .await
        .unwrap();

    let repo_dir = dir.path().join("repo");
    let src = dir.path().join("src");
    make_source(&src);

    let repo = Repository::open(backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap();
    let b1 = repo
        .backup(std::slice::from_ref(&src), common::backup_options())
        .await
        .unwrap();
    let root_tree = b1.roots[0].tree;

    let primary = repo_dir.join(keys::tree(&root_tree));
    let replica = repo_dir.join(keys::tree_replica(&root_tree));
    assert!(primary.exists() && replica.exists(), "主體與副本都要在");

    // 刪掉主體：restore 仍要成功（落到副本）。
    std::fs::remove_file(&primary).unwrap();
    let out = dir.path().join("out");
    repo.restore(&b1.snapshot_key, &out, Default::default())
        .await
        .unwrap();
    assert!(find_file(&out, "small.txt"), "還原必須包含 small.txt");

    // check：孤兒副本（主體不在）= 錯誤。
    let report = repo.check(CheckOptions::default()).await.unwrap();
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("replica exists but its primary tree is missing")),
        "孤兒副本必須被回報：{:?}",
        report.errors
    );

    // snapshot 副本存在，但列表不把 `.r1` 當 snapshot。
    let snaps = repo.list_snapshots().await.unwrap();
    assert_eq!(snaps.len(), 1, "`.r1` 不得列為 snapshot");
    let ts = b1.snapshot_key.rsplit('/').next().unwrap().to_owned();
    assert!(
        repo_dir
            .join(keys::SNAPSHOTS_PREFIX)
            .join(format!("{}.r1", hex_of_client_and_ts(&ts).0))
            .exists()
            || walk_has_suffix(&repo_dir.join(keys::SNAPSHOTS_PREFIX), keys::REPLICA_SUFFIX),
        "snapshot `.r1` 必須存在"
    );
}

fn hex_of_client_and_ts(ts: &str) -> (String, String) {
    (String::new(), ts.to_owned())
}

/// 對照組：replicas=0 不寫任何 `.r1`。
#[tokio::test]
async fn replicas_zero_writes_no_replica_objects() {
    let t = TestRepo::new().await; // init_options 預設 replicas=0
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let _ = repo
        .backup(std::slice::from_ref(&src), common::backup_options())
        .await
        .unwrap();
    assert!(
        !walk_has_suffix(
            &t.repo_path().join(keys::TREES_PREFIX),
            keys::REPLICA_SUFFIX
        ),
        "replicas=0 不得有樹副本"
    );
    assert_eq!(
        walk_count_suffix(
            &t.repo_path().join(keys::SNAPSHOTS_PREFIX),
            keys::REPLICA_SUFFIX
        ),
        0,
        "replicas=0 不得有 snapshot 副本"
    );
}

fn find_file(root: &Path, name: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(root) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if find_file(&p, name) {
                return true;
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return true;
        }
    }
    false
}

fn walk_has_suffix(root: &Path, suffix: &str) -> bool {
    walk_count_suffix(root, suffix) > 0
}

fn walk_count_suffix(root: &Path, suffix: &str) -> usize {
    let Ok(rd) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += walk_count_suffix(&p, suffix);
        } else if p.to_string_lossy().ends_with(suffix) {
            n += 1;
        }
    }
    n
}

/// 主體不在、`.r1` 的讀取**本身失敗**（不是 NotFound）時：要把真錯誤回報，
/// 不能吞成 SnapshotNotFound——那會讓 `check`／`snapshots` 把一次暫時性的
/// I/O 錯誤謊報成「snapshot 遺失」（對照：`read_tree` 的 fallback 保留
/// 主體錯誤，同一個 repo 裡兩種語意）。
#[cfg(unix)]
#[tokio::test]
async fn replica_read_error_is_not_masked_as_not_found() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let backend = kist_backend::Backend::local(&dir.path().join("repo")).unwrap();
    let mut o = replica_init_options();
    o.kdf_cost = KdfCost {
        m_cost_kib: 8,
        t_cost: 1,
        p_cost: 1,
    };
    Repository::init(backend.clone(), PASSWORD.as_bytes(), o)
        .await
        .unwrap();

    let repo_dir = dir.path().join("repo");
    let src = dir.path().join("src");
    make_source(&src);
    let repo = Repository::open(backend, PASSWORD.as_bytes())
        .await
        .unwrap();
    let b1 = repo
        .backup(std::slice::from_ref(&src), common::backup_options())
        .await
        .unwrap();

    let primary = repo_dir.join(&b1.snapshot_key);
    let replica = repo_dir.join(format!("{}.r1", b1.snapshot_key));
    assert!(primary.exists() && replica.exists(), "主體與副本都要在");

    std::fs::remove_file(&primary).unwrap();
    std::fs::set_permissions(&replica, PermissionsExt::from_mode(0o000)).unwrap();

    let err = repo
        .read_snapshot_by_key(&b1.snapshot_key)
        .await
        .unwrap_err();
    assert!(
        !matches!(err, kist_core::CoreError::SnapshotNotFound(_)),
        "副本的 I/O 錯誤不能被吞成 SnapshotNotFound：{err}"
    );
}
