// Package index maps a chunk ID to where its bytes live: which pack, at
// what offset, for how many bytes.
//
// An index is a cache and never a source of truth. Every entry in it can
// be recovered by reading the trailer of the pack it names, which is what
// makes rebuild-index a safe operation and what keeps a repository
// repairable after a client dies mid-backup.
//
// Index blobs are written one per backup run, under indexes/<hash> where
// the hash is the unkeyed BLAKE3 of the blob's ciphertext. Opening a
// repository lists the prefix and merges what it finds. A blob that was
// never written -- because a client crashed after uploading packs but
// before committing -- costs nothing but the space of the orphaned packs,
// which prune reclaims.
package index
