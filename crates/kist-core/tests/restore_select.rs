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

/// 審查修正：include 樣式要涵蓋某個被還原的節點（比對到它或它的上層）才算選到東西。比對
/// 到、但被後面的反向樣式取消，或只比對到 root 的上層、而 root 被排除，都算沒選到。
#[tokio::test]
async fn an_include_counts_only_when_something_it_covers_is_restored() {
    let f = fixture().await;
    let docs = f.pattern("docs");
    let negated = format!("!{docs}");
    let (summary, got) = f.restore(select(&[&docs, &negated], &[])).await;
    assert_eq!(summary.unmatched_includes, vec![docs.clone()]);
    assert!(got.is_empty(), "{got:?}");

    let parent = f.abs.parent().unwrap().display().to_string();
    let root = f.abs.display().to_string();
    let (summary, got) = f.restore(select(&[&parent], &[&root])).await;
    assert_eq!(summary.unmatched_includes, vec![parent.clone()]);
    assert!(got.is_empty(), "{got:?}");

    // 對照：同一個節點被幾條樣式比對到時每一條都算，不只決定選取的最後一條（root 的上層
    // 與 root 本身、一個檔案與 `*.pdf`）。
    let report = f.pattern("docs/report.pdf");
    for include in [[parent.as_str(), root.as_str()], [report.as_str(), "*.pdf"]] {
        let (summary, got) = f.restore(select(&include, &[])).await;
        assert!(
            summary.unmatched_includes.is_empty(),
            "{include:?}: {:?}",
            summary.unmatched_includes
        );
        assert!(got.contains(Path::new("docs/report.pdf")), "{include:?}");
    }
}

/// 只比對到一個空的目錄 root 的 include 也算選到東西：落點本身被還原。
#[tokio::test]
async fn an_include_selecting_an_empty_root_counts() {
    let t = TestRepo::new().await;
    let empty = t.dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let abs = std::fs::canonicalize(&empty).unwrap();
    let repo = t.open().await;
    let key = repo
        .backup(std::slice::from_ref(&abs), backup_options())
        .await
        .unwrap()
        .snapshot_key;
    let target = t.dir.path().join("t");
    let opts = select(&[&abs.display().to_string()], &[]);
    let summary = repo.restore(&key, &target, opts).await.unwrap();
    assert!(
        summary.unmatched_includes.is_empty(),
        "{:?}",
        summary.unmatched_includes
    );
    assert!(target.join(abs.strip_prefix("/").unwrap()).is_dir());
}

/// 審查修正：只有反向樣式的 include 什麼都選不到（反向樣式只取消前面的 include 選到的），
/// 直接報錯、不建目標，不以「還原 0 個檔案」成功結束。dry run 一樣。
#[tokio::test]
async fn only_negated_includes_are_an_error() {
    let f = fixture().await;
    let target = f.t.dir.path().join("never");
    let opts = select(&["!*.tmp", "!scratch"], &[]);
    let err = f
        .repo
        .restore(&f.key, &target, opts.clone())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("negated"), "{err}");
    let err = f
        .repo
        .plan_restore(&f.key, &target, opts, &mut |_, _| {})
        .await
        .unwrap_err();
    assert!(err.to_string().contains("negated"), "{err}");
    assert!(!target.exists());
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
    // 檔案來源本身不是目錄，root 被排除時也一樣（審查修正）。dry run 一樣。
    let file = abs.join("readme.txt");
    for exclude in [
        PathFilter::default(),
        PathFilter::new(&[&file.display().to_string()]).unwrap(),
    ] {
        for path in [file.clone(), file.join("x")] {
            let opts = RestoreOptions {
                subtree: subtree(&path),
                exclude: exclude.clone(),
                ..RestoreOptions::default()
            };
            let err = repo.restore(&key, &target, opts.clone()).await.unwrap_err();
            assert!(err.to_string().contains("not a directory"), "{err}");
            let err = repo
                .plan_restore(&key, &target, opts, &mut |_, _| {})
                .await
                .unwrap_err();
            assert!(err.to_string().contains("not a directory"), "{err}");
        }
    }
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
        select(
            &[&f.pattern("docs"), &format!("!{}", f.pattern("docs"))],
            &[],
        ),
        select(
            &[&f.abs.parent().unwrap().display().to_string()],
            &[&f.abs.display().to_string()],
        ),
        select(&[&f.pattern("docs/report.pdf"), "*.pdf"], &[]),
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

