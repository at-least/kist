//! v4 backup 的「直接遠端備份」，以注入的記憶體來源（Source 抽象）測：metadata 依來源種類記錄、快速路徑依
//! 來源分級（s3 的 etag 可沿用、sftp 一律重讀）、遠端檔案 root 的還原映射與本機檔案 root 對稱（§9）。搬自
//! v3 的 `source_backup.rs`（v4 的走訪是 v3 的複製）。

#[path = "v4_common/mod.rs"]
mod v4_common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use kist_backend::source::{Source, SourceItem, SourceItemKind};
use kist_backend::BackendError;
use kist_core::v4::BackupOptions;
use kist_core::{RestoreOptions, SourceSpec};
use kist_format::v4::meta::meta_kind;
use kist_format::v4::snapshot::Root;
use v4_common::*;

/// 記憶體來源：檔案放在 rel（`/` 分隔）→ 內容的 map，**目錄由檔案路徑推導**
/// （與 object_store 的 common prefix 同一形狀，所以空目錄不存在）。
/// `mk` 決定來源種類：S3（帶可控的 etag）或 SFTP（無 etag）。
#[derive(Clone)]
struct FakeSource {
    locator: Vec<u8>,
    mk: u8,
    files: BTreeMap<Vec<u8>, Vec<u8>>,
    mtimes: BTreeMap<Vec<u8>, i64>,
    etags: BTreeMap<Vec<u8>, Vec<u8>>,
    /// `Source::read_ahead`（預設 0，同本機來源）。
    ahead: usize,
    /// `list` 會失敗的目錄。
    unlistable: BTreeSet<Vec<u8>>,
    /// 開過的讀取與列過的目錄，所有 clone 共用（注入的是 clone）。
    reads: Arc<std::sync::Mutex<Reads>>,
}

/// [`FakeSource`] 開過的讀取：照順序的 rel、現在開著幾個、最多同時開著幾個；列過的目錄（照順序）。
#[derive(Default)]
struct Reads {
    opened: Vec<String>,
    open_now: usize,
    most_open: usize,
    listed: Vec<String>,
}

/// 關掉（drop）時把 [`Reads::open_now`] 減一的讀取。
struct Counted {
    inner: std::io::Cursor<Vec<u8>>,
    reads: Arc<std::sync::Mutex<Reads>>,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.reads.lock().unwrap().open_now -= 1;
    }
}

impl FakeSource {
    fn new(locator: &str, mk: u8) -> Self {
        Self {
            locator: locator.as_bytes().to_vec(),
            mk,
            files: BTreeMap::new(),
            mtimes: BTreeMap::new(),
            etags: BTreeMap::new(),
            ahead: 0,
            unlistable: BTreeSet::new(),
            reads: Arc::default(),
        }
    }

    /// 列過的目錄（照順序，根是 ""），並清掉紀錄。
    fn take_listed(&self) -> Vec<String> {
        std::mem::take(&mut self.reads.lock().unwrap().listed)
    }

    /// 開過的讀取（照順序），並清掉紀錄。
    fn take_opened(&self) -> Vec<String> {
        std::mem::take(&mut self.reads.lock().unwrap().opened)
    }

    fn most_open(&self) -> usize {
        self.reads.lock().unwrap().most_open
    }

    fn put(&mut self, rel: &str, content: Vec<u8>, mtime_ns: i64, etag: Option<&str>) {
        let rel = rel.as_bytes().to_vec();
        self.mtimes.insert(rel.clone(), mtime_ns);
        if let Some(e) = etag {
            self.etags.insert(rel.clone(), e.as_bytes().to_vec());
        } else {
            self.etags.remove(&rel);
        }
        self.files.insert(rel, content);
    }

    /// 只改 mtime（etag 不動）：模擬「來源說檔案變了、內容其實沒變」。
    fn touch(&mut self, rel: &str, mtime_ns: i64) {
        self.mtimes.insert(rel.as_bytes().to_vec(), mtime_ns);
    }

    fn content(&self, rel: &str) -> Vec<u8> {
        self.files.get(rel.as_bytes()).cloned().unwrap()
    }

    fn file_item(&self, meta_key: &[u8], name: Vec<u8>, content: &[u8]) -> SourceItem {
        SourceItem {
            kind: SourceItemKind::File {
                size: content.len() as u64,
                mtime_ns: *self.mtimes.get(meta_key).unwrap_or(&0),
                etag: self.etags.get(meta_key).cloned(),
                vern: None,
            },
            name,
        }
    }

