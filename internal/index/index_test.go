package index

import (
	"bytes"
	"context"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
)

var update = flag.Bool("update", false, "rewrite testdata golden files")

var (
	goldenMaster = crypto.Key{
		0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
		0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
		0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
		0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
	}
	goldenRepoID = crypto.RepoID{
		0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
		0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
	}
)

func testKeys(t *testing.T) *crypto.Keys {
	t.Helper()

	keys, err := crypto.DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive keys: %v", err)
	}
	return keys
}

func testBackend(t *testing.T) backend.Backend {
	t.Helper()

	b, err := backend.CreateLocal(filepath.Join(t.TempDir(), "repo"))
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}
	t.Cleanup(func() {
		if err := b.Close(); err != nil {
			t.Errorf("close backend: %v", err)
		}
	})
	return b
}

func id(b byte) crypto.ID {
	var out crypto.ID
	out[0] = b
	return out
}

func entry(chunk byte, offset uint64, length uint32) pack.Entry {
	return pack.Entry{ID: id(chunk), Offset: offset, Length: length}
}

func TestLookupAndHas(t *testing.T) {
	ix := New()
	ix.AddPack(id(0xaa), []pack.Entry{entry(1, 0, 100), entry(2, 100, 250)})

	loc, ok := ix.Lookup(id(2))
	if !ok {
		t.Fatal("chunk 2 is missing")
	}
	if want := (Location{Pack: id(0xaa), Offset: 100, Length: 250}); loc != want {
		t.Errorf("location = %+v, want %+v", loc, want)
	}
	if !ix.Has(id(1)) {
		t.Error("Has(1) = false")
	}
	if ix.Has(id(9)) {
		t.Error("Has(9) = true for a chunk never added")
	}
	if ix.Len() != 2 {
		t.Errorf("Len = %d, want 2", ix.Len())
	}
	if packs := ix.Packs(); len(packs) != 1 || packs[0] != id(0xaa) {
		t.Errorf("Packs = %v, want [%s]", packs, id(0xaa))
	}
}

// Two clients can pack the same content at the same moment. Either copy
// serves, so the first one recorded wins and the second is left
// unreferenced for prune to collect.
func TestDuplicateChunkKeepsTheFirstLocation(t *testing.T) {
	ix := New()
	ix.AddPack(id(0xaa), []pack.Entry{entry(1, 0, 100)})
	ix.AddPack(id(0xbb), []pack.Entry{entry(1, 500, 100)})

	loc, ok := ix.Lookup(id(1))
	if !ok {
		t.Fatal("chunk 1 is missing")
	}
	if loc.Pack != id(0xaa) {
		t.Errorf("chunk 1 points at pack %s, want the first one, %s", loc.Pack, id(0xaa))
	}
	if len(ix.Packs()) != 2 {
		t.Errorf("Packs = %d, want both packs recorded", len(ix.Packs()))
	}
}

func TestIndexIsSafeForConcurrentUse(t *testing.T) {
	ix := New()

	var wg sync.WaitGroup
	for w := range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range 32 {
				chunk := byte(w*32 + i)
				ix.AddPack(id(byte(w)), []pack.Entry{entry(chunk, 0, 10)})
				ix.Has(id(chunk))
				ix.Lookup(id(chunk))
				ix.Len()
			}
		}()
	}
	wg.Wait()

	if ix.Len() != 8*32 {
		t.Errorf("Len = %d, want %d", ix.Len(), 8*32)
	}
}

