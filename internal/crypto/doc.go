// Package crypto owns the key hierarchy and the authenticated encryption
// envelope used for every object written to a repository.
//
// The hierarchy is: password -> Argon2id -> KEK -> master key, with
// per-purpose subkeys (chunk key, hash key, index key) derived from the
// master key via HKDF. Chunk payloads are sealed with
// XChaCha20-Poly1305 under a random 24-byte nonce, with the chunk ID as
// additional authenticated data.
//
// This package composes primitives from x/crypto and lukechampine.com/blake3.
// It does not implement any primitive itself.
package crypto
