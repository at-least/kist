// Package backend defines the storage abstraction every kist repository
// sits on: a flat namespace of immutable objects.
//
// The contract a backend must honour:
//
//   - An object that can be read is complete. A partial write must never
//     become visible under its final name, at any point, including after
//     a crash. This is what lets a repository be checked without a lock.
//   - Objects are never modified in place. The only mutation is Delete,
//     and only maintenance operations perform it.
//   - PutIfAbsent either stores the object or reports ErrExists. It is
//     the conditional write that lets snapshots commit, and clients
//     deduplicate uploads, without a repository-wide lock.
//
// Backends see ciphertext and key names only. Encryption, naming and
// integrity are the caller's concern; a backend never interprets the
// bytes it stores.
//
// Implementations land in milestone order: local filesystem (M1), S3 (M2),
// SFTP (M4).
package backend
