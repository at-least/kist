//! 以注入的記憶體來源（Source 抽象）測「直接遠端備份」：metadata 依來源種類
//! 記錄（§8 聯集）、快速路徑依 §8.2 分級（s3 的 etag 可沿用、sftp 一律重讀）、
//! 遠端檔案 root 的還原映射與本機檔案 root 對稱（§9）。

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use common::*;
use kist_backend::source::{Source, SourceItem, SourceItemKind};
use kist_backend::BackendError;
use kist_core::{BackupOptions, RestoreOptions, SourceSpec};
use kist_format::tree::meta_kind;

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
}

impl FakeSource {
    fn new(locator: &str, mk: u8) -> Self {
        Self {
            locator: locator.as_bytes().to_vec(),
            mk,
            files: BTreeMap::new(),
            mtimes: BTreeMap::new(),
            etags: BTreeMap::new(),
        }
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
            Some(content) => Ok(Box::new(std::io::Cursor::new(content.clone()))),
            None => Err(BackendError::NotFound(
                String::from_utf8_lossy(file).into_owned(),
            )),
        }
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
    let snap = repo
        .read_snapshot_by_key(&summary.snapshot_key)
        .await
        .unwrap();
    assert_eq!(snap.roots.len(), 1);
    assert_eq!(snap.roots[0].path.as_slice(), b"mem://bucket/data");

    // 還原映射：目錄來源的內容落在 `target/<locator 去掉 scheme>/` 底下。
    let target = t.dir.path().join("out");
    repo.restore(&summary.snapshot_key, &target, RestoreOptions::default())
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
        Some(first.snapshot_key.as_str()),
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
    repo.restore(&second.snapshot_key, &target, RestoreOptions::default())
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
    repo.restore(&summary.snapshot_key, &target, RestoreOptions::default())
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

    let snap = repo
        .read_snapshot_by_key(&summary.snapshot_key)
        .await
        .unwrap();
    let expected = std::fs::canonicalize(&src).unwrap();
    assert_eq!(
        snap.roots[0].path.as_slice(),
        kist_core::fsmeta::path_to_bytes(&expected).unwrap(),
        "Root.path = canonicalized locator"
    );

    let target = t.dir.path().join("out");
    repo.restore(&summary.snapshot_key, &target, RestoreOptions::default())
        .await
        .unwrap();
    let restored = target.join(src.strip_prefix("/").unwrap_or(&src));
    assert_same_tree(&src, &restored);
}
