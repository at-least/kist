// Package snapshot defines the snapshot object, a repository's commit
// point.
//
// A snapshot records the root tree, the time, the host and the paths that
// were backed up. It is written under snapshots/<clientID>/<timestamp>
// only after every pack, tree and index blob it depends on is durably
// stored, so a snapshot that exists is always complete and restorable.
// Nothing else in the repository is ordered; this is the one write whose
// position in time matters.
//
// The write is conditional. Two clients that pick the same timestamp must
// not overwrite each other, so a collision retries at the next
// nanosecond rather than replacing what is there.
//
// The timestamp is formatted 20060102T150405.000000000Z: fixed width, so
// it sorts lexically, and free of colons, which are illegal in Windows
// filenames and would make a local repository unusable there.
package snapshot
