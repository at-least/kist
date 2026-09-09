//go:build unix

package repo

import (
	"bytes"
	"sort"

	"golang.org/x/sys/unix"

	"github.com/at-least/kist/internal/tree"
)

// readXattrs records a file's extended attributes. Only the user.*
// namespace is stored: trusted.* and security.* need privileges to read
// and are the kernel's business, not a backup's.
//
// Restore does not yet apply them; recording them from day one is the
// point (dropping and re-adding a field later would be a format change).
func readXattrs(path string) tree.Xattrs {
	size, err := unix.Llistxattr(path, nil)
	if err != nil || size <= 0 {
		return nil
	}
	buf := make([]byte, size)
	n, err := unix.Llistxattr(path, buf)
	if err != nil || n <= 0 {
		return nil
	}

	var out tree.Xattrs
	for _, name := range bytes.Split(buf[:n], []byte{0}) {
		if len(name) == 0 || !bytes.HasPrefix(name, []byte("user.")) {
			continue
		}
		vsize, err := unix.Lgetxattr(path, string(name), nil)
		if err != nil || vsize < 0 {
			continue
		}
		value := make([]byte, vsize)
		if _, err := unix.Lgetxattr(path, string(name), value); err != nil {
			continue
		}
		out = append(out, tree.Xattr{Name: name, Value: value})
	}
	if len(out) == 0 {
		return nil
	}
	sort.Slice(out, func(i, j int) bool { return bytes.Compare(out[i].Name, out[j].Name) < 0 })
	return out
}
