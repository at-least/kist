//go:build unix

package repo

import (
	"io/fs"
	"syscall"

	"github.com/at-least/kist/internal/tree"
)

// fillOwnership records the owning user and group.
//
// They are stored even when the restoring machine will not be able to
// apply them: a restore running as an ordinary user keeps the file, and a
// later restore running as root can put the ownership back.
func fillOwnership(entry *tree.Entry, info fs.FileInfo) {
	st, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return
	}
	entry.UID = st.Uid
	entry.GID = st.Gid
	entry.CTimeNs = ctimeNs(st)
}

// hardLinkOf identifies the inode behind a path, so that a file reachable
// under several names is stored once.
func hardLinkOf(info fs.FileInfo) (hardLinkKey, uint64, bool) {
	st, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return hardLinkKey{}, 0, false
	}
	// The conversions look redundant on Linux and are not on Darwin,
	// where Dev is int32 and Nlink is uint16. A device number is an
	// opaque bit pattern used only as a map key, so a "negative" Dev
	// widening to a large uint64 is harmless.
	//nolint:unconvert,gosec // widths differ between Unixes; see above
	return hardLinkKey{device: uint64(st.Dev), inode: uint64(st.Ino)}, uint64(st.Nlink), true
}
