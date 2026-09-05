package repo

import (
	"testing"

	"github.com/at-least/kist/internal/crypto"
)

// FuzzDecodeRecords: the config and the key slot, the two plaintext
// objects and so the ones an attacker can hand the decoder without a key.
// (A v2 gc mark carries no structure at all: 8 constant bytes.)
func FuzzDecodeRecords(f *testing.F) {
	for _, v := range []any{
		crypto.KeySlot{Version: crypto.KeySlotVersion},
		Config{Version: ConfigVersion},
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
		var slot crypto.KeySlot
		if err := crypto.Unmarshal(data, &slot); err == nil {
			if _, err := crypto.Marshal(slot); err != nil {
				t.Fatalf("a decoded key slot failed to encode: %v", err)
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
