// Package source abstracts what a backup walks: a local directory, an
// SFTP server or an S3 bucket. The client is a relay -- list, read and
// stream the source through the chunker and the encryption -- so a
// remote source needs nothing on the far end but a filesystem-shaped
// read: list one directory level, read one file.
//
// The metadata an entry carries depends on what its source can PROVE,
// which is the format's metadata union (docs/format.md §8): a posix
// source records the full kernel-backed set, an S3 source records the
// mtime and the etag it computed, an SFTP source records the mtime it
// was told. The fast-path contract (§8.2) grades the same way: posix
// compares ctime/inode (kernel maintained, racy-guarded), s3 compares
// etag + size (a content fingerprint the source vouches for), and
// sftp/generic have no safe fast path at all -- their mtime is an
// unprovable claim, so the chunk dedup absorbs the re-read instead.
package source
