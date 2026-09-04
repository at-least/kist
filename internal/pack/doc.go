// Package pack reads and writes pack files, the only place chunk data is
// stored in a repository.
//
// Layout of packs/<hash>:
//
//	[encrypted chunk]*
//	encrypted trailer index
//	trailer length (8 bytes)
//	magic
//
// Packs target 64 MiB and are flushed when full or when a backup ends.
// The trailer makes each pack self-describing: the repository index is a
// cache that can always be rebuilt by reading pack trailers.
package pack
