package repo

import (
	"io/fs"

	"github.com/at-least/kist/internal/tree"
)

// fillOwnership is a no-op on Windows: there is no uid/gid to record, and
// the ACLs that take their place are not something M1 represents. A tree
// entry written here simply has no owner, which restores as "owned by
// whoever ran the restore" -- the honest answer.
func fillOwnership(*tree.Entry, fs.FileInfo) {}

// hardLinkOf reports no hard links on Windows. NTFS has them, but the
// file index that identifies one is not exposed through fs.FileInfo, so
// M1 stores linked files as separate copies rather than guessing.
func hardLinkOf(fs.FileInfo) (hardLinkKey, uint64, bool) { return hardLinkKey{}, 0, false }
