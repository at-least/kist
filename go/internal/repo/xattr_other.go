//go:build !unix

package repo

import "github.com/at-least/kist/internal/tree"

// readXattrs reports no extended attributes on platforms without them.
// NTFS alternate data streams would be the analogue; representing them
// is future work, and the entry is simply absent rather than wrong.
func readXattrs(string) tree.Xattrs { return nil }