func TestSaveLoadRoundTrip(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	packs := map[crypto.ID][]pack.Entry{
		id(0xaa): {entry(1, 0, 100), entry(2, 100, 250)},
		id(0xbb): {entry(3, 0, 77)},
	}
	blobID, err := Save(ctx, b, keys, packs, crypto.DeterministicReader("save"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}

	loaded, err := LoadAll(ctx, b, keys)
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if loaded.Len() != 3 {
		t.Errorf("loaded %d chunks, want 3", loaded.Len())
	}
	for chunk, wantPack := range map[byte]crypto.ID{1: id(0xaa), 2: id(0xaa), 3: id(0xbb)} {
		loc, ok := loaded.Lookup(id(chunk))
		if !ok {
			t.Errorf("chunk %d is missing after a round trip", chunk)
			continue
		}
		if loc.Pack != wantPack {
			t.Errorf("chunk %d points at %s, want %s", chunk, loc.Pack, wantPack)
		}
	}

	// The blob is named by its ciphertext, like a pack.
	sealed, err := backend.GetAll(ctx, b, Key(blobID))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if got := crypto.CiphertextID(sealed); got != blobID {
		t.Errorf("blob stored as %s but hashes to %s", blobID, got)
	}
}

// The same set of packs must encode identically however Go happens to
// order the map, or two clients recording the same work would write two
// blobs instead of deduplicating one.
func TestSaveIsIndependentOfMapOrder(t *testing.T) {
	ctx := context.Background()
	keys := testKeys(t)

	packs := map[crypto.ID][]pack.Entry{}
	for i := range 32 {
		packs[id(byte(i))] = []pack.Entry{entry(byte(i), 0, 100)}
	}

	var first crypto.ID
	for run := range 8 {
		got, err := Save(ctx, testBackend(t), keys, packs, crypto.DeterministicReader("order"))
		if err != nil {
			t.Fatalf("save: %v", err)
		}
		if run == 0 {
			first = got
			continue
		}
		if got != first {
			t.Fatalf("run %d produced blob %s, run 0 produced %s", run, got, first)
		}
	}
}

func TestSaveRefusesAnEmptyIndex(t *testing.T) {
	if _, err := Save(context.Background(), testBackend(t), testKeys(t), nil, crypto.DeterministicReader("empty")); err == nil {
		t.Fatal("save with no packs: got nil error")
	}
}

func TestLoadAllOnAnEmptyRepository(t *testing.T) {
	ix, err := LoadAll(context.Background(), testBackend(t), testKeys(t))
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if ix.Len() != 0 {
		t.Errorf("Len = %d, want 0", ix.Len())
	}
}

func TestLoadRejectsDamagedBlobs(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	blobID, err := Save(ctx, b, keys, map[crypto.ID][]pack.Entry{id(0xaa): {entry(1, 0, 100)}}, crypto.DeterministicReader("damage"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	original, err := backend.GetAll(ctx, b, Key(blobID))
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	cases := map[string]struct {
		mutate  func([]byte) []byte
		wantErr error
	}{
		"flipped ciphertext byte": {
			func(p []byte) []byte { p = bytes.Clone(p); p[len(p)/2] ^= 0x01; return p },
			ErrCorrupt, // the name no longer matches, caught before decryption
		},
		"truncated": {
			func(p []byte) []byte { return p[:len(p)-1] },
			ErrCorrupt,
		},
		"empty": {
			func([]byte) []byte { return nil },
			ErrCorrupt,
		},
	}

	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			damaged := testBackend(t)
			if err := backend.PutBytesIfAbsent(ctx, damaged, Key(blobID), tc.mutate(original)); err != nil {
				t.Fatalf("store: %v", err)
			}
			if err := Load(ctx, damaged, keys, blobID, New()); !errors.Is(err, tc.wantErr) {
				t.Fatalf("load: err = %v, want %v", err, tc.wantErr)
			}
		})
	}
}

