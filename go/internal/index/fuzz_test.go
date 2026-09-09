package index

import (
	"testing"

	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
)

// FuzzDecodeBlob: arbitrary bytes into the index blob decoder, then into
// an index. No panics; a blob that decodes must produce lookups that
// agree with its entries.
func FuzzDecodeBlob(f *testing.F) {
	seed, err := crypto.Marshal(blob{Version: Version, Packs: []blobPack{{ID: crypto.ID{1}, Entries: []pack.Entry{{ID: crypto.ID{2}, Offset: 0, Length: 10}}}}})
	if err != nil {
		f.Fatal(err)
	}
	f.Add(seed)
	f.Add([]byte{0xa0})
	f.Add([]byte{})
	f.Fuzz(func(t *testing.T, data []byte) {
		var b blob
		if err := crypto.Unmarshal(data, &b); err != nil {
			return
		}
		if b.Version != Version {
			return
		}
		// A pack listing one chunk twice is what the trailer reader
		// rejects before a blob is ever written from it; the index has
		// no opinion on it, so neither does this test.
		for _, p := range b.Packs {
			seen := map[crypto.ID]struct{}{}
			for _, e := range p.Entries {
				if _, dup := seen[e.ID]; dup {
					return
				}
				seen[e.ID] = struct{}{}
			}
		}
		ix := New()
		for _, p := range b.Packs {
			ix.AddPack(p.ID, p.Entries)
		}
		for _, p := range b.Packs {
			for _, e := range p.Entries {
				loc, ok := ix.Lookup(e.ID)
				if !ok {
					t.Fatalf("chunk %s from the blob is not in the index", e.ID)
				}
				if loc.Length != e.Length && loc.Pack == p.ID {
					t.Fatalf("chunk %s: length %d, blob says %d", e.ID, loc.Length, e.Length)
				}
			}
		}
	})
}