    fn items_at(&self, dir: &[u8]) -> Vec<SourceItem> {
        let mut prefix: Vec<u8> = dir.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let mut dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut files: Vec<SourceItem> = Vec::new();
        for (rel, content) in &self.files {
            let Some(rest) = rel.strip_prefix(prefix.as_slice()) else {
                continue;
            };
            match rest.iter().position(|&b| b == b'/') {
                Some(i) => {
                    dirs.insert(rest[..i].to_vec());
                }
                // metadata 以**完整 rel** 為鍵；節點名是最後元件。
                None => files.push(self.file_item(rel, rest.to_vec(), content)),
            }
        }
        for name in dirs {
            files.push(SourceItem {
                kind: SourceItemKind::Dir,
                name,
            });
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        files
    }
}

impl Source for FakeSource {
    fn locator(&self) -> &[u8] {
        &self.locator
    }

    fn meta_kind(&self) -> u8 {
        self.mk
    }

    fn list(
        &self,
        dir: &[u8],
    ) -> Result<Box<dyn kist_backend::source::SortedItems + Send>, BackendError> {
        struct Items {
            iter: std::vec::IntoIter<SourceItem>,
        }
        impl kist_backend::source::SortedItems for Items {
            fn next_item(&mut self) -> Option<Result<SourceItem, BackendError>> {
                self.iter.next().map(Ok)
            }
        }
        self.reads
            .lock()
            .unwrap()
            .listed
            .push(String::from_utf8_lossy(dir).into_owned());
        if self.unlistable.contains(dir) {
            return Err(BackendError::Sftp(format!(
                "cannot list {:?}: permission denied",
                String::from_utf8_lossy(dir)
            )));
        }
        Ok(Box::new(Items {
            iter: self.items_at(dir).into_iter(),
        }))
    }

    fn read(&self, file: &[u8]) -> Result<Box<dyn Read + Send>, BackendError> {
        // 檔案來源契約：根物件以 rel = ""（來源根本身）讀取。
        let key: &[u8] = if file.is_empty() {
            self.locator.rsplit(|&b| b == b'/').next().unwrap_or(file)
        } else {
            file
        };
        match self.files.get(key) {
            Some(content) => {
                let mut reads = self.reads.lock().unwrap();
                reads
                    .opened
                    .push(String::from_utf8_lossy(file).into_owned());
                reads.open_now += 1;
                reads.most_open = reads.most_open.max(reads.open_now);
                Ok(Box::new(Counted {
                    inner: std::io::Cursor::new(content.clone()),
                    reads: Arc::clone(&self.reads),
                }))
            }
            None => Err(BackendError::NotFound(
                String::from_utf8_lossy(file).into_owned(),
            )),
        }
    }

