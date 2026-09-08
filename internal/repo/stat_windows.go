package repo

import (
	"io/fs"

	"github.com/at-least/kist/internal/tree"
)

// fillOwnership records ownership on Windows as uid/gid 0: v3 posix
// entries carry ownership unconditionally (uid 0 is a real value, not
// "not recorded"), and Windows has nothing better to say -- the ACLs that
// take its place are not something the format represents. A restore
// applies nothing for uid/gid 0, which lands the file with the
// restorer's identity -- the honest answer.
func fillOwnership(entry *tree.Entry, _ fs.FileInfo) {
	entry.UID = tree.Ptr(uint32(0))
	entry.GID = tree.Ptr(uint32(0))
}

// hardLinkOf reports no hard links on Windows. NTFS has them, but the
// file index that identifies one is not exposed through fs.FileInfo, so
// M1 stores linked files as separate copies rather than guessing.
func hardLinkOf(fs.FileInfo) (hardLinkKey, uint64, bool) { return hardLinkKey{}, 0, false }
