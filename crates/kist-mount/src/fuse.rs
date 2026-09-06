//! fuser 0.18 的轉接層：把 FUSE callback 翻成 [`FsCore`] 呼叫。邏輯都在
//! `corefs`，這裡只有 errno 對映、`FileAttr` 組裝與 TTL 選擇。
//!
//! 執行模型：fuser 的 callback 是同步 `&self`（多條 event loop 併發呼叫），
//! 非同步工作用共用的 tokio runtime `Handle::block_on` 完成——fuser 執行緒
//! 會 park，runtime workers 才是真的跑任務的地方（worker 數要 > fuser 執行緒數，
//! 見 [`crate::mount`]）。鎖的紀律：[`FsCore`] 內部鎖不跨 `.await`。

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileType, Filesystem, FopenFlags, Generation, INodeNo, OpenAccMode, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyXattr, Request,
};
use tokio::runtime::Handle;

use crate::corefs::{Attr, FsCore, FsError, Kind};

pub struct KistFs {
    pub(crate) core: FsCore,
    pub(crate) rt: Handle,
}

fn system_time(ns: i64) -> SystemTime {
    if ns <= 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    }
}

fn file_type(kind: Kind) -> FileType {
    match kind {
        Kind::File => FileType::RegularFile,
        Kind::Dir => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
    }
}

fn file_attr(ino: u64, a: &Attr) -> FileAttr {
    let mtime = system_time(a.mtime_ns);
    let ctime = system_time(a.ctime_ns);
    FileAttr {
        ino: INodeNo(ino),
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime,
        crtime: ctime,
        kind: file_type(a.kind),
        perm: (a.perm & 0o7777) as u16,
        // mount 只服務 bytes 與 modes，不做 inode 身分：硬連結各自成檔、nlink=1
        nlink: 1,
        uid: a.uid,
        gid: a.gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

fn err(e: FsError) -> Errno {
    match e {
        FsError::NotFound => Errno::ENOENT,
        FsError::Io => Errno::EIO,
        FsError::InvalidInput => Errno::EINVAL,
        FsError::BadHandle => Errno::EBADF,
    }
}

impl Filesystem for KistFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self
            .rt
            .block_on(self.core.lookup(parent.0, name.as_bytes()))
        {
            Ok(l) => reply.entry(&l.ttl, &file_attr(l.ino, &l.attr), Generation(0)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        match self.core.getattr(ino.0) {
            Ok((attr, ttl)) => reply.attr(&ttl, &file_attr(ino.0, &attr)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<fuser::FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.core.readlink(ino.0) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(err(e)),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _link: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn open(&self, _req: &Request, _ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        reply.opened(fuser::FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        match self.rt.block_on(self.core.read_file(ino.0, offset, size)) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(err(e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        // 唯讀：close 時沒有髒資料要沖，一律成功
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.rt.block_on(self.core.opendir(ino.0)) {
            Ok(fh) => reply.opened(fuser::FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let _ = ino;
        let page = match self.core.readdir(fh.0, offset, 1024) {
            Ok(p) => p,
            Err(e) => {
                reply.error(err(e));
                return;
            }
        };
        for (i, entry) in page.into_iter().enumerate() {
            // kernel 的 offset 慣例：這一筆的「下一個位置」= offset + 1 + i
            let next = offset.saturating_add(1).saturating_add(i as u64);
            let full = reply.add(
                INodeNo(0),
                next,
                file_type(entry.kind),
                OsStr::from_bytes(&entry.name),
            );
            if full {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: fuser::FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.core.releasedir(fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // 唯讀橋接：空間與檔案數沒有意義，全給 0
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 512);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self.core.get_xattr(ino.0, name.as_bytes()) {
            Ok(Some(value)) => {
                if (size as usize) >= value.len() {
                    reply.data(&value)
                } else {
                    reply.size(value.len() as u32) // kernel 對 caller 回 ERANGE
                }
            }
            Ok(None) => reply.error(Errno::ENODATA),
            Err(e) => reply.error(err(e)),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.core.list_xattrs(ino.0) {
            Ok(names) => {
                let needed: usize = names.iter().map(|n| n.len() + 1).sum();
                if (size as usize) >= needed {
                    let mut buf = Vec::with_capacity(needed);
                    for n in &names {
                        buf.extend_from_slice(n);
                        buf.push(0);
                    }
                    reply.data(&buf)
                } else {
                    reply.size(needed as u32)
                }
            }
            Err(e) => reply.error(err(e)),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }
}
