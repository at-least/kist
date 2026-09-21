//go:build unix

package repo

import (
	"os"
	"syscall"
)

// lockClientIDFile holds an exclusive advisory lock for the process's
// lifetime of the handle (released on close); see ClientID.
func lockClientIDFile(f *os.File) error {
	return syscall.Flock(int(f.Fd()), syscall.LOCK_EX)
}
