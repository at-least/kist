//! 部分還原（`RestoreOptions::include`／`exclude`，ADR 020）：只還原選到的節點與它們底下的
//! 一切；沿途的上層目錄照樣建立並套記錄的 metadata；沒被選到的分支一個目錄也不建，
//! 不可能有東西被選到的子樹連 tree 都不讀。

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::*;
use kist_core::{PathFilter, Repository, RestoreOptions, RestoreSummary};

/// 測試資料（目錄名與檔名都有對照）。回傳 canonical 絕對路徑：樣式比對的就是它。
fn make_tree(src: &Path) -> PathBuf {
    for dir in ["docs/old", "photos/sub", "scratch", "empty"] {
        std::fs::create_dir_all(src.join(dir)).unwrap();
    }
    for (file, content) in [
        ("readme.txt", "r"),
        ("docs/report.pdf", "report"),
        ("docs/notes.txt", "notes"),
        ("docs/old/a.pdf", "old"),
        ("photos/p1.jpg", "p1"),
        ("photos/sub/p2.jpg", "p2"),
        ("scratch/x.tmp", "x"),
    ] {
        std::fs::write(src.join(file), content).unwrap();
    }
    std::fs::canonicalize(src).unwrap()
}

fn select(include: &[&str], exclude: &[&str]) -> RestoreOptions {
    RestoreOptions {
        include: PathFilter::new(include).unwrap(),
        exclude: PathFilter::new(exclude).unwrap(),
        ..RestoreOptions::default()
    }
}

/// `target` 底下、相對於 `abs` 落點的全部路徑（檔案、目錄、symlink）。落點不存在時是空的。
fn restored(target: &Path, abs: &Path) -> BTreeSet<PathBuf> {
    let base = target.join(abs.strip_prefix("/").unwrap());
    if !base.exists() {
        return BTreeSet::new();
    }
    snapshot_dir(&base).into_keys().collect()
}

fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

struct Fixture {
    t: TestRepo,
    repo: Repository,
    abs: PathBuf,
    key: String,
}

async fn fixture() -> Fixture {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    let abs = make_tree(&src);
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    Fixture {
        t,
        repo,
        abs,
        key: s.snapshot_key,
    }
}

impl Fixture {
    fn pattern(&self, rel: &str) -> String {
        format!("{}/{rel}", self.abs.display())
    }

    async fn restore(&self, opts: RestoreOptions) -> (RestoreSummary, BTreeSet<PathBuf>) {
        let target = tempfile::tempdir_in(self.t.dir.path()).unwrap().keep();
        let summary = self.repo.restore(&self.key, &target, opts).await.unwrap();
        (summary, restored(&target, &self.abs))
    }
}

#[tokio::test]
async fn include_one_file_restores_it_and_the_directories_above_it() {
    let f = fixture().await;
    // 上層目錄的 metadata 也要照記錄套上：先把 docs 改成可辨認的 mode 與 mtime 再備份。
    let docs = f.abs.join("docs");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docs, std::fs::Permissions::from_mode(0o750)).unwrap();
    }
    let mtime = filetime::FileTime::from_unix_time(1_500_000_000, 0);
    filetime::set_file_mtime(&docs, mtime).unwrap();
    let key = f
        .repo
        .backup(std::slice::from_ref(&f.abs), backup_options())
        .await
        .unwrap()
        .snapshot_key;

    let target = f.t.dir.path().join("one");
    let opts = select(&[&f.pattern("docs/report.pdf")], &[]);
    let summary = f.repo.restore(&key, &target, opts).await.unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert!(summary.unmatched_includes.is_empty());
    assert_eq!(restored(&target, &f.abs), set(&["docs", "docs/report.pdf"]));
    assert_eq!((summary.files, summary.dirs, summary.symlinks), (1, 1, 0));
    let restored_docs = target.join(docs.strip_prefix("/").unwrap());
    let meta = std::fs::metadata(&restored_docs).unwrap();
    assert_eq!(
        filetime::FileTime::from_last_modification_time(&meta),
        mtime
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o7777, 0o750);
    }
    assert_eq!(
        std::fs::read(restored_docs.join("report.pdf")).unwrap(),
        b"report"
    );
}

#[tokio::test]
async fn include_a_directory_restores_everything_below_it() {
    let f = fixture().await;
    let (summary, got) = f.restore(select(&[&f.pattern("docs")], &[])).await;
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(
        got,
        set(&[
            "docs",
            "docs/notes.txt",
            "docs/old",
            "docs/old/a.pdf",
            "docs/report.pdf"
        ])
    );
}

