package crypto

import (
	"strings"
	"testing"
)

func TestIDStringParseRoundTrip(t *testing.T) {
	keys, err := DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	id := ContentID(&keys.Hash, []byte("round trip"))

	parsed, err := ParseID(id.String())
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if parsed != id {
		t.Errorf("parsed = %s, want %s", parsed, id)
	}
	if len(id.String()) != 2*IDSize {
		t.Errorf("string length = %d, want %d", len(id.String()), 2*IDSize)
	}
}

func TestParseIDRejectsMalformed(t *testing.T) {
	valid := strings.Repeat("ab", IDSize)

	cases := map[string]string{
		"empty":       "",
		"too short":   valid[:len(valid)-2],
		"too long":    valid + "cd",
		"not hex":     strings.Repeat("zz", IDSize),
		"upper case ": strings.ToUpper(valid)[:len(valid)-1] + "!",
	}
	for name, in := range cases {
		t.Run(name, func(t *testing.T) {
			if _, err := ParseID(in); err == nil {
				t.Fatalf("parse %q: got nil error", in)
			}
		})
	}
}

// Keying the hash is what stops an observer confirming a guess about
// file contents from the names in the repository.
func TestContentIDDependsOnTheHashKey(t *testing.T) {
	a, err := DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	other := goldenMaster
	other[0] ^= 0xff
	b, err := DeriveKeys(other, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}

	payload := []byte("guessable contents")
	if ContentID(&a.Hash, payload) == ContentID(&b.Hash, payload) {
		t.Fatal("content IDs do not depend on the hash key")
	}
	if ContentID(&a.Hash, payload) == CiphertextID(payload) {
		t.Fatal("keyed content ID equals the unkeyed hash")
	}
}

func TestCiphertextHasherMatchesCiphertextID(t *testing.T) {
	payload := []byte("streamed in pieces")

	h := CiphertextHasher()
	for _, part := range [][]byte{payload[:5], payload[5:9], payload[9:]} {
		if _, err := h.Write(part); err != nil {
			t.Fatalf("write: %v", err)
		}
	}
	var streamed ID
	copy(streamed[:], h.Sum(nil))

	if streamed != CiphertextID(payload) {
		t.Errorf("streamed = %s, want %s", streamed, CiphertextID(payload))
	}
}

func TestZeroIDIsRecognised(t *testing.T) {
	var zero ID
	if !zero.IsZero() {
		t.Error("zero ID does not report IsZero")
	}
	if (ID{1}).IsZero() {
		t.Error("non-zero ID reports IsZero")
	}
}
