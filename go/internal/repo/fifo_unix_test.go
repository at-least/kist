//go:build unix

package repo

import (
	"fmt"

	"golang.org/x/sys/unix"
)

func makeFIFO(path string) error {
	if err := unix.Mkfifo(path, 0o600); err != nil {
		return fmt.Errorf("mkfifo %s: %w", path, err)
	}
	return nil
}