#[tokio::test]
async fn an_unanchored_include_selects_at_every_depth() {
    let f = fixture().await;
    let (summary, got) = f.restore(select(&["*.pdf"], &[])).await;
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(
        got,
        set(&["docs", "docs/old", "docs/old/a.pdf", "docs/report.pdf"]),
        "只有通往 pdf 的目錄被建，photos／tmp／empty 都沒有"
    );
    assert_eq!(summary.files, 2);
}

#[tokio::test]
async fn exclude_skips_files_and_whole_directories() {
    let f = fixture().await;
    let (summary, got) = f.restore(select(&[], &["scratch", "*.jpg"])).await;
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(
        got,
        set(&[
            "docs",
            "docs/notes.txt",
            "docs/old",
            "docs/old/a.pdf",
            "docs/report.pdf",
            "empty",
            "photos",
            "photos/sub",
            "readme.txt",
        ]),
        "沒有 include 時目錄照常還原，即使裡面的東西都被排除"
    );
    // restore 的樣式也比對 root 定位本身與它的上層（backup 不會：指名的來源不排除）。
    // 這份 snapshot 的 root 是 `…/src`：不錨定的 `src` 排除整個 root。
    let (summary, got) = f.restore(select(&[], &["src"])).await;
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(got, set(&[]));
}

#[tokio::test]
async fn exclude_wins_over_include() {
    let f = fixture().await;
    let (summary, got) = f.restore(select(&[&f.pattern("docs")], &["*.pdf"])).await;
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(got, set(&["docs", "docs/notes.txt", "docs/old"]));
}

#[tokio::test]
async fn includes_that_match_nothing_are_reported() {
    let f = fixture().await;
    let nope = f.pattern("nope");
    let (summary, got) = f
        .restore(select(
            &[
                &f.pattern("readme.txt"),
                &nope,
                "*.docx",
                &f.pattern("docs"),
            ],
            &[],
        ))
        .await;
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(summary.unmatched_includes, vec![nope, "*.docx".to_owned()]);
    assert!(got.contains(Path::new("readme.txt")));
    assert!(got.contains(Path::new("docs/report.pdf")));

    // 比對到的全被 exclude 排除也算沒選到：`*.jpg` 只在 photos 底下，而 photos 整棵被
    // 排除（不會走進去）；`*.pdf` 比對到的 report.pdf 被排除，但 old/a.pdf 留下。
    let (summary, got) = f
        .restore(select(&["*.jpg", "*.pdf"], &["photos", "report.pdf"]))
        .await;
    assert_eq!(summary.unmatched_includes, vec!["*.jpg".to_owned()]);
    assert_eq!(got, set(&["docs", "docs/old", "docs/old/a.pdf"]));
}

/// 剪枝的證據：把 photos 子目錄的 tree 物件刪掉。整份還原會撞到它（證明這個測試抓得到
/// 「有去讀」）；只 include docs 的還原完全不碰它。
#[tokio::test]
async fn an_anchored_include_does_not_read_unrelated_trees() {
    let f = fixture().await;
    let snap = f.repo.read_snapshot_by_key(&f.key).await.unwrap();
    let top = f.repo.read_tree_chain(&snap.roots[0].tree).await.unwrap();
    let photos = top.iter().find(|e| e.name == b"photos").unwrap();
    let tree_file =
        f.t.repo_path()
            .join(kist_format::keys::tree(&photos.subtree));
    std::fs::remove_file(&tree_file).unwrap();

    let (full, _) = f.restore(RestoreOptions::default()).await;
    assert_eq!(
        full.errors.len(),
        1,
        "整份還原要讀 photos：{:?}",
        full.errors
    );

    let (summary, got) = f.restore(select(&[&f.pattern("docs")], &[])).await;
    assert!(
        summary.errors.is_empty(),
        "不該讀 photos 的 tree：{:?}",
        summary.errors
    );
    assert!(got.contains(Path::new("docs/report.pdf")));
}

