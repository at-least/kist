package repo

import "syscall"

// ctimeNs reads the inode change time. The field is spelled differently
// on each Unix, which is the only reason this is not inline.
func ctimeNs(st *syscall.Stat_t) int64 { return st.Ctim.Sec*1e9 + st.Ctim.Nsec }
