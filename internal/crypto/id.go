package crypto

import (
	"encoding/hex"
	"fmt"

	"lukechampine.com/blake3"
)

// IDSize is the length of a content address in bytes.
//
// IDs are never truncated. A 32-byte ID is the whole BLAKE3 output; the
// index keeps memory in bounds by its own structure, not by shortening a
// content address in a format that is about to be frozen.
const IDSize = 32

// An ID is the content address of a chunk or a tree: BLAKE3 in keyed mode
// over the plaintext, under the repository's hash key.
//
// Keying the hash means an attacker who can guess a file's contents still
// cannot confirm the guess by looking at the names in the repository.
type ID [IDSize]byte

// String renders the ID as lowercase hex, which is also how it appears in
// repository keys.
func (id ID) String() string { return hex.EncodeToString(id[:]) }

// IsZero reports whether the ID is the zero value, which is never a valid
// content address.
func (id ID) IsZero() bool { return id == ID{} }

// ParseID decodes the hex form produced by ID.String.
func ParseID(s string) (ID, error) {
	var id ID
	if len(s) != hex.EncodedLen(IDSize) {
		return id, fmt.Errorf("parse id %q: want %d hex digits, got %d", s, hex.EncodedLen(IDSize), len(s))
	}
	if _, err := hex.Decode(id[:], []byte(s)); err != nil {
		return id, fmt.Errorf("parse id %q: %w", s, err)
	}
	return id, nil
}

// ContentID is the keyed content address of plaintext under the hash key.
// It is what names a chunk and what names a tree.
func ContentID(hashKey *Key, plaintext []byte) ID {
	h := blake3.New(IDSize, hashKey[:])
	// hash.Hash forbids Write from returning an error, and blake3.Hasher
	// honours that; errcheck cannot know it, hence the exemption.
	_, _ = h.Write(plaintext) //nolint:errcheck // hash.Hash.Write never fails

	var id ID
	copy(id[:], h.Sum(nil))
	return id
}

// CiphertextID is the unkeyed BLAKE3 of a stored object's bytes. Packs and
// index blobs are named this way so that integrity can be checked, and
// uploads deduplicated, without holding the repository key.
func CiphertextID(ciphertext []byte) ID { return ID(blake3.Sum256(ciphertext)) }

// CiphertextHasher returns a hasher matching CiphertextID, for naming an
// object that is streamed rather than held in memory.
func CiphertextHasher() *blake3.Hasher { return blake3.New(IDSize, nil) }
