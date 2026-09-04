package crypto

import (
	"crypto/cipher"
	"crypto/rand"
	"errors"
	"fmt"
	"io"

	"golang.org/x/crypto/chacha20poly1305"
)

const (
	// KeySize is the length of every symmetric key in the hierarchy.
	KeySize = chacha20poly1305.KeySize

	// NonceSize is the XChaCha20-Poly1305 nonce length. At 24 bytes,
	// random nonces are safe for any number of messages a backup tool
	// will ever produce.
	NonceSize = chacha20poly1305.NonceSizeX

	// TagSize is the Poly1305 authentication tag length.
	TagSize = chacha20poly1305.Overhead

	// Overhead is how much longer a sealed message is than its plaintext.
	Overhead = NonceSize + TagSize
)

// A Key is a symmetric key: the master key or one of its subkeys.
type Key [KeySize]byte

// ErrDecrypt is returned when a sealed message does not authenticate. It
// is deliberately opaque: whether the key, the AAD or the ciphertext is
// wrong is not something a caller should be able to distinguish.
var ErrDecrypt = errors.New("decrypt: message failed authentication")

// RandomKey returns a fresh key read from the process CSPRNG.
func RandomKey() (Key, error) {
	var k Key
	if _, err := io.ReadFull(rand.Reader, k[:]); err != nil {
		return k, fmt.Errorf("generate key: %w", err)
	}
	return k, nil
}

// Seal encrypts plaintext under key, binding it to aad, and returns
// nonce || ciphertext || tag.
//
// nonceSource supplies the 24 random bytes of the nonce. Production
// callers pass nil, which means crypto/rand; tests pass a fixed reader,
// which is what makes byte-exact golden files of encrypted objects
// possible.
func Seal(key *Key, aad, plaintext []byte, nonceSource io.Reader) ([]byte, error) {
	aead, err := newAEAD(key)
	if err != nil {
		return nil, err
	}
	if nonceSource == nil {
		nonceSource = rand.Reader
	}

	out := make([]byte, NonceSize, NonceSize+len(plaintext)+TagSize)
	if _, err := io.ReadFull(nonceSource, out[:NonceSize]); err != nil {
		return nil, fmt.Errorf("read nonce: %w", err)
	}

	return aead.Seal(out, out[:NonceSize], plaintext, aad), nil
}

// Open reverses Seal. It returns ErrDecrypt if the message does not
// authenticate under key and aad.
func Open(key *Key, aad, sealed []byte) ([]byte, error) {
	aead, err := newAEAD(key)
	if err != nil {
		return nil, err
	}
	if len(sealed) < Overhead {
		return nil, fmt.Errorf("%w: message is %d bytes, shorter than the %d-byte envelope", ErrDecrypt, len(sealed), Overhead)
	}

	plaintext, err := aead.Open(nil, sealed[:NonceSize], sealed[NonceSize:], aad)
	if err != nil {
		return nil, ErrDecrypt
	}
	return plaintext, nil
}

// SealedSize is the length Seal will return for a plaintext of n bytes.
func SealedSize(n int) int { return n + Overhead }

func newAEAD(key *Key) (cipher.AEAD, error) {
	aead, err := chacha20poly1305.NewX(key[:])
	if err != nil {
		return nil, fmt.Errorf("init XChaCha20-Poly1305: %w", err)
	}
	return aead, nil
}
