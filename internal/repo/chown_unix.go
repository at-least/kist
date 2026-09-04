//go:build unix

package repo

import (
	"fmt"
	"os"
)

// chown restores ownership. It fails for an unprivileged restore, which
// the caller turns into a warning: keeping the file is worth more than
// insisting on its owner.
func chown(path string, uid, gid uint32) error {
	if err := os.Lchown(path, int(uid), int(gid)); err != nil {
		return fmt.Errorf("chown %s: %w", path, err)
	}
	return nil
}