    fn read_ahead(&self) -> usize {
        self.ahead
    }
}

/// 注入一個來源副本：locator bytes 同時當 `Root.path`。
fn injected(src: &FakeSource) -> SourceSpec {
    SourceSpec::Injected(Arc::new(src.clone()), src.locator.clone())
}

fn source_opts(source: SourceSpec) -> BackupOptions {
    BackupOptions {
        source,
        ..backup_options()
    }
}

/// 設檔案的 mtime（奈秒）。
fn set_mtime(path: &Path, mtime_ns: i64) {
    let ft = filetime::FileTime::from_unix_time(
        mtime_ns.div_euclid(1_000_000_000),
        mtime_ns.rem_euclid(1_000_000_000) as u32,
    );
    filetime::set_file_times(path, ft, ft).unwrap();
}

/// 一個 snapshot 的 roots（v4 的 roots 在一個 metadata item 裡）。
async fn roots_of(repo: &kist_core::v4::Repo, snapshot: &str) -> Vec<Root> {
    let snap = repo.read_snapshot(&skey(snapshot)).await.unwrap();
    let reader = repo.reader(None).await.unwrap();
    reader.roots(&snap.roots).await.unwrap()
}

/// 比對兩棵樹的檔案清單、內容與**檔案**的 mtime。目錄的時間不比：遠端來源
/// 沒有目錄時間（缺席＝未知），restore 也不該假造一個。
fn assert_files_match(a: &Path, b: &Path) {
    let sa = snapshot_dir(a);
    let sb = snapshot_dir(b);
    assert_eq!(sa, sb, "檔案清單或內容不同");
    for (k, v) in &sa {
        if v == b"<dir>" {
            continue;
        }
        let ma = std::fs::symlink_metadata(a.join(k)).unwrap();
        let mb = std::fs::symlink_metadata(b.join(k)).unwrap();
        assert_eq!(
            filetime::FileTime::from_last_modification_time(&ma),
            filetime::FileTime::from_last_modification_time(&mb),
            "mtime 不同：{}",
            k.display()
        );
    }
}

#[tokio::test]
async fn backup_from_memory_source_restores() {
    let t = TestRepo::new().await;
    let mut src = FakeSource::new("mem://bucket/data", meta_kind::S3);
    src.put(
        "alpha.txt",
        b"hello".to_vec(),
        1_600_000_000_000_000_000,
        Some("etag-alpha"),
    );
    src.put(
        "big.bin",
        random_bytes(3, 300 * 1024),
        1_600_000_000_100_000_000,
        Some("etag-big"),
    );
    src.put(
        "sub/nested.txt",
        b"nested".to_vec(),
        1_600_000_000_200_000_000,
        Some("etag-nested"),
    );
    let repo = t.open().await;
    let summary = repo.backup(&[], source_opts(injected(&src))).await.unwrap();
    assert_eq!(summary.stats.files, 3, "{:?}", summary.stats);
    assert_eq!(summary.stats.dirs, 1, "{:?}", summary.stats);
    assert_eq!(summary.stats.symlinks, 0, "{:?}", summary.stats);
    assert_eq!(summary.report.errors, 0, "{:?}", summary.report);

    // Root.path = locator bytes（§9：遠端來源是不透明定位）。
    let roots = roots_of(&repo, &summary.snapshot).await;
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].path.as_slice(), b"mem://bucket/data");

    // 還原映射：目錄來源的內容落在 `target/<locator 去掉 scheme>/` 底下。
    let target = t.dir.path().join("out");
    repo.restore(&skey(&summary.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join("bucket/data");

    // 本機鏡像比對：清單、內容、檔案 mtime（s3 檔案的 mtime 會還原）。
    let mirror = t.dir.path().join("mirror");
    std::fs::create_dir_all(mirror.join("sub")).unwrap();
    std::fs::write(mirror.join("alpha.txt"), b"hello").unwrap();
    std::fs::write(mirror.join("big.bin"), src.content("big.bin")).unwrap();
    std::fs::write(mirror.join("sub/nested.txt"), b"nested").unwrap();
    set_mtime(&mirror.join("alpha.txt"), 1_600_000_000_000_000_000);
    set_mtime(&mirror.join("big.bin"), 1_600_000_000_100_000_000);
    set_mtime(&mirror.join("sub/nested.txt"), 1_600_000_000_200_000_000);
    assert_files_match(&mirror, &restored);
}

/// §8.2 的 s3 快速路徑：mtime 變了（甚至晚於 parent snapshot 的開始時間——
/// posix 的 racy guard 一定會擋）、etag 不變 → 沿用 chunk 清單、不重讀。
#[tokio::test]
async fn etag_fast_path_reuses_chunks_without_rereading() {
    let t = TestRepo::new().await;
    let mut src = FakeSource::new("mem://bucket/data", meta_kind::S3);
    src.put(
        "f1.bin",
        random_bytes(1, 300 * 1024),
        1_000_000_000_000_000_000,
        Some("etag-1"),
    );
    src.put(
        "f2.txt",
        b"small".to_vec(),
        1_000_000_000_100_000_000,
        Some("etag-2"),
    );
    let repo = t.open().await;
    let first = repo.backup(&[], source_opts(injected(&src))).await.unwrap();
    assert_eq!(first.report.files_reused, 0, "{:?}", first.report);

    // mtime 推到遙遠的未來：etag 是內容指紋，沒有時間 guard。
    let mut src2 = src.clone();
    src2.touch("f1.bin", 2_500_000_000_000_000_000);
    src2.touch("f2.txt", 2_500_000_000_100_000_000);
    let second = repo
        .backup(&[], source_opts(injected(&src2)))
        .await
        .unwrap();
    assert_eq!(second.report.files_reused, 2, "{:?}", second.report);
    assert_eq!(second.report.chunks_new, 0, "{:?}", second.report);
    assert_eq!(second.report.packs_new, 0, "{:?}", second.report);
    assert_eq!(
        second.parent.as_deref(),
        Some(first.snapshot.as_str()),
        "同 locator 的 snapshot 要被當 parent"
    );
}

/// §8.2：sftp 沒有安全快速路徑（mtime/size 都是來源聲稱的）——mtime 變了
/// 就重讀，靠 chunk 去重吸收，不寫新 pack。sftp 樹（目錄記 generic、檔案帶
/// mtime）也要通過讀取端的驗證。
#[tokio::test]
async fn sftp_semantics_never_reuse() {
    let t = TestRepo::new().await;
    let mut src = FakeSource::new("mem://box/data", meta_kind::SFTP);
    src.put(
        "f1.bin",
        random_bytes(2, 300 * 1024),
        1_000_000_000_000_000_000,
        None,
    );
    let repo = t.open().await;
    let first = repo.backup(&[], source_opts(injected(&src))).await.unwrap();
    assert_eq!(first.report.files_reused, 0, "{:?}", first.report);

    let mut src2 = src.clone();
    src2.touch("f1.bin", 2_500_000_000_000_000_000);
    let second = repo
        .backup(&[], source_opts(injected(&src2)))
        .await
        .unwrap();
    assert_eq!(second.report.files_reused, 0, "{:?}", second.report);
    assert_eq!(second.report.chunks_new, 0, "內容沒變，去重要吸收");
    assert_eq!(second.report.packs_new, 0, "{:?}", second.report);

    let target = t.dir.path().join("out");
    repo.restore(&skey(&second.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(target.join("box/data/f1.bin")).unwrap(),
        src.content("f1.bin")
    );
}

/// 遠端檔案 root：定位指向單一檔案時 root tree 只有那個 entry，restore 落在
/// `target/<locator 去掉末段>/<name>`——與本機檔案 root 對稱（§9）。
#[tokio::test]
async fn remote_file_root_backup_restores_to_locator_parent() {
    let t = TestRepo::new().await;
    let mut src = FakeSource::new("mem://bucket/data/only.bin", meta_kind::S3);
    src.put(
        "only.bin",
        random_bytes(4, 64 * 1024),
        1_600_000_000_000_000_000,
        Some("etag-only"),
    );
    let repo = t.open().await;
    let summary = repo.backup(&[], source_opts(injected(&src))).await.unwrap();
    assert_eq!(summary.stats.files, 1, "{:?}", summary.stats);
    assert_eq!(summary.stats.dirs, 0, "{:?}", summary.stats);

    let target = t.dir.path().join("out");
    repo.restore(&skey(&summary.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join("bucket/data/only.bin");
    assert_eq!(std::fs::read(&restored).unwrap(), src.content("only.bin"));
    let m = std::fs::symlink_metadata(&restored).unwrap();
    assert_eq!(
        filetime::FileTime::from_last_modification_time(&m).unix_seconds(),
        1_600_000_000,
        "檔案 mtime 要還原"
    );
}

/// 同一條 locator 換了來源種類（s3 → sftp）：parent 的 etag 證明對不上
/// 現在的 sftp 語意 → 不可沿用（內容相同時去重仍然吸收）。
#[tokio::test]
async fn same_locator_with_different_source_kind_never_reuses() {
    let t = TestRepo::new().await;
    let mut s3 = FakeSource::new("mem://bucket/data", meta_kind::S3);
    let content = random_bytes(5, 100 * 1024);
    s3.put(
        "f.bin",
        content.clone(),
        1_000_000_000_000_000_000,
        Some("etag-1"),
    );
    let repo = t.open().await;
    let first = repo.backup(&[], source_opts(injected(&s3))).await.unwrap();
    assert_eq!(first.report.files_reused, 0, "{:?}", first.report);

    let mut sftp = FakeSource::new("mem://bucket/data", meta_kind::SFTP);
    sftp.put("f.bin", content, 1_000_000_000_000_000_000, None);
    let second = repo
        .backup(&[], source_opts(injected(&sftp)))
        .await
        .unwrap();
    assert_eq!(second.report.files_reused, 0, "{:?}", second.report);
    assert_eq!(second.report.chunks_new, 0, "內容相同，去重要吸收");
}

/// `SourceSpec::Url` 的 paths 必須正好是那個 URL（單一 root）；多餘或缺少
/// 的路徑都是使用錯誤，在開連線**之前**就拒絕。
#[tokio::test]
async fn url_source_requires_exactly_that_path() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let err = repo
        .backup(
            &[],
            source_opts(SourceSpec::Url("sftp://nowhere.invalid/data".to_owned())),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("exactly one root"),
        "unexpected error: {err}"
    );
}

/// `Url` 也可以指向本機路徑（`open_source` 開出 `LocalSource`）：整個 backup
/// 只有這一個 root，`Root.path` 是 canonicalize 後的絕對路徑。
#[tokio::test]
async fn url_source_works_for_a_local_path() {
    let t = TestRepo::new().await;
    let src = t.dir.path().join("src");
    make_source(&src);
    let repo = t.open().await;
    let url = src.to_str().unwrap().to_owned();
    let summary = repo
        .backup(
            std::slice::from_ref(&src),
            source_opts(SourceSpec::Url(url)),
        )
        .await
        .unwrap();

    let roots = roots_of(&repo, &summary.snapshot).await;
    let expected = std::fs::canonicalize(&src).unwrap();
    assert_eq!(
        roots[0].path.as_slice(),
        kist_core::fsmeta::path_to_bytes(&expected).unwrap(),
        "Root.path = canonicalized locator"
    );

    let target = t.dir.path().join("out");
    repo.restore(&skey(&summary.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}

/// 遠端來源的排除樣式比對「還原落點」的路徑：定位 `mem://bucket/data` 切段成
/// `/bucket/data`，錨定樣式要從那裡寫起（ADR 020）。
#[tokio::test]
async fn excludes_match_a_remote_root_by_its_restore_path() {
    let t = TestRepo::new().await;
    let mut src = FakeSource::new("mem://bucket/data", meta_kind::S3);
    for rel in [
        "keep.txt",
        "skip/x.bin",
        "logs/a.log",
        "logs/keep.txt",
        "data/skip/y",
    ] {
        src.put(
            rel,
            rel.as_bytes().to_vec(),
            1_600_000_000_000_000_000,
            Some(rel),
        );
    }
    let repo = t.open().await;
    let opts = BackupOptions {
        exclude: kist_core::Exclusions {
            patterns: kist_core::PathFilter::new(&["/bucket/data/skip", "*.log"]).unwrap(),
            ..kist_core::Exclusions::default()
        },
        ..source_opts(injected(&src))
    };
    let s = repo.backup(&[], opts).await.unwrap();
    assert_eq!(s.report.excluded, 2, "{:?}", s.report);

    let target = t.dir.path().join("out");
    let r = repo
        .restore(&skey(&s.snapshot), &target, RestoreOptions::default())
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let base = target.join("bucket/data");
    assert!(base.join("keep.txt").is_file());
    assert!(base.join("logs/keep.txt").is_file());
    assert!(
        base.join("data/skip/y").is_file(),
        "錨定樣式不比對更深處的同名目錄"
    );
    assert!(!base.join("skip").exists());
    assert!(!base.join("logs/a.log").exists());
}

/// `one_file_system` 看的是本機的裝置號碼，遠端來源沒有：開始前就拒絕，不默默忽略。
#[tokio::test]
async fn one_file_system_is_refused_for_remote_sources() {
    let t = TestRepo::new().await;
    let mut src = FakeSource::new("mem://bucket/data", meta_kind::S3);
    src.put("a", b"a".to_vec(), 1_600_000_000_000_000_000, Some("a"));
    let repo = t.open().await;
    let opts = BackupOptions {
        exclude: kist_core::Exclusions {
            one_file_system: true,
            ..kist_core::Exclusions::default()
        },
        ..source_opts(injected(&src))
    };
    let err = repo.backup(&[], opts).await.unwrap_err();
    assert!(err.to_string().contains("one-file-system"), "{err}");
    assert_eq!(t.count("snapshots"), 0);
}

// ---- SFTP 的大小與 mtime 快速路徑（選用；M12 第 7 段之三第八步，修改 ADR 016 的分級）----

const NS: i64 = 1_000_000_000;
const DAY: i64 = 86_400;
/// 這些測試的起點（Unix 秒，2026-09-21）。
const T0: i64 = 1_790_000_000;

/// 牆鐘固定在 `secs`（Unix 秒）的 backup 選項；`trust` 開著 SFTP 的快速路徑（一週整份讀一次）。
fn at(source: SourceSpec, secs: i64, trust: bool) -> BackupOptions {
    let wall = time::OffsetDateTime::from_unix_timestamp(secs).unwrap();
    BackupOptions {
        source,
        clock: kist_core::v4::Clock::new(move || kist_core::v4::Reading {
            wall,
            boot: std::time::Duration::from_secs(1_000_000),
        }),
        sftp_trust_mtime: trust.then_some(kist_core::v4::DEFAULT_REREAD_EVERY),
        ..backup_options()
    }
}

async fn trusting(
    repo: &kist_core::v4::Repo,
    src: &FakeSource,
    secs: i64,
) -> kist_core::v4::BackupSummary {
    repo.backup(&[], at(injected(src), secs, true))
        .await
        .unwrap()
}

fn trust_of(s: &kist_core::v4::BackupSummary) -> kist_core::v4::MtimeTrust {
    s.mtime_trust.clone().unwrap()
}

/// snapshot 裡 `rel` 的內容（還原到一個新目錄再讀）。
async fn restored(repo: &kist_core::v4::Repo, snapshot: &str, rel: &str) -> Vec<u8> {
    let target = tempfile::tempdir().unwrap();
    repo.restore(&skey(snapshot), target.path(), RestoreOptions::default())
        .await
        .unwrap();
    std::fs::read(target.path().join("box/data").join(rel)).unwrap()
}

fn sftp_source() -> FakeSource {
    let mut src = FakeSource::new("mem://box/data", meta_kind::SFTP);
    src.put("a.bin", random_bytes(3, 200 * 1024), (T0 - DAY) * NS, None);
    src.put("d/b.bin", random_bytes(4, 50 * 1024), (T0 - DAY) * NS, None);
    src
}

/// 整份讀過一次之後，大小與 mtime 都沒變的檔案沿用、不讀；mtime 或大小變了的照讀。
#[tokio::test]
async fn trusted_sftp_mtime_skips_unchanged_files_after_a_full_read() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let mut src = sftp_source();
    let first = trusting(&repo, &src, T0).await;
    assert_eq!(trust_of(&first).reason, Some("no parent snapshot"));
    assert_eq!(first.report.files_reused, 0);

    let second = trusting(&repo, &src, T0 + DAY).await;
    let trust = trust_of(&second);
    assert!(trust.trusted, "{trust:?}");
    assert_eq!(trust.last_full_read, Some(T0 * NS));
    assert_eq!(second.report.files_reused, 2, "{:?}", second.report);

    src.touch("a.bin", (T0 + DAY) * NS);
    src.put("d/b.bin", random_bytes(5, 60 * 1024), (T0 - DAY) * NS, None);
    let third = trusting(&repo, &src, T0 + 2 * DAY).await;
    assert!(trust_of(&third).trusted);
    assert_eq!(third.report.files_reused, 0, "{:?}", third.report);
    assert_eq!(
        restored(&repo, &third.snapshot, "d/b.bin").await,
        src.content("d/b.bin")
    );
}

/// SFTP 的 mtime 是秒：mtime 那一秒沒有在 parent 開始之前結束的檔案（parent 讀它的同一秒裡可能又被改過）照讀；
/// 剛好在 parent 開始前一秒的沿用。
#[tokio::test]
async fn trusted_sftp_mtime_rereads_a_file_from_the_parents_start_second() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let mut src = sftp_source();
    src.put("racy.bin", random_bytes(6, 10 * 1024), T0 * NS, None);
    src.put("edge.bin", random_bytes(7, 10 * 1024), (T0 - 1) * NS, None);
    trusting(&repo, &src, T0).await;
    let second = trusting(&repo, &src, T0 + DAY).await;
    assert!(trust_of(&second).trusted);
    // a.bin、d/b.bin、edge.bin 沿用；racy.bin 重讀
    assert_eq!(second.report.files_reused, 3, "{:?}", second.report);
}

/// 至少每 `reread_every` 整份讀一次：信任的那幾次不更新紀錄，紀錄太舊就整份讀，之後又從那次算。
#[tokio::test]
async fn trusted_sftp_mtime_reads_everything_again_after_reread_every() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = sftp_source();
    trusting(&repo, &src, T0).await;
    for day in [1, 6] {
        let s = trusting(&repo, &src, T0 + day * DAY).await;
        let trust = trust_of(&s);
        assert!(trust.trusted, "day {day}: {trust:?}");
        assert_eq!(trust.last_full_read, Some(T0 * NS), "day {day}");
        assert_eq!(s.report.files_reused, 2, "day {day}");
    }
    let full = trusting(&repo, &src, T0 + 8 * DAY).await;
    assert_eq!(
        trust_of(&full).reason,
        Some("the last full read is too old")
    );
    assert_eq!(full.report.files_reused, 0, "{:?}", full.report);
    let after = trusting(&repo, &src, T0 + 8 * DAY + 3600).await;
    assert_eq!(trust_of(&after).last_full_read, Some((T0 + 8 * DAY) * NS));
    assert_eq!(after.report.files_reused, 2);
}

/// 接受的風險（文件化的限制，不是 bug）：內容變了、大小與 mtime 都沒變的檔案，信任的那幾次沿用舊的內容；下一次
/// 整份讀才備份到新的。
#[tokio::test]
async fn trusted_sftp_mtime_misses_a_same_size_same_mtime_change_until_the_next_full_read() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let mut src = sftp_source();
    let before = src.content("a.bin");
    trusting(&repo, &src, T0).await;
    src.put("a.bin", random_bytes(8, 200 * 1024), (T0 - DAY) * NS, None);
    let trusted = trusting(&repo, &src, T0 + DAY).await;
    assert!(trust_of(&trusted).trusted);
    assert_eq!(restored(&repo, &trusted.snapshot, "a.bin").await, before);
    let full = trusting(&repo, &src, T0 + 8 * DAY).await;
    assert!(!trust_of(&full).trusted);
    assert_eq!(
        restored(&repo, &full.snapshot, "a.bin").await,
        src.content("a.bin")
    );
}

/// 沒有本機快取（`--no-cache`）就沒有整份讀的紀錄：每次都整份讀。
#[tokio::test]
async fn trusted_sftp_mtime_without_a_cache_reads_everything() {
    let t = TestRepo::new().await;
    let repo = t.open_uncached().await;
    let src = sftp_source();
    trusting(&repo, &src, T0).await;
    let second = trusting(&repo, &src, T0 + DAY).await;
    assert_eq!(trust_of(&second).reason, Some("no local cache"));
    assert_eq!(second.report.files_reused, 0);
}

/// parent 比上一次整份讀還舊（forget 掉了比較新的 snapshot）：parent 那一串不一定在 `reread_every` 之內整份讀過，
/// 所以整份讀。沒開快速路徑的 backup 也是整份讀，一樣記下。
#[tokio::test]
async fn trusted_sftp_mtime_needs_a_parent_no_older_than_the_last_full_read() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = sftp_source();
    trusting(&repo, &src, T0).await;
    let plain = repo
        .backup(&[], at(injected(&src), T0 + DAY, false))
        .await
        .unwrap();
    assert_eq!(plain.mtime_trust, None);
    repo.forget(kist_core::v4::ForgetOptions {
        snapshots: vec![plain.snapshot.clone()],
        retire: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let third = trusting(&repo, &src, T0 + 2 * DAY).await;
    let trust = trust_of(&third);
    assert_eq!(
        trust.reason,
        Some("the parent snapshot is older than the last full read")
    );
    assert_eq!(trust.last_full_read, Some((T0 + DAY) * NS));
    assert_eq!(third.report.files_reused, 0);
}

/// 紀錄比這次開始還晚（時鐘往回調過）當成沒有紀錄。
#[tokio::test]
async fn trusted_sftp_mtime_ignores_a_full_read_recorded_in_the_future() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let src = sftp_source();
    trusting(&repo, &src, T0 + 10 * DAY).await;
    let back = trusting(&repo, &src, T0).await;
    assert_eq!(
        trust_of(&back).reason,
        Some("the recorded full read is later than now")
    );
    assert_eq!(back.report.files_reused, 0);
}

/// 只有 SFTP 來源有這個快速路徑：別的來源開了是用法錯誤，不是默默沒作用。
#[tokio::test]
async fn trusting_sftp_mtime_is_refused_for_other_sources() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let mut src = FakeSource::new("mem://bucket/data", meta_kind::S3);
    src.put("a.bin", b"a".to_vec(), T0 * NS, Some("e1"));
    let err = repo
        .backup(&[], at(injected(&src), T0, true))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("sftp:// sources only"), "{err}");
}

