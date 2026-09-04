//go:build unix && !linux && !darwin && !freebsd && !netbsd && !openbsd && !dragonfly

package repo

import "syscall"

// ctimeNs has no portable spelling on this platform, so the change time
// is simply not recorded. It is metadata, not content, and a restore does
// not set it in any case.
func ctimeNs(*syscall.Stat_t) int64 { return 0 }
