//! restore 用的目錄 handle（ADR 019 A4）：每一層持有一個已驗證的目錄
//! handle，建目錄、建暫存檔、rename、硬連結、symlink 都**相對於它**做，
//! 不再以完整路徑重新解析。
//!
//! 以前逐段 lstat 檢查完就以完整路徑開檔，O_NOFOLLOW 只管最末段：檢查之後
//! 中間某一層被換成外指 symlink，寫入就跟著它到目標之外（實測 2000 個檔有
//! 1999 個）。現在寫入一律落在已開的那個目錄——它之後被搬到哪、原位換成
//! 什麼都一樣。
//!
//! - unix：rustix 的 `*at` 系列（safe 封裝，kist 維持 `forbid(unsafe_code)`）。
//!   往下一層一律 `openat(O_DIRECTORY|O_NOFOLLOW)`：擋路的是 symlink 或其他
//!   非目錄就開不起來，不跟隨。
//! - 非 unix：沒有 `*at` 系列，維持以前的兩步檢查（lstat 看過再以完整路徑
//!   操作），中間段的競態仍在。
//!
//! 同時開著的 handle 數以 tree 深度為上限（每層一個，上限見
//! [`crate::MAX_TREE_DEPTH`]），與 entry 數無關：硬連結表記的是相對路徑，
//! 不是 handle（見 [`DirHandle::open_rel`]）。

use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;

use crate::{CoreError, Result};

/// 目標之下的一層目錄。clone 只多一份 `Arc`，不多開 fd。
#[derive(Clone)]
pub(crate) struct DirHandle {
    /// 已開的目錄（unix：`O_RDONLY|O_DIRECTORY`，目標之下的每一層另加
    /// `O_NOFOLLOW`）。
    #[cfg(unix)]
    file: Arc<File>,
    /// 這層目錄的完整路徑。unix 只拿來寫錯誤訊息；非 unix 拿來做實際操作。
    path: PathBuf,
    /// 相對於還原目標的路徑元件（目標本身是空的）。硬連結表記這個，之後從
    /// 目標的 handle 沿它重新逐層開（[`DirHandle::open_rel`]）。
    rel: Vec<OsString>,
}

/// 名字處現有的東西是什麼（不跟隨 symlink）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    /// FIFO、socket、裝置。
    Special,
}

impl DirHandle {
    /// 這層目錄的完整路徑（錯誤訊息用）。
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 這層目錄裡 `name` 的完整路徑（錯誤訊息用）。
    pub(crate) fn child_path(&self, name: &OsStr) -> PathBuf {
        self.path.join(name)
    }

    /// 這層目錄裡 `name` 相對於還原目標的路徑元件（硬連結表用）。
    pub(crate) fn child_rel(&self, name: &OsStr) -> Vec<OsString> {
        let mut rel = self.rel.clone();
        rel.push(name.to_os_string());
        rel
    }

    /// 在這層之下建立（已存在就沿用）子目錄 `rel` 的每一段，回傳最後一層。
    /// 只收一般的路徑元件：`..`、根、Windows 的磁碟代號都代表路徑不在目標
    /// 之下。
    pub(crate) fn create_dirs(&self, rel: &Path) -> Result<DirHandle> {
        let mut dir = self.clone();
        for comp in rel.components() {
            let std::path::Component::Normal(name) = comp else {
                return Err(CoreError::Corrupt {
                    key: self.path.join(rel).display().to_string(),
                    reason: format!(
                        "restore path is not under the target {}",
                        self.path.display()
                    ),
                });
            };
            // 往下走時放掉上一層：同時只開著兩個 handle。
            dir = dir.create_child_dir(name)?;
        }
        Ok(dir)
    }

    /// 從還原目標 `self` 沿 `rel`（目標之下的路徑元件）逐層開到最後一層，
    /// 不建目錄（硬連結的第一個名字已經還原過，路徑上的目錄都該在）。途中
    /// 任何一層被換成 symlink 或非目錄就開不起來，由呼叫端退回複製。往下走
    /// 時隨即放掉上一層，所以硬連結表記的是路徑、不是 handle：fd 用量與
    /// entry 數無關。
    pub(crate) fn open_rel(&self, rel: &[OsString]) -> Result<DirHandle> {
        let mut dir = self.clone();
        for name in rel {
            dir = dir.open_child_dir(name)?;
        }
        Ok(dir)
    }

