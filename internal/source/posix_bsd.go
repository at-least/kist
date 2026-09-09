//go:build darwin || freebsd || netbsd || openbsd || dragonfly

package source

import (
	"io/fs"
	"syscall"
)

// capturePosix reads the fields only the kernel's stat record has:
// ownership, the inode change time, and the hard-link identity. The mode
// stays in Go's fs.FileMode encoding, which is what a tree entry stores.
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
	// The conversions look redundant on Linux and are not on the BSDs,
	// where the fields are narrower. A device number is an opaque bit
	// pattern used only as a map key, so a "negative" Dev widening to a
	// large uint64 is harmless.
	meta.CTimeNs = st.Ctimespec.Sec*1e9 + st.Ctimespec.Nsec //nolint:gosec // nanosecond fields cannot overflow an int64 in practice
	meta.Inode = uint64(st.Ino)
	meta.Dev = uint64(st.Dev)
	meta.NLink = uint64(st.Nlink)
	return meta
}
