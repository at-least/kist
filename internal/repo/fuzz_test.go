package repo

import (
	"testing"

	"github.com/at-least/kist/internal/crypto"
)

// FuzzDecodeRecords: the two small GC objects and the config, which is
// the one plaintext object and so the one an attacker can hand the
// decoder without a key.
func FuzzDecodeRecords(f *testing.F) {
	for _, v := range []any{
		gcMark{Version: gcVersion, MarkedNs: 1, By: "x"},
		clientRecord{Version: gcVersion, FirstSeenNs: 1},
	} {
		seed, err := crypto.Marshal(v)
		if err != nil {
			f.Fatal(err)
		}
		f.Add(seed)
	}
	f.Add([]byte{0xa0})
	f.Add([]byte{})
	f.Fuzz(func(t *testing.T, data []byte) {
		// Decoding may fail; it may not panic, and what it accepts must
		// re-encode.
		var m gcMark
		if err := crypto.Unmarshal(data, &m); err == nil {
			if _, err := crypto.Marshal(m); err != nil {
				t.Fatalf("a decoded mark failed to encode: %v", err)
			}
		}
		var c clientRecord
		if err := crypto.Unmarshal(data, &c); err == nil {
			if _, err := crypto.Marshal(c); err != nil {
				t.Fatalf("a decoded client record failed to encode: %v", err)
			}
		}
		var cfg Config
		if err := crypto.Unmarshal(data, &cfg); err == nil {
			if _, err := crypto.Marshal(&cfg); err != nil {
				t.Fatalf("a decoded config failed to encode: %v", err)
			}
		}
	})
}