// ---- 遠端來源在一個目錄裡預讀（M12 第 7 段之三第九步）----

/// 遠端來源（`read_ahead` > 0）在一個目錄裡同時開著正在處理的這個加上 `read_ahead` 個讀取；本機來源（0）一次一個。
/// 每個檔案只讀一次，讀回的內容相同。
#[tokio::test]
async fn remote_sources_read_ahead_within_a_directory() {
    for ahead in [0, 4] {
        let t = TestRepo::new().await;
        let repo = t.open().await;
        let mut src = FakeSource::new("mem://box/data", meta_kind::SFTP);
        src.ahead = ahead;
        for i in 0..10 {
            src.put(&format!("f{i}"), random_bytes(10 + i, 3000), T0 * NS, None);
        }
        let s = repo.backup(&[], source_opts(injected(&src))).await.unwrap();
        assert_eq!(src.most_open(), 1 + ahead, "read_ahead {ahead}");
        let opened = src.take_opened();
        assert_eq!(opened.len(), 10, "read_ahead {ahead}: {opened:?}");
        assert_eq!(
            opened.iter().collect::<BTreeSet<_>>().len(),
            10,
            "each file once: {opened:?}"
        );
        assert_eq!(restored(&repo, &s.snapshot, "f7").await, src.content("f7"));
    }
}