func TestLoadRejectsTheWrongKeys(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	blobID, err := Save(ctx, b, keys, map[crypto.ID][]pack.Entry{id(0xaa): {entry(1, 0, 100)}}, crypto.DeterministicReader("keys"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}

	otherMaster := goldenMaster
	otherMaster[0] ^= 0xff
	other, err := crypto.DeriveKeys(otherMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	if err := Load(ctx, b, other, blobID, New()); !errors.Is(err, crypto.ErrDecrypt) {
		t.Fatalf("load with the wrong keys: err = %v, want ErrDecrypt", err)
	}
}

func TestLoadAllRejectsAMisnamedBlob(t *testing.T) {
	ctx := context.Background()
	b := testBackend(t)

	if err := backend.PutBytesIfAbsent(ctx, b, Prefix+"not-a-hash", []byte("junk")); err != nil {
		t.Fatalf("store: %v", err)
	}
	if _, err := LoadAll(ctx, b, testKeys(t)); err == nil || !strings.Contains(err.Error(), "not-a-hash") {
		t.Fatalf("load all: err = %v, want it to name the bad blob", err)
	}
}

// An index is a cache. Rebuild proves it by reconstructing the same
// answers from the packs alone, with the blobs deleted.
func TestRebuildReconstructsTheIndexFromPacksAlone(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	payloads := [][]byte{[]byte("alpha"), []byte("beta"), bytes.Repeat([]byte("gamma "), 1000)}
	written := map[crypto.ID][]pack.Entry{}
	for i, payload := range payloads {
		w, err := pack.NewWriter(keys, t.TempDir(), crypto.DeterministicReader(fmt.Sprintf("rebuild-%d", i)))
		if err != nil {
			t.Fatalf("new writer: %v", err)
		}
		chunkID := crypto.ContentID(&keys.Hash, payload)
		if err := w.Add(chunkID, payload); err != nil {
			t.Fatalf("add: %v", err)
		}
		packID, entries, err := w.Finish(ctx, b)
		if err != nil {
			t.Fatalf("finish: %v", err)
		}
		written[packID] = entries
	}

	saved, err := Save(ctx, b, keys, written, crypto.DeterministicReader("rebuild-index"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	fromBlob, err := LoadAll(ctx, b, keys)
	if err != nil {
		t.Fatalf("load all: %v", err)
	}

	// Delete the blob: the packs alone must be enough.
	if err := b.Delete(ctx, Key(saved)); err != nil {
		t.Fatalf("delete: %v", err)
	}
	rebuilt, err := Rebuild(ctx, b, keys)
	if err != nil {
		t.Fatalf("rebuild: %v", err)
	}

	if rebuilt.Len() != fromBlob.Len() {
		t.Fatalf("rebuilt index has %d chunks, the blob had %d", rebuilt.Len(), fromBlob.Len())
	}
	for _, payload := range payloads {
		chunkID := crypto.ContentID(&keys.Hash, payload)
		want, ok := fromBlob.Lookup(chunkID)
		if !ok {
			t.Fatalf("chunk %s missing from the loaded index", chunkID)
		}
		got, ok := rebuilt.Lookup(chunkID)
		if !ok {
			t.Fatalf("chunk %s missing from the rebuilt index", chunkID)
		}
		if got != want {
			t.Errorf("chunk %s: rebuilt %+v, loaded %+v", chunkID, got, want)
		}
	}
}

func TestGoldenIndexBlob(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	packs := map[crypto.ID][]pack.Entry{
		id(0xbb): {entry(3, 0, 77)},
		id(0xaa): {entry(1, 0, 100), entry(2, 100, 250)},
	}
	blobID, err := Save(ctx, b, keys, packs, crypto.DeterministicReader("golden-index"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	sealed, err := backend.GetAll(ctx, b, Key(blobID))
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	var out strings.Builder
	fmt.Fprintf(&out, "blob %s\n", blobID)
	fmt.Fprintf(&out, "size %d\n", len(sealed))

	loaded := New()
	if err := Load(ctx, b, keys, blobID, loaded); err != nil {
		t.Fatalf("load: %v", err)
	}
	ids := loaded.Packs()
	slices.SortFunc(ids, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
	for _, p := range ids {
		fmt.Fprintf(&out, "pack %s\n", p)
	}
	fmt.Fprintf(&out, "bytes %s\n", hex.EncodeToString(sealed))

	path := filepath.Join("testdata", "index.txt")
	if *update {
		if err := os.WriteFile(path, []byte(out.String()), 0o644); err != nil {
			t.Fatalf("write golden: %v", err)
		}
		return
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read golden: %v (run: go test ./internal/index/ -update)", err)
	}
	if out.String() != string(want) {
		t.Error("the index blob format changed")
	}
}
