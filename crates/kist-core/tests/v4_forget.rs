//! v4 的 forget（format-v4.md §9）：分組、每一組最新的那一份不刪、明確指定（含只有 `.r1` 與解不開的）、
//! 刪除順序。

mod v4_common;

use kist_core::retention::RetentionPolicy;
use kist_core::v4::{ForgetOptions, Repo};
use kist_core::CoreError;
use kist_format::v4::ids::ClientId;
use kist_format::v4::keys::Key;
use kist_format::v4::meta::{meta_kind, node_type, Item, Seg};
use kist_format::v4::snapshot::Root;
use v4_common::*;

const DAY: i64 = 24 * 3_600_000_000_000;

fn file_root(path: &str) -> Root {
    let name = path.rsplit('/').next().unwrap();
    Root {
        path: path.as_bytes().to_vec(),
        t: node_type::FILE,
        tree: None,
        entry: Some(entry(name.as_bytes(), node_type::FILE, meta_kind::GENERIC)),
    }
}

/// 同一個路徑的目錄 root（空目錄）。
fn dir_root(repo: &Repo, path: &str) -> (Root, Item) {
    let seg = Item::Seg(Seg::new(vec![]));
    let id = repo.keys().meta_id(&seg.encode().unwrap());
    (
        Root {
            path: path.as_bytes().to_vec(),
            t: node_type::DIR,
            tree: Some(id),
            entry: None,
        },
        seg,
    )
}

/// client `c` 在 `day` 天前（相對於一個固定的起點）對 `paths` 的 snapshot。
async fn snap(repo: &Repo, c: u8, day: i64, paths: &[&str]) -> String {
    let at = hours_from_now(-24 * 400) + day * DAY;
    let roots = paths.iter().map(|p| file_root(p)).collect();
    hand_snapshot(repo, client(c), at, roots, vec![])
        .await
        .path()
}

async fn setup() -> (HookedRepo, Repo) {
    let t = HookedRepo::new(init_options(1)).await;
    let repo = t.open_as("forget").await;
    (t, repo)
}

fn keep_last(n: u32) -> RetentionPolicy {
    RetentionPolicy {
        keep_last: Some(n),
        ..Default::default()
    }
}

async fn listed(t: &HookedRepo) -> Vec<String> {
    t.keys("snapshots").await
}

