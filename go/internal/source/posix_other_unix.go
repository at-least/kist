//go:build unix && !linux && !darwin && !freebsd && !netbsd && !openbsd && !dragonfly

package source

import (
	"io/fs"
	"syscall"
)

// capturePosix reads what this platform's stat record offers. It has no
// portable ctime spelling, so the change time is simply not recorded --
// it is metadata, not content, and a backup does not invent it.
func capturePosix(info fs.FileInfo) PosixMeta {
	meta := PosixMeta{
		Mode:    uint32(info.Mode()),
		MTimeNs: info.ModTime().UnixNano(),
	}
	st, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return meta
	}
	meta.UID = st.Uid
	meta.GID = st.Gid
	meta.Inode = uint64(st.Ino)
	meta.Dev = uint64(st.Dev)
	meta.NLink = uint64(st.Nlink)
	return meta
}