    /// 回報「`name` 處擋著一個 symlink／非目錄」。unix 開目錄失敗之後才呼叫，
    /// 那時只為了挑訊息再看一眼；看不到就當非目錄。這是本機的擋路物，不是
    /// repo 損壞（ADR 019 A30）。
    fn in_the_way_of_dir(&self, name: &OsStr) -> CoreError {
        let what = match self.kind_of(name) {
            Ok(Some(Kind::Symlink)) => "a symlink",
            _ => "a non-directory",
        };
        CoreError::RestoreBlocked {
            path: self.child_path(name),
            reason: format!("{what} is in the way of a restored directory"),
        }
    }

    /// 內部：往下一層的 handle。
    #[cfg(unix)]
    fn child(&self, name: &OsStr, file: File) -> DirHandle {
        DirHandle {
            file: Arc::new(file),
            path: self.child_path(name),
            rel: self.child_rel(name),
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::path::Path;
    use std::sync::Arc;

    use rustix::fs::{AtFlags, FileType, Gid, Mode, Nsecs, OFlags, Timespec, Timestamps, Uid};
    use rustix::io::Errno;

    use super::{DirHandle, Kind};
    use crate::{CoreError, Result};

    /// 往下一層目錄的開法：唯讀（之後在同一個 handle 上套目錄的 metadata，
    /// ADR 019 A3）、必須是目錄、最末段不跟隨 symlink。
    fn dir_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    impl DirHandle {
        /// 開還原目標本身。`target` 與其之上是使用者自己的路徑，照常跟隨
        /// symlink；目標之下的每一層才不跟隨。
        pub(crate) fn open_target(target: &Path) -> Result<DirHandle> {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            let fd = rustix::fs::open(target, flags, Mode::empty())
                .map_err(|e| CoreError::io(target, e.into()))?;
            Ok(DirHandle {
                file: Arc::new(File::from(fd)),
                path: target.to_path_buf(),
                rel: Vec::new(),
            })
        }

        /// 已開的目錄本身：目錄的 metadata 在這個 handle 上套（ADR 019 A3），
        /// 目錄之後被搬走、原位換成 symlink 也套不到別處。
        pub(crate) fn meta_handle(&self) -> Result<Arc<File>> {
            Ok(Arc::clone(&self.file))
        }

        /// 建立（已存在就沿用）子目錄 `name` 並開它。`mkdirat` 遇到既有的
        /// 名字（目錄、symlink 或其他）一律 EEXIST、不跟隨；是不是真目錄由
        /// 接著的 `openat(O_DIRECTORY|O_NOFOLLOW)` 決定。
        pub(crate) fn create_child_dir(&self, name: &OsStr) -> Result<DirHandle> {
            match rustix::fs::mkdirat(&*self.file, name, Mode::from_raw_mode(0o777)) {
                Ok(()) => {}
                Err(e) if e == Errno::EXIST => {}
                Err(e) => return Err(CoreError::io(self.child_path(name), e.into())),
            }
            self.open_child_dir(name)
        }

        /// 開既有的子目錄 `name`，不跟隨 symlink。擋路的是 symlink 時 Linux
        /// 回 ENOTDIR（有 O_DIRECTORY 時；實測），其他 unix 可能回 ELOOP：兩個
        /// 都回報成擋路。
        pub(crate) fn open_child_dir(&self, name: &OsStr) -> Result<DirHandle> {
            match rustix::fs::openat(&*self.file, name, dir_flags(), Mode::empty()) {
                Ok(fd) => Ok(self.child(name, File::from(fd))),
                Err(e) if e == Errno::NOTDIR || e == Errno::LOOP => {
                    Err(self.in_the_way_of_dir(name))
                }
                Err(e) => Err(CoreError::io(self.child_path(name), e.into())),
            }
        }

        /// `name` 處現有的東西（不跟隨 symlink）；沒有就是 `None`。
        pub(crate) fn kind_of(&self, name: &OsStr) -> Result<Option<Kind>> {
            let stat = match rustix::fs::statat(&*self.file, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(e) if e == Errno::NOENT => return Ok(None),
                Err(e) => return Err(CoreError::io(self.child_path(name), e.into())),
            };
            Ok(Some(match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => Kind::File,
                FileType::Directory => Kind::Dir,
                FileType::Symlink => Kind::Symlink,
                _ => Kind::Special,
            }))
        }

        /// 新建檔案 `name` 來寫：O_CREAT|O_EXCL（名字已被佔用——含 symlink——
        /// 就失敗，不會打開別人放的東西）加 O_NOFOLLOW。權限 0o666 再經 umask，
        /// 與 std 的預設相同。
        pub(crate) fn create_new_file(&self, name: &OsStr) -> std::io::Result<File> {
            let flags =
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let fd = rustix::fs::openat(&*self.file, name, flags, Mode::from_raw_mode(0o666))?;
            Ok(File::from(fd))
        }

        /// 同一層裡把 `from` 改名成 `to`（`to` 已存在就取代它；rename 不跟隨
        /// symlink）。
        pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
            Ok(rustix::fs::renameat(&*self.file, from, &*self.file, to)?)
        }

        /// 刪掉這層裡的檔案或 symlink `name`（不跟隨）。
        pub(crate) fn remove_file(&self, name: &OsStr) -> std::io::Result<()> {
            Ok(rustix::fs::unlinkat(&*self.file, name, AtFlags::empty())?)
        }

        /// 在 `dst` 那一層建 `new_name`，硬連結到這層的 `name`。旗標是空的：
        /// `name` 是 symlink 時連到 symlink 本身，不跟隨。
        pub(crate) fn hard_link(
            &self,
            name: &OsStr,
            dst: &DirHandle,
            new_name: &OsStr,
        ) -> std::io::Result<()> {
            Ok(rustix::fs::linkat(
                &*self.file,
                name,
                &*dst.file,
                new_name,
                AtFlags::empty(),
            )?)
        }

        /// 在這層建 symlink `name` → `target`（`target` 原樣寫進連結，不解析）。
        pub(crate) fn symlink(&self, target: &Path, name: &OsStr) -> std::io::Result<()> {
            Ok(rustix::fs::symlinkat(target, &*self.file, name)?)
        }

        /// 這層的 `name` 與 `other` 那層的 `other_name`（都不跟隨 symlink）是不是
        /// 同一個 inode。任一邊看不到就當不同。
        pub(crate) fn same_file(
            &self,
            name: &OsStr,
            other: &DirHandle,
            other_name: &OsStr,
        ) -> bool {
            let a = rustix::fs::statat(&*self.file, name, AtFlags::SYMLINK_NOFOLLOW);
            let b = rustix::fs::statat(&*other.file, other_name, AtFlags::SYMLINK_NOFOLLOW);
            match (a, b) {
                (Ok(a), Ok(b)) => a.st_dev == b.st_dev && a.st_ino == b.st_ino,
                _ => false,
            }
        }

        /// 設 symlink `name` 本身的時間（atime＝mtime），不跟隨。有些平台不支援
        /// 設 symlink 本身的時間；失敗不算錯（與以前以路徑設時一樣）。
        pub(crate) fn set_symlink_mtime(&self, name: &OsStr, mtime_ns: i64) {
            // rem_euclid 落在 0..1e9，u32 放得下（與 fsmeta 的換算相同，1970 年
            // 前的負值照實）。
            let nanos = mtime_ns.rem_euclid(1_000_000_000) as u32;
            let at = Timespec {
                tv_sec: mtime_ns.div_euclid(1_000_000_000),
                tv_nsec: Nsecs::from(nanos),
            };
            let times = Timestamps {
                last_access: at,
                last_modification: at,
            };
            let _ = rustix::fs::utimensat(&*self.file, name, &times, AtFlags::SYMLINK_NOFOLLOW);
        }

        /// 設 symlink `name` 本身的擁有者，不跟隨（fchownat＋AT_SYMLINK_NOFOLLOW，
        /// ADR 019 A43）。只在以 root 還原時呼叫。
        pub(crate) fn chown_symlink(
            &self,
            name: &OsStr,
            uid: u32,
            gid: u32,
        ) -> std::io::Result<()> {
            // -1 先擋掉：它對 chown 是「不改」，Uid::from_raw 在 debug 下也會 panic。
            crate::fsmeta::check_owner_ids(uid, gid)?;
            Ok(rustix::fs::chownat(
                &*self.file,
                name,
                Some(Uid::from_raw(uid)),
                Some(Gid::from_raw(gid)),
                AtFlags::SYMLINK_NOFOLLOW,
            )?)
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::path::Path;
    use std::sync::Arc;

    use super::{DirHandle, Kind};
    use crate::{CoreError, Result};

    impl DirHandle {
        /// 還原目標本身。非 unix 不開 handle，只記路徑。
        pub(crate) fn open_target(target: &Path) -> Result<DirHandle> {
            Ok(DirHandle {
                path: target.to_path_buf(),
                rel: Vec::new(),
            })
        }

        /// Windows：與以前 filetime 以路徑設目錄時間時開 handle 的方式相同（寫入權、
        /// FILE_FLAG_BACKUP_SEMANTICS 才開得了目錄；filetime 0.2.29 windows.rs 的
        /// `open`），行為不變：會跟隨 reparse point。
        #[cfg(windows)]
        pub(crate) fn meta_handle(&self) -> Result<Arc<File>> {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&self.path)
                .map(Arc::new)
                .map_err(|e| CoreError::io(&self.path, e))
        }

        /// 以前的兩步檢查（非 unix 沒有 `*at` 系列）：lstat 看一眼，沒有就建，
        /// 建的時候撞到同名的東西再看一眼；symlink 或非目錄都拒絕。檢查之後
        /// 路徑被換掉的競態仍在。
        pub(crate) fn create_child_dir(&self, name: &OsStr) -> Result<DirHandle> {
            let path = self.child_path(name);
            match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&path) {
                        Ok(()) => {}
                        // 同一路徑可能在另一個 root 已經建好：只要它是真目錄就放行。
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(CoreError::io(&path, e)),
                    }
                }
                Err(e) => return Err(CoreError::io(&path, e)),
                Ok(_) => {}
            }
            self.open_child_dir(name)
        }

        /// 既有的子目錄 `name`：lstat 是真目錄才放行。
        pub(crate) fn open_child_dir(&self, name: &OsStr) -> Result<DirHandle> {
            match self.kind_of(name)? {
                Some(Kind::Dir) => Ok(DirHandle {
                    path: self.child_path(name),
                    rel: self.child_rel(name),
                }),
                Some(_) => Err(self.in_the_way_of_dir(name)),
                None => Err(CoreError::io(
                    self.child_path(name),
                    std::io::ErrorKind::NotFound.into(),
                )),
            }
        }

        pub(crate) fn kind_of(&self, name: &OsStr) -> Result<Option<Kind>> {
            let path = self.child_path(name);
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_symlink() => Ok(Some(Kind::Symlink)),
                Ok(m) if m.is_dir() => Ok(Some(Kind::Dir)),
                Ok(m) if m.is_file() => Ok(Some(Kind::File)),
                Ok(_) => Ok(Some(Kind::Special)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(CoreError::io(&path, e)),
            }
        }

        /// `create_new`＝O_CREAT|O_EXCL：名字已被佔用（含 symlink）就失敗。
        pub(crate) fn create_new_file(&self, name: &OsStr) -> std::io::Result<File> {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.child_path(name))
        }

        pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
            std::fs::rename(self.child_path(from), self.child_path(to))
        }

        pub(crate) fn remove_file(&self, name: &OsStr) -> std::io::Result<()> {
            std::fs::remove_file(self.child_path(name))
        }

        pub(crate) fn hard_link(
            &self,
            name: &OsStr,
            dst: &DirHandle,
            new_name: &OsStr,
        ) -> std::io::Result<()> {
            std::fs::hard_link(self.child_path(name), dst.child_path(new_name))
        }

        #[cfg(windows)]
        pub(crate) fn symlink(&self, target: &Path, name: &OsStr) -> std::io::Result<()> {
            std::os::windows::fs::symlink_file(target, self.child_path(name))
        }

        /// 非 unix 的 std 沒有穩定的 inode 比對；當成不同，照常 link＋rename。
        pub(crate) fn same_file(&self, _: &OsStr, _: &DirHandle, _: &OsStr) -> bool {
            false
        }

        /// 以路徑設 symlink 本身的時間，不跟隨；有些平台不支援，失敗不算錯。
        pub(crate) fn set_symlink_mtime(&self, name: &OsStr, mtime_ns: i64) {
            let mtime = crate::fsmeta::file_time(mtime_ns);
            let _ = filetime::set_symlink_file_times(self.child_path(name), mtime, mtime);
        }

        /// 非 unix 不還原擁有者（[`crate::fsmeta::running_as_root`] 一律是 false，
        /// 不會呼叫到這裡）。
        pub(crate) fn chown_symlink(&self, _: &OsStr, _: u32, _: u32) -> std::io::Result<()> {
            Ok(())
        }
    }
}

