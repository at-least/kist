package snapshot

import (
	"testing"
	"time"

	"github.com/at-least/kist/internal/crypto"
)

// FuzzDecode: arbitrary bytes into the snapshot decoder; no panics, and
// what validates round-trips.
func FuzzDecode(f *testing.F) {
	valid := &Snapshot{
		Version: Version, Root: crypto.ID{1}, TimeNs: time.Date(2026, 1, 2, 3, 4, 5, 6, time.UTC).UnixNano(),
		Host: "h", Paths: []string{"/a"}, ClientID: "00112233445566778899aabbccddeeff",
		Stats: Stats{Files: 1, Bytes: 2},
	}
	seed, err := crypto.Marshal(valid)
	if err != nil {
		f.Fatal(err)
	}
	f.Add(seed)
	f.Add([]byte{0xa0})
	f.Add([]byte{})
	f.Fuzz(func(t *testing.T, data []byte) {
		var s Snapshot
		if err := crypto.Unmarshal(data, &s); err != nil {
			return
		}
		if err := s.validate(); err != nil {
			return
		}
		again, err := crypto.Marshal(&s)
		if err != nil {
			t.Fatalf("a valid snapshot failed to encode: %v", err)
		}
		var s2 Snapshot
		if err := crypto.Unmarshal(again, &s2); err != nil {
			t.Fatalf("re-encoded snapshot does not decode: %v", err)
		}
	})
}
