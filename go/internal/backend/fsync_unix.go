//go:build !windows

package backend

import (
	"fmt"
	"os"
)

// syncDir flushes a directory entry, so that a file that has just been
// linked or renamed into it survives a power loss. Without it, fsync on
// the file itself only guarantees the data, not the name that reaches it.
func syncDir(dir string) error {
	d, err := os.Open(dir) //nolint:gosec // dir is a repository directory the backend just wrote to
	if err != nil {
		return fmt.Errorf("open directory %s: %w", dir, err)
	}
	defer func() { _ = d.Close() }()

	if err := d.Sync(); err != nil {
		return fmt.Errorf("sync directory %s: %w", dir, err)
	}
	return nil
}
