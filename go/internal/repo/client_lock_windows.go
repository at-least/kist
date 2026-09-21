//go:build windows

package repo

import (
	"os"

	"golang.org/x/sys/windows"
)

// lockClientIDFile holds an exclusive lock for the process's lifetime of
// the handle (released on close); see ClientID.
func lockClientIDFile(f *os.File) error {
	return windows.LockFileEx(windows.Handle(f.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &windows.Overlapped{})
}
