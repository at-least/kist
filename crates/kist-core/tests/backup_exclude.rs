//! backup 的排除規則（`BackupOptions::exclude`，ADR 020）：被排除的檔案與目錄不進
//! snapshot、計進 `report.excluded`；指名的來源本身不會被排除；樣式比對的是節點在
//! snapshot 裡的路徑（本機來源＝canonical 絕對路徑）。

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::*;
use kist_core::{BackupOptions, Exclusions, Marker, PathFilter, RestoreOptions};

fn excluding(patterns: &[&str]) -> BackupOptions {
    with(Exclusions {
        patterns: PathFilter::new(patterns).unwrap(),
        ..Exclusions::default()
    })
}

fn with(exclude: Exclusions) -> BackupOptions {
    BackupOptions {
        exclude,
        ..backup_options()
    }
}

/// 備份 `src` 再整份還原，回傳還原出來、相對於 `src` 落點的路徑集合與 backup 的 excluded。
async fn round_trip(t: &TestRepo, src: &Path, opts: BackupOptions) -> (BTreeSet<PathBuf>, u64) {
    let repo = t.open().await;
    let s = repo.backup(&[src.to_path_buf()], opts).await.unwrap();
    assert_eq!(s.report.errors, 0, "{:?}", s.report);
    let target = tempfile::tempdir_in(t.dir.path()).unwrap().keep();
    let r = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let abs = std::fs::canonicalize(src).unwrap();
    (
        names_under(&target.join(abs.strip_prefix("/").unwrap())),
        s.report.excluded,
    )
}

fn files(root: &Path, entries: &[(&str, &[u8])]) {
    for (rel, content) in entries {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
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

/// README／ADR 020 的例子：遠端 root 只剝掉 `scheme://`，URL 裡的 `[user@]host[:port]` 原樣是
/// 路徑的第一段，錨定樣式要照它寫。
#[test]
fn a_remote_root_keeps_user_host_and_port_as_its_first_component() {
    let comps = kist_core::fsmeta::locator_components(b"sftp://me@nas:2222/srv/data").unwrap();
    assert_eq!(comps, [&b"me@nas:2222"[..], b"srv", b"data"]);
    let filter = PathFilter::new(&["/me@nas:2222/srv/data/x"]).unwrap();
    let mut state = filter.start();
    for name in comps.into_iter().chain([&b"x"[..]]) {
        state = filter.descend(&state, name);
    }
    assert!(state.is_match());
}

/// 目錄裡有記號檔時，除了記號檔本身，內容都不備份（目錄本身照備）。指名的來源裡有記號檔
/// 也一樣——記號檔管的是內容，不是來源本身。記號是目錄也算有。
#[tokio::test]
async fn a_marker_file_excludes_the_rest_of_its_directory() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    files(
        &src,
        &[
            ("proj/.nobackup", b""),
            ("proj/a.txt", b"a"),
            ("proj/sub/b", b"b"),
            ("other/c", b"c"),
            ("d/.nobackup/inside", b"kept with the marker"),
            ("d/e", b"e"),
        ],
    );
    let marker = || Exclusions {
        if_present: vec![Marker::parse(".nobackup").unwrap()],
        ..Exclusions::default()
    };
    let (got, excluded) = round_trip(&t, &src, with(marker())).await;
    assert_eq!(
        got,
        set(&[
            "d",
            "d/.nobackup",
            "d/.nobackup/inside",
            "other",
            "other/c",
            "proj",
            "proj/.nobackup",
        ])
    );
    assert_eq!(excluded, 3, "proj/a.txt、proj/sub、d/e");

    let (got, _) = round_trip(&t, &src.join("proj"), with(marker())).await;
    assert_eq!(got, set(&[".nobackup"]));
}

/// `--exclude-caches`：CACHEDIR.TAG 的開頭必須是規格的 43 bytes 簽章（https://bford.info/cachedir/），
/// 內容不對的照常備份。
#[tokio::test]
async fn cachedir_tags_must_carry_the_signature() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    files(
        &src,
        &[
            (
                "good/CACHEDIR.TAG",
                b"Signature: 8a477f597d28d172789f06886806bc55\n# a cache\n",
            ),
            ("good/x", b"x"),
            ("bad/CACHEDIR.TAG", b"Signature: not the right one"),
            ("bad/y", b"y"),
        ],
    );
    let opts = with(Exclusions {
        if_present: vec![Marker::cachedir_tag()],
        ..Exclusions::default()
    });
    let (got, excluded) = round_trip(&t, &src, opts).await;
    assert_eq!(
        got,
        set(&[
            "bad",
            "bad/CACHEDIR.TAG",
            "bad/y",
            "good",
            "good/CACHEDIR.TAG"
        ])
    );
    assert_eq!(excluded, 1);
}

#[test]
fn marker_specs_are_name_and_optional_header() {
    assert_eq!(
        Marker::parse("x:abc").unwrap(),
        Marker::parse("x:abc").unwrap()
    );
    assert_eq!(
        Marker::parse("CACHEDIR.TAG:Signature: 8a477f597d28d172789f06886806bc55").unwrap(),
        Marker::cachedir_tag()
    );
    for bad in ["", ":abc", "a/b", ".", "..", "./x"] {
        assert!(Marker::parse(bad).is_err(), "{bad:?} 不是單一路徑元件");
    }
}

