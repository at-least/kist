//! backup 的排除規則（`BackupOptions::exclude`，ADR 020）：被排除的檔案與目錄不進
//! snapshot、計進 `report.excluded`；指名的來源本身不會被排除；樣式比對的是節點在
//! snapshot 裡的路徑（本機來源＝canonical 絕對路徑）。

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::*;
use kist_core::{BackupOptions, PathFilter, RestoreOptions};

fn excluding(patterns: &[&str]) -> BackupOptions {
    BackupOptions {
        exclude: PathFilter::new(patterns).unwrap(),
        ..backup_options()
    }
}

/// 還原出來的相對路徑集合（相對於 `root`）。
fn names_under(root: &Path) -> BTreeSet<PathBuf> {
    snapshot_dir(root).into_keys().collect()
}

fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

/// 測試資料：每一種排除方式各有一個會被排除、一個會被留下的對照。
fn make_tree(src: &Path) {
    for dir in [
        "node_modules",
        "sub/node_modules",
        "sub/cache",
        "sub/keep",
        "中文",
    ] {
        std::fs::create_dir_all(src.join(dir)).unwrap();
    }
    for (file, content) in [
        ("keep.txt", "k"),
        ("a.tmp", "t"),
        ("node_modules/x.js", "x"),
        ("sub/keep2.txt", "k2"),
        ("sub/b.tmp", "t"),
        ("sub/node_modules/y.js", "y"),
        ("sub/cache/z", "z"),
        ("sub/keep/cache", "not the anchored one"),
        ("中文/文.txt", "one character"),
        ("中文/長名.txt", "two characters"),
    ] {
        std::fs::write(src.join(file), content).unwrap();
    }
}

#[tokio::test]
async fn excluded_paths_are_left_out_and_counted() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_tree(&src);
    let abs = std::fs::canonicalize(&src).unwrap();
    let anchored_cache = format!("{}/sub/cache", abs.display());
    let anchored_cjk = format!("{}/中文/?.txt", abs.display());
    let repo = t.open().await;
    let s = repo
        .backup(
            std::slice::from_ref(&src),
            excluding(&["*.tmp", "node_modules", &anchored_cache, &anchored_cjk]),
        )
        .await
        .unwrap();
    // a.tmp、node_modules、sub/b.tmp、sub/node_modules、sub/cache、中文/文.txt：
    // 被排除的目錄算一個（不往下走）。
    assert_eq!(s.report.excluded, 6, "{:?}", s.report);
    assert_eq!(s.report.errors, 0, "排除不是錯誤");
    assert_eq!(s.stats.files, 4);

    let target = t.dir.path().join("out");
    let r = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let restored = target.join(abs.strip_prefix("/").unwrap());
    assert_eq!(
        names_under(&restored),
        set(&[
            "keep.txt",
            "sub",
            "sub/keep",
            "sub/keep/cache",
            "sub/keep2.txt",
            "中文",
            "中文/長名.txt",
        ])
    );
}

#[tokio::test]
async fn a_named_source_is_never_excluded() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_tree(&src);
    let abs = std::fs::canonicalize(&src).unwrap();
    let repo = t.open().await;
    // 目錄來源的名字本身符合樣式（錨定與不錨定各一條）：來源照備，樣式只管它底下的東西。
    let s = repo
        .backup(
            std::slice::from_ref(&src),
            excluding(&["src", &abs.display().to_string()]),
        )
        .await
        .unwrap();
    assert_eq!(s.report.excluded, 0);
    assert_eq!(s.stats.files, 10);
    // 單一檔案當來源：符合 `*.tmp` 也照備。
    let file = src.join("a.tmp");
    let s = repo
        .backup(std::slice::from_ref(&file), excluding(&["*.tmp"]))
        .await
        .unwrap();
    assert_eq!(s.report.excluded, 0);
    assert_eq!(s.stats.files, 1);
}

#[tokio::test]
async fn a_second_backup_with_the_same_excludes_reuses_everything() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_tree(&src);
    let repo = t.open().await;
    let patterns = ["*.tmp", "node_modules"];
    let first = repo
        .backup(std::slice::from_ref(&src), excluding(&patterns))
        .await
        .unwrap();
    let second = repo
        .backup(std::slice::from_ref(&src), excluding(&patterns))
        .await
        .unwrap();
    assert_eq!(second.parent.as_deref(), Some(first.snapshot_key.as_str()));
    assert_eq!(second.report.chunks_new, 0, "{:?}", second.report);
    assert_eq!(second.report.files_reused, first.stats.files);
    assert_eq!(second.report.excluded, first.report.excluded);
    assert_eq!(second.roots, first.roots, "內容沒變：同一組 root tree");
}
