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

var goldenMaster = crypto.Key{
	0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
	0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
	0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
	0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
}

func testKeys(t *testing.T) *crypto.Keys {
	t.Helper()

	return crypto.DeriveKeys(goldenMaster)
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

func entry(chunk byte, offset uint64, length uint64) pack.Entry {
	return pack.Entry{ID: id(chunk), Offset: offset, Length: length}
}

// packInfos wraps per-pack entries as the Save/Rebuild argument form.
func packInfos(entries map[crypto.ID][]pack.Entry) map[crypto.ID]PackInfo {
	out := make(map[crypto.ID]PackInfo, len(entries))
	for packID, es := range entries {
		out[packID] = PackInfo{Size: uint64(len(es)), Entries: es}
	}
	return out
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
// serves; the index points at the one with the smaller pack ID, whatever
// order the packs were added in, so that a rebuild cannot move the live
// copy out from under prune.
func TestDuplicateChunkResolvesToTheSmallestPackID(t *testing.T) {
	packs := []struct {
		pack    crypto.ID
		entries []pack.Entry
	}{
		{id(0xcc), []pack.Entry{entry(1, 0, 100), entry(2, 100, 50)}},
		{id(0xaa), []pack.Entry{entry(1, 500, 100), entry(3, 0, 10)}},
		{id(0xbb), []pack.Entry{entry(2, 0, 50), entry(3, 50, 10)}},
	}

	forward, reverse := New(), New()
	for _, p := range packs {
		forward.AddPack(p.pack, p.entries)
	}
	for i := len(packs) - 1; i >= 0; i-- {
		reverse.AddPack(packs[i].pack, packs[i].entries)
	}

	want := map[byte]crypto.ID{1: id(0xaa), 2: id(0xbb), 3: id(0xaa)}
	for chunk, wantPack := range want {
		for name, ix := range map[string]*Index{"forward": forward, "reverse": reverse} {
			loc, ok := ix.Lookup(id(chunk))
			if !ok {
				t.Fatalf("%s: chunk %d is missing", name, chunk)
			}
			if loc.Pack != wantPack {
				t.Errorf("%s: chunk %d points at pack %s, want %s", name, chunk, loc.Pack, wantPack)
			}
		}
	}
	if len(forward.Packs()) != 3 {
		t.Errorf("Packs = %d, want all three recorded", len(forward.Packs()))
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
	blobID, err := Save(ctx, b, keys, packInfos(packs), nil, crypto.DeterministicReader("save"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}

	loaded, skipped, err := LoadAll(ctx, b, keys)
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if len(skipped) != 0 {
		t.Errorf("load all skipped %v", skipped)
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
		got, err := Save(ctx, testBackend(t), keys, packInfos(packs), nil, crypto.DeterministicReader("order"))
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
	if _, err := Save(context.Background(), testBackend(t), testKeys(t), nil, nil, crypto.DeterministicReader("empty")); err == nil {
		t.Fatal("save with no packs: got nil error")
	}
}

func TestLoadAllOnAnEmptyRepository(t *testing.T) {
	ix, skipped, err := LoadAll(context.Background(), testBackend(t), testKeys(t))
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if len(skipped) != 0 {
		t.Errorf("load all skipped %v", skipped)
	}
	if ix.Len() != 0 {
		t.Errorf("Len = %d, want 0", ix.Len())
	}
}

func TestLoadRejectsDamagedBlobs(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	blobID, err := Save(ctx, b, keys, packInfos(map[crypto.ID][]pack.Entry{id(0xaa): {entry(1, 0, 100)}}), nil, crypto.DeterministicReader("damage"))
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

	blobID, err := Save(ctx, b, keys, packInfos(map[crypto.ID][]pack.Entry{id(0xaa): {entry(1, 0, 100)}}), nil, crypto.DeterministicReader("keys"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}

	otherMaster := goldenMaster
	otherMaster[0] ^= 0xff
	other := crypto.DeriveKeys(otherMaster)
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

	// Skipped and reported, not fatal: an index is a cache, and opening
	// the repository is how a caller reaches rebuild-index.
	ix, skipped, err := LoadAll(ctx, b, testKeys(t))
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if ix.Len() != 0 {
		t.Errorf("index holds %d chunks, want 0", ix.Len())
	}
	if len(skipped) != 1 || !strings.Contains(skipped[0].Error(), "not-a-hash") {
		t.Fatalf("skipped = %v, want one naming the bad blob", skipped)
	}
}

// A blob that will not decrypt is skipped the same way, so one damaged
// cache entry cannot make a repository unopenable.
func TestLoadAllSkipsUnreadableBlobs(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	good, err := Save(ctx, b, keys, packInfos(map[crypto.ID][]pack.Entry{id(0xaa): {entry(1, 0, 100)}}), nil, crypto.DeterministicReader("good"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}

	// A blob whose name matches its bytes but which is not ours.
	junk := bytes.Repeat([]byte{0x5a}, 128)
	if err := backend.PutBytesIfAbsent(ctx, b, Key(crypto.CiphertextID(junk)), junk); err != nil {
		t.Fatalf("store: %v", err)
	}

	ix, skipped, err := LoadAll(ctx, b, keys)
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if len(skipped) != 1 {
		t.Fatalf("skipped = %v, want exactly one", skipped)
	}
	if !ix.Has(id(1)) {
		t.Error("the readable blob was not loaded")
	}
	_ = good
}

// An index is a cache. Rebuild proves it by reconstructing the same
// answers from the packs alone, with the blobs deleted.
func TestRebuildReconstructsTheIndexFromPacksAlone(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	payloads := [][]byte{[]byte("alpha"), []byte("beta"), bytes.Repeat([]byte("gamma "), 1000)}
	written := map[crypto.ID]PackInfo{}
	for i, payload := range payloads {
		w, err := pack.NewWriter(keys, t.TempDir(), crypto.DeterministicReader(fmt.Sprintf("rebuild-%d", i)))
		if err != nil {
			t.Fatalf("new writer: %v", err)
		}
		chunkID := crypto.ContentID(&keys.Hash, payload)
		if err := w.Add(chunkID, payload); err != nil {
			t.Fatalf("add: %v", err)
		}
		packID, entries, total, err := w.Finish(ctx, b)
		if err != nil {
			t.Fatalf("finish: %v", err)
		}
		written[packID] = PackInfo{Size: total, Entries: entries}
	}

	saved, err := Save(ctx, b, keys, written, nil, crypto.DeterministicReader("rebuild-index"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	fromBlob, loadErrors, err := LoadAll(ctx, b, keys)
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if len(loadErrors) != 0 {
		t.Fatalf("load all reported %d errors: %v", len(loadErrors), loadErrors)
	}

	// Delete the blob: the packs alone must be enough.
	if err := b.Delete(ctx, Key(saved)); err != nil {
		t.Fatalf("delete: %v", err)
	}
	rebuilt, rebuiltPacks, err := Rebuild(ctx, b, keys)
	if err != nil {
		t.Fatalf("rebuild: %v", err)
	}
	if len(rebuiltPacks) != len(payloads) {
		t.Errorf("rebuild reported %d packs, want %d", len(rebuiltPacks), len(payloads))
	}
	// The rebuilt PackInfo sizes must be what `check` will HEAD for: the
	// stored packs' actual lengths.
	for packID, info := range rebuiltPacks {
		fi, err := b.Stat(ctx, pack.Key(packID))
		if err != nil {
			t.Fatalf("stat pack %s: %v", packID, err)
		}
		if info.Size != uint64(fi.Size) {
			t.Errorf("pack %s: rebuilt size %d, stored %d", packID, info.Size, fi.Size)
		}
		if len(info.Entries) != 1 {
			t.Errorf("pack %s: rebuilt %d entries, want 1", packID, len(info.Entries))
		}
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

// LoadAll ignores any blob named in a surviving blob's supersedes. That is
// what makes a prune's index rewrite safe: a reader that still sees the old
// and new blobs together takes the new one's word for what exists.
func TestLoadAllIgnoresSupersededBlobs(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	old, err := Save(ctx, b, keys, packInfos(map[crypto.ID][]pack.Entry{
		id(0xaa): {entry(1, 0, 100)},
	}), nil, crypto.DeterministicReader("old"))
	if err != nil {
		t.Fatalf("save old: %v", err)
	}

	// A replacement that names a different pack for the same chunk and
	// supersedes the first blob.
	fresh, err := Save(ctx, b, keys, packInfos(map[crypto.ID][]pack.Entry{
		id(0xbb): {entry(1, 0, 50)},
	}), []crypto.ID{old}, crypto.DeterministicReader("new"))
	if err != nil {
		t.Fatalf("save fresh: %v", err)
	}
	if fresh == old {
		t.Fatal("the replacement blob collided with the one it replaces")
	}

	ix, skipped, err := LoadAll(ctx, b, keys)
	if err != nil {
		t.Fatalf("load all: %v", err)
	}
	if len(skipped) != 0 {
		t.Fatalf("skipped = %v, want none", skipped)
	}
	loc, ok := ix.Lookup(id(1))
	if !ok {
		t.Fatal("chunk 1 is missing")
	}
	if loc.Pack != id(0xbb) {
		t.Errorf("chunk 1 resolves to pack %s (the superseded blob's), want %s", loc.Pack, id(0xbb))
	}
	if packs := ix.Packs(); len(packs) != 1 || packs[0] != id(0xbb) {
		t.Errorf("Packs = %v, want only the replacement's pack", packs)
	}

	// Load (single blob) still reads a superseded blob when asked directly:
	// supersession is a LoadAll-level view, not a mark on the object.
	ix2 := New()
	if err := Load(ctx, b, keys, old, ix2); err != nil {
		t.Fatalf("load superseded blob directly: %v", err)
	}
	loc2, ok := ix2.Lookup(id(1))
	if !ok || loc2.Pack != id(0xaa) {
		t.Error("direct load of the superseded blob lost its own entry")
	}
}

func TestGoldenIndexBlob(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	packs := map[crypto.ID][]pack.Entry{
		id(0xbb): {entry(3, 0, 77)},
		id(0xaa): {entry(1, 0, 100), entry(2, 100, 250)},
	}
	blobID, err := Save(ctx, b, keys, packInfos(packs), nil, crypto.DeterministicReader("golden-index"))
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