/// 遠端 root 只列一次：`plan_roots` 為了分辨 root 是檔案還是目錄已經列了根、取走第一項，walk 接著用那一份
/// （原本再列一次：100 萬項的平面 root 就是兩次完整的列舉）。第一項是檔案、是目錄都一樣，第一項不會漏掉。
#[tokio::test]
async fn remote_root_is_listed_once() {
    for (names, listed) in [
        (
            &["a", "b", "sub/x", "sub/deeper/y"][..],
            &["", "sub", "sub/deeper"][..],
        ),
        (&["0/x", "a"][..], &["", "0"][..]),
    ] {
        let t = TestRepo::new().await;
        let repo = t.open().await;
        let mut src = FakeSource::new("mem://box/data", meta_kind::S3);
        for (i, name) in names.iter().enumerate() {
            src.put(name, random_bytes(30 + i as u64, 100), T0 * NS, Some(name));
        }
        let s = repo.backup(&[], source_opts(injected(&src))).await.unwrap();
        assert_eq!(src.take_listed(), listed);
        for name in names {
            assert_eq!(
                restored(&repo, &s.snapshot, name).await,
                src.content(name),
                "{name}"
            );
        }
    }
}

/// 列不了的遠端 root 讓 backup 失敗（不是記成一個空的 root）；列不了的子目錄是略過的條目，backup 照樣寫。
#[tokio::test]
async fn unlistable_remote_root_fails_the_backup() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let mut src = FakeSource::new("mem://box/data", meta_kind::SFTP);
    src.put("a", random_bytes(40, 100), T0 * NS, None);
    src.put("sub/x", random_bytes(41, 100), T0 * NS, None);
    src.unlistable.insert(b"sub".to_vec());
    let s = repo.backup(&[], source_opts(injected(&src))).await.unwrap();
    assert_eq!(s.report.errors, 1);
    assert_eq!(restored(&repo, &s.snapshot, "a").await, src.content("a"));
    src.unlistable.insert(Vec::new());
    let err = repo
        .backup(&[], source_opts(injected(&src)))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot list \"\""), "{err}");
}

