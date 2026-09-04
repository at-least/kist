// Package tree implements directory objects: content-addressed listings
// of names, metadata and child references.
//
// A tree is named by the keyed BLAKE3 of its canonical CBOR encoding, not
// by the hash of its ciphertext. That is deliberate and load-bearing: a
// ciphertext hash would change on every backup, because every seal draws
// a fresh nonce, and an unchanged directory would get a new name each
// night. Naming by the plaintext is what lets an untouched subtree be
// reused whole, so an incremental backup writes only the trees on the
// path from a changed file up to the root.
//
// Entries are sorted by name before encoding, so one directory has one
// encoding and therefore one name.
package tree