/// `--exclude-larger-than`：比上限大的檔案不備份，剛好等於上限的照備；指名的檔案來源不排除。
#[tokio::test]
async fn files_larger_than_the_limit_are_left_out() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    files(
        &src,
        &[
            ("small", &[1u8; 10]),
            ("exact", &[2u8; 100]),
            ("dir/big", &[3u8; 1000]),
        ],
    );
    let limit = || Exclusions {
        larger_than: Some(100),
        ..Exclusions::default()
    };
    let (got, excluded) = round_trip(&t, &src, with(limit())).await;
    assert_eq!(got, set(&["dir", "exact", "small"]));
    assert_eq!(excluded, 1);

    let repo = t.open().await;
    let s = repo
        .backup(&[src.join("dir/big")], with(limit()))
        .await
        .unwrap();
    assert_eq!((s.stats.files, s.report.excluded), (1, 0));
}

/// 反向樣式（ADR 023）：「家目錄底下只備 Documents」——同 restic 文件的例子。不分大小寫的
/// 樣式也走同一套。
#[tokio::test]
async fn negated_and_ignore_case_excludes() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    files(
        &src,
        &[
            ("Documents/a", b"a"),
            ("Music/b", b"b"),
            ("top.txt", b"t"),
            ("Documents/junk.TMP", b"j"),
        ],
    );
    let abs = std::fs::canonicalize(&src).unwrap();
    let patterns = PathFilter::new(&[
        format!("{}/*", abs.display()),
        format!("!{}/Documents", abs.display()),
    ])
    .unwrap()
    .and_ignoring_case(&["*.tmp"])
    .unwrap();
    let (got, excluded) = round_trip(
        &t,
        &src,
        with(Exclusions {
            patterns,
            ..Exclusions::default()
        }),
    )
    .await;
    assert_eq!(got, set(&["Documents", "Documents/a"]));
    assert_eq!(excluded, 3, "Music、top.txt、junk.TMP");

    // 目錄一旦被排除就整棵不走，裡面的東西加不回來（restic 文件寫的限制，同 gitignore）。
    let patterns = PathFilter::new(&[
        format!("{}/Music", abs.display()),
        format!("!{}/Music/b", abs.display()),
    ])
    .unwrap();
    let (got, _) = round_trip(
        &t,
        &src,
        with(Exclusions {
            patterns,
            ..Exclusions::default()
        }),
    )
    .await;
    assert!(!got.contains(Path::new("Music")), "{got:?}");
    assert!(!got.contains(Path::new("Music/b")), "{got:?}");
    assert!(got.contains(Path::new("top.txt")));
}

/// 審查修正：指名的來源包在另一個指名來源裡時，仍然是指名的——外層走訪到它時，排除樣式、
/// 大小上限、記號檔都不排除它；它在被排除的目錄底下時，那些上層只沿著通往它的路徑走進去。
#[tokio::test]
async fn a_named_source_inside_another_is_never_excluded() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    files(
        &src,
        &[
            ("keep/k.txt", b"k"),
            ("big.bin", &[7u8; 5000]),
            ("small.txt", b"s"),
            ("x/y/f", b"f"),
            ("x/other", b"o"),
            ("m/.nobackup", b""),
            ("m/z/g", b"g"),
            ("m/drop", b"d"),
        ],
    );
    let abs = std::fs::canonicalize(&src).unwrap();
    let repo = t.open().await;
    let opts = with(Exclusions {
        patterns: PathFilter::new(&["keep", &format!("{}/x", abs.display())]).unwrap(),
        if_present: vec![Marker::parse(".nobackup").unwrap()],
        larger_than: Some(1000),
        ..Exclusions::default()
    });
    let named = [
        src.clone(),
        src.join("keep"),
        src.join("big.bin"),
        src.join("x/y"),
        src.join("m/z"),
    ];
    let s = repo.backup(&named, opts).await.unwrap();
    assert_eq!(s.report.errors, 0, "{:?}", s.report);
    assert_eq!(s.roots.len(), 1, "巢狀的指名來源仍併進外層的 root");
    let target = tempfile::tempdir_in(t.dir.path()).unwrap().keep();
    let r = repo
        .restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        names_under(&target.join(abs.strip_prefix("/").unwrap())),
        set(&[
            "big.bin",
            "keep",
            "keep/k.txt",
            "m",
            "m/.nobackup",
            "m/z",
            "m/z/g",
            "small.txt",
            "x",
            "x/y",
            "x/y/f",
        ]),
        "x/other 與 m/drop 照樣被排除"
    );
}

/// 審查修正：沒有 header 的記號只看在不在，讀不了（權限 000）也算數——restic 0.19.1 同樣
/// 會排除該目錄的內容。記號本身照樣備份，讀不了就是一個略過的錯誤。
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_marker_without_a_header_still_counts() {
    use std::os::unix::fs::PermissionsExt;
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    files(
        &src,
        &[("d/x.txt", b"x"), ("d/.nobackup", b""), ("y.txt", b"y")],
    );
    std::fs::set_permissions(
        src.join("d/.nobackup"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let repo = t.open().await;
    let opts = with(Exclusions {
        patterns: PathFilter::new(&[".nobackup"]).unwrap(),
        if_present: vec![Marker::parse(".nobackup").unwrap()],
        ..Exclusions::default()
    });
    let s = repo.backup(std::slice::from_ref(&src), opts).await.unwrap();
    assert_eq!(s.report.errors, 0, "{:?}", s.report);
    assert_eq!(s.report.excluded, 2, "x.txt 與記號本身：{:?}", s.report);
    std::fs::set_permissions(
        src.join("d/.nobackup"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let target = tempfile::tempdir_in(t.dir.path()).unwrap().keep();
    repo.restore(&s.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let abs = std::fs::canonicalize(&src).unwrap();
    assert_eq!(
        names_under(&target.join(abs.strip_prefix("/").unwrap())),
        set(&["d", "y.txt"])
    );
}