/// ADR 019 A4 的原語：handle 開好之後，原路徑被換成指向「外面」（同一個
/// tempdir 裡、目標之外的目錄）的 symlink，經 handle 做的每一種寫入都要落在
/// 被搬開的那個真目錄，外面永遠是空的。
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::ffi::OsStr;
    use std::io::Write;
    use std::path::Path;

    use super::DirHandle;

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// tempdir 之下：`out/`（還原目標）、`outside/`（目標之外）。回傳
    /// (tempdir, 目標, 外面)。
    fn layout() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        (dir, target, outside)
    }

    #[test]
    fn writes_through_a_handle_land_in_the_moved_dir_not_the_symlink_target() {
        let (_dir, target, outside) = layout();
        let root = DirHandle::open_target(&target).unwrap();
        let real = root.create_child_dir(OsStr::new("real")).unwrap();

        // 攻擊者：真目錄搬到旁邊（仍在目標之內），原位換成外指 symlink。
        let moved = target.join("real.moved");
        std::fs::rename(target.join("real"), &moved).unwrap();
        std::os::unix::fs::symlink(&outside, target.join("real")).unwrap();

        let mut f = real.create_new_file(OsStr::new(".tmp")).unwrap();
        f.write_all(b"content").unwrap();
        drop(f);
        real.rename(OsStr::new(".tmp"), OsStr::new("f")).unwrap();
        real.hard_link(OsStr::new("f"), &real, OsStr::new("g"))
            .unwrap();
        real.symlink(Path::new("f"), OsStr::new("s")).unwrap();
        real.set_symlink_mtime(OsStr::new("s"), 1_000_000 * 1_000_000_000);
        let sub = real.create_child_dir(OsStr::new("sub")).unwrap();
        drop(sub.create_new_file(OsStr::new("inner")).unwrap());
        assert!(real.same_file(OsStr::new("f"), &real, OsStr::new("g")));

        assert_eq!(names(&outside), Vec::<String>::new(), "寫到目標之外了");
        assert_eq!(names(&moved), ["f", "g", "s", "sub"]);
        assert_eq!(std::fs::read(moved.join("g")).unwrap(), b"content");
        assert_eq!(names(&moved.join("sub")), ["inner"]);
        let s = std::fs::symlink_metadata(moved.join("s")).unwrap();
        assert_eq!(
            filetime::FileTime::from_last_modification_time(&s).unix_seconds(),
            1_000_000
        );
    }

    /// 往下一層遇到 symlink（既有的，或換上的）：拒絕、不跟隨、不在外面建東西。
    #[test]
    fn a_symlink_in_the_way_of_a_child_dir_is_refused() {
        let (_dir, target, outside) = layout();
        std::os::unix::fs::symlink(&outside, target.join("link")).unwrap();
        let root = DirHandle::open_target(&target).unwrap();

        let err = root.create_child_dir(OsStr::new("link")).err().unwrap();
        assert!(
            err.to_string()
                .contains("a symlink is in the way of a restored directory"),
            "{err}"
        );
        let err = root.create_dirs(Path::new("link/deeper")).err().unwrap();
        assert!(err.to_string().contains("symlink is in the way"), "{err}");
        assert_eq!(names(&outside), Vec::<String>::new());
    }

    /// 硬連結表記路徑、到時再從目標逐層開（[`DirHandle::open_rel`]）：中間
    /// 一層被換成外指 symlink 時開不起來，不會開到外面那個同名的目錄。
    #[test]
    fn open_rel_does_not_follow_a_swapped_component() {
        let (_dir, target, outside) = layout();
        std::fs::create_dir_all(outside.join("b")).unwrap();
        let root = DirHandle::open_target(&target).unwrap();
        let b = root.create_dirs(Path::new("a/b")).unwrap();
        assert_eq!(b.child_rel(OsStr::new("f")), ["a", "b", "f"]);
        assert!(root.open_rel(&b.child_rel(OsStr::new("f"))[..2]).is_ok());

        std::fs::rename(target.join("a"), target.join("a.moved")).unwrap();
        std::os::unix::fs::symlink(&outside, target.join("a")).unwrap();

        let err = root
            .open_rel(&b.child_rel(OsStr::new("f"))[..2])
            .err()
            .unwrap();
        assert!(err.to_string().contains("symlink is in the way"), "{err}");
    }
}