/// 硬連結的第一個名字被排除時，第二個名字照樣還原成完整的檔案（硬連結表只記真的還原出來的名字）。
#[cfg(unix)]
#[tokio::test]
async fn a_hard_link_whose_first_name_is_excluded_is_restored_as_a_file() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    std::fs::create_dir_all(src.join("a")).unwrap();
    std::fs::create_dir_all(src.join("b")).unwrap();
    std::fs::write(src.join("a/first"), random_bytes(7, 100 * 1024)).unwrap();
    std::fs::hard_link(src.join("a/first"), src.join("b/second")).unwrap();
    let abs = std::fs::canonicalize(&src).unwrap();
    let repo = t.open().await;
    let s = repo
        .backup(std::slice::from_ref(&src), backup_options())
        .await
        .unwrap();
    let target = t.dir.path().join("out");
    let opts = select(&[], &[&format!("{}/a", abs.display())]);
    let summary = repo.restore(&s.snapshot_key, &target, opts).await.unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(restored(&target, &abs), set(&["b", "b/second"]));
    let second = target.join(abs.strip_prefix("/").unwrap()).join("b/second");
    assert_eq!(
        std::fs::read(second).unwrap(),
        std::fs::read(src.join("a/first")).unwrap()
    );
}

/// 多個 root：不可能有東西被選到的 root 連落點都不建；樣式也可以比對 root 定位的上層
/// （整個 root 被選到或被排除）。
#[tokio::test]
async fn roots_are_selected_by_their_path() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    let abs = make_tree(&src);
    let repo = t.open().await;
    let roots = [src.join("docs"), src.join("photos"), src.join("readme.txt")];
    let key = repo
        .backup(&roots, backup_options())
        .await
        .unwrap()
        .snapshot_key;
    let run = |opts: RestoreOptions| {
        let repo = &repo;
        let key = &key;
        let target = tempfile::tempdir_in(t.dir.path()).unwrap().keep();
        let abs = abs.clone();
        async move {
            let summary = repo.restore(key, &target, opts).await.unwrap();
            assert!(summary.errors.is_empty(), "{:?}", summary.errors);
            (summary, restored(&target, &abs))
        }
    };
    let p = |rel: &str| format!("{}/{rel}", abs.display());

    let (_, got) = run(select(&[&p("photos/sub")], &[])).await;
    assert_eq!(got, set(&["photos", "photos/sub", "photos/sub/p2.jpg"]));

    let (_, got) = run(select(&[&abs.display().to_string()], &[])).await;
    assert!(got.contains(Path::new("docs/old/a.pdf")));
    assert!(got.contains(Path::new("photos/p1.jpg")));
    assert!(got.contains(Path::new("readme.txt")));

    let (_, got) = run(select(&[], &[&p("docs"), &p("readme.txt")])).await;
    assert_eq!(
        got,
        set(&["photos", "photos/p1.jpg", "photos/sub", "photos/sub/p2.jpg"])
    );

    let (summary, got) = run(select(&["readme.txt"], &[])).await;
    assert_eq!(got, set(&["readme.txt"]), "檔案 root 也能被 include 選到");
    assert_eq!(summary.files, 1);
}

// --- 只還原一個子目錄、直接放進目標（`RestoreOptions::subtree`，ADR 022）---

fn subtree(path: &Path) -> Vec<Vec<u8>> {
    kist_core::restore::subtree_components(path.to_str().unwrap()).unwrap()
}

/// 目標底下的全部路徑（不再加 root 的定位）。
fn under(target: &Path) -> BTreeSet<PathBuf> {
    snapshot_dir(target).into_keys().collect()
}

#[tokio::test]
async fn a_subtree_lands_directly_in_the_target() {
    let f = fixture().await;
    let run = |opts: RestoreOptions| {
        let target = tempfile::tempdir_in(f.t.dir.path()).unwrap().keep();
        let repo = &f.repo;
        let key = &f.key;
        async move {
            let summary = repo.restore(key, &target, opts).await.unwrap();
            assert!(summary.errors.is_empty(), "{:?}", summary.errors);
            under(&target)
        }
    };
    let docs = RestoreOptions {
        subtree: subtree(&f.abs.join("docs")),
        ..RestoreOptions::default()
    };
    assert_eq!(
        run(docs).await,
        set(&["notes.txt", "old", "old/a.pdf", "report.pdf"])
    );
    // root 本身：內容直接放進目標。
    let root = RestoreOptions {
        subtree: subtree(&f.abs),
        ..RestoreOptions::default()
    };
    let got = run(root).await;
    assert!(got.contains(Path::new("readme.txt")));
    assert!(got.contains(Path::new("photos/sub/p2.jpg")));
    // root 的上一層：從那裡往下的相對路徑。
    let above = RestoreOptions {
        subtree: subtree(f.abs.parent().unwrap()),
        ..RestoreOptions::default()
    };
    assert!(run(above).await.contains(Path::new("src/docs/report.pdf")));
    // 樣式照樣比對 snapshot 裡的完整路徑。
    let filtered = RestoreOptions {
        subtree: subtree(&f.abs.join("docs")),
        exclude: PathFilter::new(&[f.pattern("docs/old")]).unwrap(),
        ..RestoreOptions::default()
    };
    assert_eq!(run(filtered).await, set(&["notes.txt", "report.pdf"]));
}

