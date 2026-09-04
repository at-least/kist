// Package backend defines the storage abstraction every kist repository
// sits on: an immutable, content-addressed key-value namespace.
//
// A backend exposes Put/Get/List/Delete/Stat plus PutIfAbsent, the
// conditional write that lets snapshots commit without a repository-wide
// lock. Implementations land in milestone order: local filesystem, then
// S3-compatible object storage, then SFTP.
//
// Backends see ciphertext only. Encryption, naming and integrity are the
// caller's concern; a backend never interprets the bytes it stores.
package backend
