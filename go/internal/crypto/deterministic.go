package crypto

import (
	"io"

	"lukechampine.com/blake3"
)

// DeterministicReader returns an infinite, reproducible byte stream keyed
// by seed. Different seeds give unrelated streams.
//
// It exists so that golden files of *encrypted* objects are possible:
// every sealed object carries a random nonce, so without a fixed source
// of randomness no encrypted object has a stable encoding to record. Pass
// one of these as the nonceSource of Seal in tests, and nil -- meaning
// crypto/rand -- everywhere else.
//
// It is not a CSPRNG substitute and must never be used to generate real
// key material.
func DeterministicReader(seed string) io.Reader {
	h := blake3.New(IDSize, nil)
	// hash.Hash forbids Write from returning an error, and blake3.Hasher
	// honours that; errcheck cannot know it, hence the exemption.
	_, _ = io.WriteString(h, "kist/test/"+seed) //nolint:errcheck // hash.Hash.Write never fails
	return h.XOF()
}
