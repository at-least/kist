// Package snapshot defines the snapshot object, a repository's commit point.
//
// A snapshot records the root tree hash, timestamp, host, source paths and
// statistics. It is written under snapshots/<clientID>/<ts> only after every
// pack and index it references has been durably stored, so a snapshot that
// exists is always complete.
package snapshot
