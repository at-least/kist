//go:build !unix

package source

import "io/fs"

// capturePosix has no POSIX fields to read on this platform. The mode is
// still recorded, and ownership comes out as 0 -- a real value under the
// posix rules (uid 0 is root), which a restore applies nothing for.
func capturePosix(info fs.FileInfo) PosixMeta {
	return PosixMeta{
		Mode:    uint32(info.Mode()),
		MTimeNs: info.ModTime().UnixNano(),
	}
}