#[tokio::test]
async fn the_policy_applies_per_client_and_roots_and_keeps_each_newest() {
    let (t, repo) = setup().await;
    let a1 = snap(&repo, 1, 1, &["/a"]).await;
    let a2 = snap(&repo, 1, 2, &["/a"]).await;
    let a3 = snap(&repo, 1, 3, &["/a"]).await;
    let b1 = snap(&repo, 1, 1, &["/b"]).await;
    let b2 = snap(&repo, 1, 2, &["/b"]).await;
    let c1 = snap(&repo, 2, 1, &["/a"]).await;
    let s = repo
        .forget(ForgetOptions {
            policy: keep_last(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.removed, vec![a1.clone(), b1.clone(), a2.clone()]);
    let kept: Vec<&str> = s.kept.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kept.len(), 3);
    for k in [&a3, &b2, &c1] {
        assert!(kept.contains(&k.as_str()), "{k} kept");
    }
    let left = listed(&t).await;
    assert_eq!(left.len(), 3);
    assert!(!left.contains(&a1) && !left.contains(&a2) && !left.contains(&b1));
}

/// 同一組路徑、不同的 `t` 是不同的組（B1 找 parent 的條件）。
#[tokio::test]
async fn the_same_path_as_a_file_and_as_a_directory_are_different_groups() {
    let (_t, repo) = setup().await;
    let f = snap(&repo, 1, 1, &["/x"]).await;
    let (root, seg) = dir_root(&repo, "/x");
    let at = hours_from_now(-24 * 400) + 2 * DAY;
    let d = hand_snapshot(&repo, client(1), at, vec![root], vec![seg])
        .await
        .path();
    let s = repo
        .forget(ForgetOptions {
            policy: keep_last(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(s.removed.is_empty(), "{:?}", s.removed);
    assert_eq!(
        s.kept.len(),
        2,
        "{f} and {d} are each the newest of a group"
    );
}

#[tokio::test]
async fn the_newest_of_a_group_is_not_forgotten_without_retire() {
    let (t, repo) = setup().await;
    let old = snap(&repo, 1, 1, &["/a"]).await;
    let new = snap(&repo, 1, 2, &["/a"]).await;
    for spec in [new.clone(), "latest".to_owned()] {
        let err = repo
            .forget(ForgetOptions {
                snapshots: vec![spec.clone()],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, CoreError::Usage(m) if m.contains("--retire") && m.contains(&new)),
            "{spec}: {err}"
        );
    }
    assert_eq!(listed(&t).await.len(), 2, "nothing deleted");
    // 較舊的照刪
    let s = repo
        .forget(ForgetOptions {
            snapshots: vec![old.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.removed, vec![old]);
    // retire：最新的也刪
    let s = repo
        .forget(ForgetOptions {
            snapshots: vec![new.clone()],
            retire: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.removed, vec![new]);
    assert!(listed(&t).await.is_empty());
}

#[tokio::test]
async fn the_r1_is_deleted_before_the_body() {
    let (t, repo) = setup().await;
    let old = snap(&repo, 1, 1, &["/a"]).await;
    snap(&repo, 1, 2, &["/a"]).await;
    // replicas=1 的 snapshot 有 .r1：手工補一份
    let bytes = repo.backend().get(&old).await.unwrap();
    repo.backend()
        .put_if_absent(&format!("{old}.r1"), bytes)
        .await
        .unwrap();
    repo.forget(ForgetOptions {
        snapshots: vec![old.clone()],
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(t.store.deleted(), vec![format!("{old}.r1"), old]);
}

/// 短的寫法：`<client>/<時間戳>` 與時間戳的前綴。
#[tokio::test]
async fn short_specs_resolve_to_one_snapshot() {
    let (_t, repo) = setup().await;
    let old = snap(&repo, 1, 1, &["/a"]).await;
    snap(&repo, 1, 2, &["/a"]).await;
    let Key::Snapshot { client, ts, .. } = skey(&old) else {
        unreachable!()
    };
    for spec in [
        format!("{client}/{}", ts.as_str()),
        ts.as_str()[..12].to_owned(),
    ] {
        let s = repo
            .forget(ForgetOptions {
                snapshots: vec![spec.clone()],
                dry_run: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(s.removed, vec![old.clone()], "{spec}");
    }
}

#[tokio::test]
async fn a_dry_run_deletes_nothing() {
    let (t, repo) = setup().await;
    snap(&repo, 1, 1, &["/a"]).await;
    snap(&repo, 1, 2, &["/a"]).await;
    let s = repo
        .forget(ForgetOptions {
            policy: keep_last(1),
            dry_run: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.removed.len(), 1);
    assert_eq!(listed(&t).await.len(), 2);
    assert!(t.store.deleted().is_empty());
}

/// 兩份都解不開的 snapshot：政策不碰它（分不了組），以完整的 key 明確刪除時不必解密（P5 的解除方法）。
#[tokio::test]
async fn an_undecryptable_snapshot_is_left_by_the_policy_and_forgotten_by_its_full_key() {
    let (t, repo) = setup().await;
    snap(&repo, 1, 1, &["/a"]).await;
    snap(&repo, 1, 2, &["/a"]).await;
    let bad = format!("snapshots/{}/20200101T000000000000000Z", client(1));
    repo.backend()
        .put_if_absent(&bad, b"garbage".to_vec())
        .await
        .unwrap();
    repo.backend()
        .put_if_absent(&format!("{bad}.r1"), b"garbage".to_vec())
        .await
        .unwrap();
    let s = repo
        .forget(ForgetOptions {
            policy: keep_last(1),
            dry_run: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.unreadable, vec![bad.clone()]);
    assert!(!s.removed.contains(&bad));
    let s = repo
        .forget(ForgetOptions {
            snapshots: vec![bad.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.removed, vec![bad.clone()]);
    assert!(!listed(&t).await.iter().any(|k| k.starts_with(&bad)));
}

/// 只有 `.r1` 的 snapshot（commit 前中斷，或主體遺失）：以完整的 key 刪得掉。
#[tokio::test]
async fn an_r1_only_snapshot_is_forgotten_by_its_full_key() {
    let (t, repo) = setup().await;
    let k = snap(&repo, 1, 1, &["/a"]).await;
    let bytes = repo.backend().get(&k).await.unwrap();
    repo.backend()
        .put_if_absent(&format!("{k}.r1"), bytes)
        .await
        .unwrap();
    repo.backend().delete(&k).await.unwrap();
    let s = repo
        .forget(ForgetOptions {
            snapshots: vec![k.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.removed, vec![k]);
    assert!(listed(&t).await.is_empty());
}

#[tokio::test]
async fn an_unknown_snapshot_is_not_found() {
    let (_t, repo) = setup().await;
    snap(&repo, 1, 1, &["/a"]).await;
    let missing = format!("snapshots/{}/20200101T000000000000000Z", client(1));
    for spec in [missing, "2019".to_owned()] {
        let err = repo
            .forget(ForgetOptions {
                snapshots: vec![spec.clone()],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, CoreError::SnapshotNotFound(_)),
            "{spec}: {err}"
        );
    }
}

#[tokio::test]
async fn forget_needs_something_to_do_and_no_zero_counts() {
    let (_t, repo) = setup().await;
    let err = repo.forget(ForgetOptions::default()).await.unwrap_err();
    assert!(matches!(err, CoreError::Usage(_)), "{err}");
    let err = repo
        .forget(ForgetOptions {
            policy: keep_last(0),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Usage(_)), "{err}");
}

/// 政策照 restic 的桶：每天最新的一份。
#[tokio::test]
async fn keep_daily_keeps_the_newest_of_each_day() {
    let (_t, repo) = setup().await;
    let base = hours_from_now(-24 * 400);
    let mut keys = Vec::new();
    for (day, hour) in [(1, 1), (1, 5), (2, 1), (2, 9), (3, 2)] {
        let at = base + day * DAY + hour * 3_600_000_000_000;
        keys.push(
            hand_snapshot(
                &repo,
                ClientId::from_bytes([1; 16]),
                at,
                vec![file_root("/a")],
                vec![],
            )
            .await
            .path(),
        );
    }
    let s = repo
        .forget(ForgetOptions {
            policy: RetentionPolicy {
                keep_daily: Some(2),
                ..Default::default()
            },
            dry_run: true,
            ..Default::default()
        })
        .await
        .unwrap();
    // 第 3 天、第 2 天各留最新的一份；第 1 天的兩份與第 2 天較早的一份刪掉
    assert_eq!(
        s.removed,
        vec![keys[0].clone(), keys[1].clone(), keys[2].clone()]
    );
}
