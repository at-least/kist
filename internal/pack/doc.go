// Package pack reads and writes pack files, the only place chunk data is
// stored in a repository.
//
// A pack is a self-describing container. Its layout is:
//
//	[sealed chunk]*        each: nonce(24) || AEAD(algorithm(1) || payload)
//	sealed trailer         AEAD over canonical CBOR: [][id, offset, length]
//	trailer length         8 bytes, big endian
//	magic                  8 bytes, "kistpk" || format version
//
// Reading it backwards from the magic is what makes a pack recoverable on
// its own: the trailer says where every chunk in the file starts and how
// long it is, so the repository index is a cache that can always be
// rebuilt by reading pack tails.
//
// Packs are named by the unkeyed BLAKE3 of their whole ciphertext. That
// choice has three consequences worth stating: a pack's integrity can be
// checked without holding any repository key, two clients that build an
// identical pack deduplicate it for free, and a writer cannot know the
// name until the last byte is written -- so packs are spooled to a local
// file, never assembled in memory.
//
// Packs target 64 MiB and are flushed when full or when a backup ends.
package pack