/// 預讀不猜：排除的、快速路徑沿用的（這裡是 s3 的 etag 與大小）、比 1 MiB 大的檔案不先讀；遇到目錄就不往後看，
/// 子目錄走完才讀它後面的檔案。
#[tokio::test]
async fn read_ahead_reads_only_files_that_will_be_read() {
    let t = TestRepo::new().await;
    let repo = t.open().await;
    let mut src = FakeSource::new("mem://box/data", meta_kind::S3);
    src.ahead = 4;
    for name in ["a", "b", "c", "skip.tmp", "sub/x", "z"] {
        src.put(name, random_bytes(20, 1000), T0 * NS, Some(name));
    }
    src.put(
        "big",
        random_bytes(21, 1024 * 1024 + 1),
        T0 * NS,
        Some("big"),
    );
    // 注入的是來源的 clone：改了來源之後要重新建選項。
    let opts = |src: &FakeSource| BackupOptions {
        exclude: kist_core::Exclusions::new(
            kist_core::PathFilter::new(&["*.tmp".to_owned()]).unwrap(),
            &[],
            false,
            None,
            false,
        )
        .unwrap(),
        ..source_opts(injected(src))
    };
    repo.backup(&[], opts(&src)).await.unwrap();
    // 處理 a 的時候：b、big、c 已經判斷過，b 與 c 先開始讀（big 太大），sub 是目錄就停；z 要等 sub/x 讀完。
    assert_eq!(src.take_opened(), ["b", "c", "a", "big", "sub/x", "z"]);

    src.put("c", random_bytes(22, 1000), T0 * NS, Some("c2"));
    let second = repo.backup(&[], opts(&src)).await.unwrap();
    assert_eq!(src.take_opened(), ["c"], "only the changed file is read");
    assert_eq!(second.report.files_reused, 5, "{:?}", second.report);
}