/// 反向樣式（ADR 023）：include 的 `!` 從選到的目錄裡取消一部分；exclude 的 `!` 把比對到的
/// 加回來。都是同一個節點上最後一個比對到的樣式決定。反向的 include 沒比對到東西不回報。
#[tokio::test]
async fn negated_patterns_carve_out_selections() {
    let f = fixture().await;
    let (summary, got) = f
        .restore(select(
            &[
                &f.pattern("docs"),
                &format!("!{}", f.pattern("docs/old")),
                "!*.nope",
            ],
            &[],
        ))
        .await;
    assert!(
        summary.unmatched_includes.is_empty(),
        "{:?}",
        summary.unmatched_includes
    );
    assert_eq!(got, set(&["docs", "docs/notes.txt", "docs/report.pdf"]));

    let (_, got) = f
        .restore(select(
            &[],
            &[&f.pattern("docs/*"), &format!("!{}", f.pattern("docs/old"))],
        ))
        .await;
    assert!(got.contains(Path::new("docs/old/a.pdf")));
    assert!(!got.contains(Path::new("docs/report.pdf")));
    assert!(!got.contains(Path::new("docs/notes.txt")));
    assert!(got.contains(Path::new("readme.txt")));
}

/// 不分大小寫的樣式（ADR 023）。
#[tokio::test]
async fn ignore_case_patterns_select_and_exclude() {
    let f = fixture().await;
    let opts = RestoreOptions {
        include: PathFilter::default().and_ignoring_case(&["*.PDF"]).unwrap(),
        exclude: PathFilter::default()
            .and_ignoring_case(&["REPORT.pdf"])
            .unwrap(),
        ..RestoreOptions::default()
    };
    let (_, got) = f.restore(opts).await;
    assert_eq!(got, set(&["docs", "docs/old", "docs/old/a.pdf"]));
}

/// 審查修正：SNAPSHOT:PATH 在一個被剪掉的 root 裡面時，照樣先確認路徑存在、是目錄——
/// 打錯的路徑不能因為 root 被排除就以「什麼都沒還原」成功結束。dry run 一樣。
#[tokio::test]
async fn a_subtree_is_checked_even_when_its_root_is_pruned() {
    let f = fixture().await;
    let target = f.t.dir.path().join("t");
    let root = f.abs.display().to_string();
    for (path, needle) in [
        (f.abs.join("typo"), "not in this snapshot"),
        (f.abs.join("readme.txt"), "not a directory"),
    ] {
        let opts = RestoreOptions {
            subtree: subtree(&path),
            exclude: PathFilter::new(&[&root]).unwrap(),
            ..RestoreOptions::default()
        };
        let err = f
            .repo
            .restore(&f.key, &target, opts.clone())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(needle),
            "{}: {err}",
            path.display()
        );
        let err = f
            .repo
            .plan_restore(&f.key, &target, opts, &mut |_, _| {})
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(needle),
            "dry run {}: {err}",
            path.display()
        );
    }
    // 對照：路徑存在、只是被排除時，什麼都不還原，不是錯誤。
    let opts = RestoreOptions {
        subtree: subtree(&f.abs.join("docs")),
        exclude: PathFilter::new(&[&root]).unwrap(),
        ..RestoreOptions::default()
    };
    let summary = f.repo.restore(&f.key, &target, opts).await.unwrap();
    assert_eq!((summary.files, summary.dirs), (0, 0));
}

/// 審查修正：SNAPSHOT:PATH 不對時，在建立任何目錄之前就回錯。
#[tokio::test]
async fn a_bad_subtree_creates_no_target_directory() {
    let f = fixture().await;
    for path in [f.abs.join("nope"), f.abs.join("readme.txt")] {
        let target = f.t.dir.path().join("new/dir/x");
        let opts = RestoreOptions {
            subtree: subtree(&path),
            ..RestoreOptions::default()
        };
        f.repo.restore(&f.key, &target, opts).await.unwrap_err();
        assert!(!f.t.dir.path().join("new").exists(), "{}", path.display());
    }
}
