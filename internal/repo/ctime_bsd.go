//go:build darwin || freebsd || netbsd || openbsd || dragonfly

package repo

import "syscall"

// ctimeNs reads the inode change time. The field is spelled differently
// on each Unix, which is the only reason this is not inline.
func ctimeNs(st *syscall.Stat_t) int64 { return st.Ctimespec.Sec*1e9 + st.Ctimespec.Nsec }