#[tokio::test]
async fn a_subtree_must_be_a_directory_in_the_snapshot() {
    let f = fixture().await;
    let target = f.t.dir.path().join("t");
    for (path, needle) in [
        (f.abs.join("nope"), "not in this snapshot"),
        (f.abs.join("readme.txt"), "not a directory"),
        (PathBuf::from("/elsewhere/entirely"), "not in this snapshot"),
    ] {
        let opts = RestoreOptions {
            subtree: subtree(&path),
            ..RestoreOptions::default()
        };
        let err = f.repo.restore(&f.key, &target, opts).await.unwrap_err();
        assert!(
            err.to_string().contains(needle),
            "{}: {err}",
            path.display()
        );
    }
    for bad in ["relative/path", "/a/../b", "/a/./b", ""] {
        assert!(
            kist_core::restore::subtree_components(bad).is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(
        kist_core::restore::subtree_components("/").unwrap(),
        Vec::<Vec<u8>>::new(),
        "`/` 是頂端：整份還原的版面"
    );
}

#[tokio::test]
async fn a_subtree_spanning_several_roots() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    let abs = make_tree(&src);
    let repo = t.open().await;
    let roots = [src.join("docs"), src.join("photos"), src.join("readme.txt")];
    let key = repo
        .backup(&roots, backup_options())
        .await
        .unwrap()
        .snapshot_key;
    let target = t.dir.path().join("all");
    let opts = RestoreOptions {
        subtree: subtree(&abs),
        ..RestoreOptions::default()
    };
    let summary = repo.restore(&key, &target, opts).await.unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let got = under(&target);
    for p in ["docs/old/a.pdf", "photos/sub/p2.jpg", "readme.txt"] {
        assert!(got.contains(Path::new(p)), "{p} missing from {got:?}");
    }
    let target = t.dir.path().join("sub");
    let opts = RestoreOptions {
        subtree: subtree(&abs.join("photos/sub")),
        ..RestoreOptions::default()
    };
    repo.restore(&key, &target, opts).await.unwrap();
    assert_eq!(under(&target), set(&["p2.jpg"]));
}

// --- dry run（`Repository::plan_restore`，ADR 022）：與真的還原寫出來的完全一致 ---

#[tokio::test]
async fn a_dry_run_predicts_exactly_what_a_restore_writes() {
    let f = fixture().await;
    let cases: Vec<RestoreOptions> = vec![
        RestoreOptions::default(),
        select(&[&f.pattern("docs/report.pdf")], &[]),
        select(&[&f.pattern("docs")], &[]),
        select(&["*.pdf"], &[]),
        select(&[], &["scratch", "*.jpg"]),
        select(&[&f.pattern("docs")], &["*.pdf"]),
        select(&["*.jpg", "*.nope"], &["photos/sub"]),
        RestoreOptions {
            subtree: subtree(&f.abs.join("photos")),
            ..RestoreOptions::default()
        },
    ];
    for opts in cases {
        let target = tempfile::tempdir_in(f.t.dir.path()).unwrap().keep();
        let probe = target.join("never-created");
        let mut planned = BTreeSet::new();
        let plan = f
            .repo
            .plan_restore(&f.key, &probe, opts.clone(), &mut |path, _| {
                planned.insert(path.strip_prefix(&probe).unwrap().to_path_buf());
            })
            .await
            .unwrap();
        assert!(!probe.exists(), "dry run 不碰目標");

        let real = f.repo.restore(&f.key, &probe, opts.clone()).await.unwrap();
        assert!(real.errors.is_empty(), "{:?}", real.errors);
        assert_eq!(
            (
                plan.files,
                plan.dirs,
                plan.symlinks,
                &plan.unmatched_includes
            ),
            (
                real.files,
                real.dirs,
                real.symlinks,
                &real.unmatched_includes
            ),
            "{opts:?}"
        );
        // 真的還原還會建出 root 的落點與它的上層（不是 snapshot 裡的節點，dry run 不列）。
        let base = if opts.subtree.is_empty() {
            f.abs.strip_prefix("/").unwrap().to_path_buf()
        } else {
            PathBuf::new()
        };
        let written: BTreeSet<PathBuf> = under(&probe)
            .into_iter()
            .filter(|p| !base.starts_with(p))
            .collect();
        assert_eq!(planned, written, "{opts:?}");
    }
}
