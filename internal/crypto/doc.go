// Package crypto owns everything a kist repository trusts: the key
// hierarchy, the authenticated encryption envelope, content addressing,
// and the canonical CBOR encoding that content addressing depends on.
//
// The hierarchy is password -> Argon2id -> KEK -> master key, with
// per-purpose subkeys (chunk, hash, index, meta) derived from the master
// key by HKDF-SHA256 salted with the repository ID. Payloads are sealed
// with XChaCha20-Poly1305 under a 24-byte nonce, with a caller-supplied
// AAD that binds each object to where it belongs.
//
// Canonical CBOR lives here rather than in a package of its own because
// it is an integrity property, not a serialisation convenience: a tree
// object is named by the hash of its encoding, so an encoder that emits
// two different byte strings for the same value would break
// deduplication and, with it, the format.
//
// This package composes primitives from x/crypto, crypto/hkdf and
// lukechampine.com/blake3. It implements no primitive of its own.
package crypto
