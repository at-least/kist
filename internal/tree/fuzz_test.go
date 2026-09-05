package tree

import (
	"bytes"
	"encoding/hex"
	"os"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/crypto"
)

// FuzzDecode: arbitrary bytes into the tree decoder. It must not panic;
// what it accepts must validate, and must re-encode to the same name.
func FuzzDecode(f *testing.F) {
	text, err := os.ReadFile("testdata/tree.txt")
	if err != nil {
		f.Fatal(err)
	}
	for _, line := range strings.Split(string(text), "\n") {
		if rest, ok := strings.CutPrefix(line, "cbor "); ok {
			data, err := hex.DecodeString(strings.TrimSpace(rest))
			if err != nil {
				f.Fatal(err)
			}
			f.Add(data)
		}
	}
	f.Add([]byte{0xa0})
	f.Add([]byte{})

	var hashKey crypto.Key
	f.Fuzz(func(t *testing.T, data []byte) {
		var tr Tree
		if err := crypto.Unmarshal(data, &tr); err != nil {
			return
		}
		if err := tr.Validate(); err != nil {
			return
		}
		id, encoded, err := tr.Encode(&hashKey)
		if err != nil {
			t.Fatalf("a valid tree failed to encode: %v", err)
		}
		var again Tree
		if err := crypto.Unmarshal(encoded, &again); err != nil {
			t.Fatalf("re-encoded tree does not decode: %v", err)
		}
		id2, _, err := again.Encode(&hashKey)
		if err != nil || id2 != id {
			t.Fatalf("re-encoding changed the name: %s vs %s (%v)", id, id2, err)
		}
		if !bytes.Equal(encoded, mustMarshal(t, &again)) {
			t.Fatal("encoding is not a fixed point")
		}
	})
}

func mustMarshal(t *testing.T, v any) []byte {
	t.Helper()
	b, err := crypto.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return b
}
