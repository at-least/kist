package crypto

import (
	"bytes"
	"errors"
	"io"
	"strings"
	"testing"
)

func testKey(seed byte) Key {
	var k Key
	for i := range k {
		k[i] = seed + byte(i)
	}
	return k
}

func TestSealOpenRoundTrip(t *testing.T) {
	key := testKey(1)
	aad := []byte("kist/v1/test")
	plaintext := []byte("the quick brown fox")

	sealed, err := Seal(&key, aad, plaintext, DeterministicReader("roundtrip"))
	if err != nil {
		t.Fatalf("seal: %v", err)
	}
	if got, want := len(sealed), SealedSize(len(plaintext)); got != want {
		t.Errorf("sealed length = %d, want %d", got, want)
	}
	if bytes.Contains(sealed, plaintext) {
		t.Error("plaintext appears verbatim in the sealed message")
	}

	opened, err := Open(&key, aad, sealed)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	if !bytes.Equal(opened, plaintext) {
		t.Errorf("opened = %q, want %q", opened, plaintext)
	}
}

func TestSealEmptyPlaintext(t *testing.T) {
	key := testKey(2)

	sealed, err := Seal(&key, nil, nil, DeterministicReader("empty"))
	if err != nil {
		t.Fatalf("seal: %v", err)
	}
	opened, err := Open(&key, nil, sealed)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	if len(opened) != 0 {
		t.Errorf("opened = %q, want empty", opened)
	}
}

// A one-byte change anywhere -- key, AAD, nonce, ciphertext or tag --
// must be fatal. This is the property every other integrity claim in the
// repository rests on.
func TestOpenRejectsTampering(t *testing.T) {
	key := testKey(3)
	aad := []byte("kist/v1/test")
	plaintext := bytes.Repeat([]byte("payload"), 16)

	sealed, err := Seal(&key, aad, plaintext, DeterministicReader("tamper"))
	if err != nil {
		t.Fatalf("seal: %v", err)
	}

	flip := func(b []byte, i int) []byte {
		out := bytes.Clone(b)
		out[i] ^= 0x01
		return out
	}

	cases := []struct {
		name string
		key  Key
		aad  []byte
		msg  []byte
	}{
		{"wrong key", testKey(4), aad, sealed},
		{"wrong aad", key, []byte("kist/v1/other"), sealed},
		{"nil aad", key, nil, sealed},
		{"flipped nonce byte", key, aad, flip(sealed, 0)},
		{"flipped ciphertext byte", key, aad, flip(sealed, NonceSize+1)},
		{"flipped tag byte", key, aad, flip(sealed, len(sealed)-1)},
		{"truncated tag", key, aad, sealed[:len(sealed)-1]},
		{"empty message", key, aad, nil},
		{"shorter than the envelope", key, aad, sealed[:Overhead-1]},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			k := tc.key
			if _, err := Open(&k, tc.aad, tc.msg); !errors.Is(err, ErrDecrypt) {
				t.Fatalf("open: err = %v, want ErrDecrypt", err)
			}
		})
	}
}

// Two seals of the same plaintext must differ, or the nonce is being
// reused and XChaCha20-Poly1305 loses all of its guarantees.
func TestSealIsNondeterministicWithRealRandomness(t *testing.T) {
	key := testKey(5)
	plaintext := []byte("same input")

	first, err := Seal(&key, nil, plaintext, nil)
	if err != nil {
		t.Fatalf("seal: %v", err)
	}
	second, err := Seal(&key, nil, plaintext, nil)
	if err != nil {
		t.Fatalf("seal: %v", err)
	}
	if bytes.Equal(first, second) {
		t.Fatal("two seals of one plaintext are identical: the nonce is not random")
	}
}

func TestSealFailsWhenNonceSourceDoes(t *testing.T) {
	key := testKey(6)

	_, err := Seal(&key, nil, []byte("x"), io.LimitReader(strings.NewReader(""), 0))
	if err == nil {
		t.Fatal("seal with an exhausted nonce source: got nil error")
	}
	if !strings.Contains(err.Error(), "nonce") {
		t.Errorf("error = %q, want it to mention the nonce", err)
	}
}
