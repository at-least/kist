package tree

import (
	"context"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
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

func chunkID(b byte) crypto.ID {
	var id crypto.ID
	id[0] = b
	return id
}

func sampleEntries() []Entry {
	return []Entry{
		{Name: "notes.txt", Type: TypeFile, Mode: 0o644, UID: 1000, GID: 1000, MTimeNs: 1767225845000000000, Size: 12, Chunks: []crypto.ID{chunkID(1), chunkID(2)}},
		{Name: "docs", Type: TypeDir, Mode: 0o755 | uint32(os.ModeDir), Subtree: chunkID(9)},
		{Name: "link", Type: TypeSymlink, Mode: 0o777 | uint32(os.ModeSymlink), Target: "notes.txt"},
		{Name: "empty", Type: TypeFile, Mode: 0o600},
	}
}

// Two clients walking one directory in different orders must produce the
// same tree ID, or deduplication stops at the first directory.
func TestNewSortsEntries(t *testing.T) {
	keys := testKeys(t)

	entries := sampleEntries()
	forward := New(entries)
	reversed := New(append([]Entry(nil), entries[3], entries[2], entries[1], entries[0]))

	forwardID, _, err := forward.Encode(&keys.Hash)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	reversedID, _, err := reversed.Encode(&keys.Hash)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	if forwardID != reversedID {
		t.Fatalf("entry order changed the tree ID: %s vs %s", forwardID, reversedID)
	}

	names := make([]string, len(forward.Entries))
	for i, e := range forward.Entries {
		names[i] = e.Name
	}
	if got := strings.Join(names, ","); got != "docs,empty,link,notes.txt" {
		t.Errorf("sorted names = %s, want docs,empty,link,notes.txt", got)
	}
}

func TestSaveLoadRoundTrip(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	original := New(sampleEntries())
	id, err := original.Save(ctx, b, keys, crypto.DeterministicReader("tree"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}

	loaded, err := Load(ctx, b, keys, id)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	if len(loaded.Entries) != len(original.Entries) {
		t.Fatalf("loaded %d entries, want %d", len(loaded.Entries), len(original.Entries))
	}
	for i := range original.Entries {
		got, want := loaded.Entries[i], original.Entries[i]
		if got.Name != want.Name || got.Type != want.Type || got.Mode != want.Mode ||
			got.Size != want.Size || got.Target != want.Target || got.Subtree != want.Subtree ||
			got.MTimeNs != want.MTimeNs || len(got.Chunks) != len(want.Chunks) {
			t.Errorf("entry %d: got %+v, want %+v", i, got, want)
		}
	}
}

// A tree is named by its plaintext, not its ciphertext. Otherwise every
// nightly backup would rename every unchanged directory.
func TestUnchangedTreeKeepsItsName(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	first, err := New(sampleEntries()).Save(ctx, b, keys, crypto.DeterministicReader("first"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	// A different nonce source means different ciphertext entirely.
	second, err := New(sampleEntries()).Save(ctx, b, keys, crypto.DeterministicReader("second-and-very-different"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	if first != second {
		t.Fatalf("the same directory got two names: %s and %s", first, second)
	}
}

func TestAChangedEntryChangesTheName(t *testing.T) {
	keys := testKeys(t)

	base, _, err := New(sampleEntries()).Encode(&keys.Hash)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}

	for name, mutate := range map[string]func([]Entry) []Entry{
		"renamed":      func(e []Entry) []Entry { e[0].Name = "notes2.txt"; return e },
		"remoded":      func(e []Entry) []Entry { e[0].Mode = 0o600; return e },
		"resized":      func(e []Entry) []Entry { e[0].Size = 13; return e },
		"rechunked":    func(e []Entry) []Entry { e[0].Chunks[1] = chunkID(3); return e },
		"retimed":      func(e []Entry) []Entry { e[0].MTimeNs++; return e },
		"new subtree":  func(e []Entry) []Entry { e[1].Subtree = chunkID(8); return e },
		"new target":   func(e []Entry) []Entry { e[2].Target = "docs"; return e },
		"entry gone":   func(e []Entry) []Entry { return e[1:] },
		"entry added":  func(e []Entry) []Entry { return append(e, Entry{Name: "extra", Type: TypeFile, Mode: 0o644}) },
		"owner change": func(e []Entry) []Entry { e[0].UID = 1001; return e },
	} {
		t.Run(name, func(t *testing.T) {
			got, _, err := New(mutate(sampleEntries())).Encode(&keys.Hash)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			if got == base {
				t.Errorf("%s did not change the tree ID", name)
			}
		})
	}
}

func TestLoadRejectsDamage(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	id, err := New(sampleEntries()).Save(ctx, b, keys, crypto.DeterministicReader("damage"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	sealed, err := backend.GetAll(ctx, b, Key(id))
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	damaged := testBackend(t)
	flipped := append([]byte(nil), sealed...)
	flipped[len(flipped)/2] ^= 0x01
	if err := backend.PutBytesIfAbsent(ctx, damaged, Key(id), flipped); err != nil {
		t.Fatalf("store: %v", err)
	}
	if _, err := Load(ctx, damaged, keys, id); !errors.Is(err, crypto.ErrDecrypt) {
		t.Fatalf("load a damaged tree: err = %v, want ErrDecrypt", err)
	}
}

// A tree served under another tree's name must not open: the name is the
// AAD as well as the address.
func TestTreeCannotBeServedUnderAnotherName(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	realID, err := New(sampleEntries()).Save(ctx, b, keys, crypto.DeterministicReader("real"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	sealed, err := backend.GetAll(ctx, b, Key(realID))
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	other := chunkID(0xfe)
	if err := backend.PutBytesIfAbsent(ctx, b, Key(other), sealed); err != nil {
		t.Fatalf("store: %v", err)
	}
	if _, err := Load(ctx, b, keys, other); !errors.Is(err, crypto.ErrDecrypt) {
		t.Fatalf("load under the wrong name: err = %v, want ErrDecrypt", err)
	}
}

func TestValidateRejectsImpossibleTrees(t *testing.T) {
	cases := map[string][]Entry{
		"empty name":          {{Name: "", Type: TypeFile, Mode: 0o644}},
		"dot":                 {{Name: ".", Type: TypeFile, Mode: 0o644}},
		"dotdot":              {{Name: "..", Type: TypeFile, Mode: 0o644}},
		"path separator":      {{Name: "a/b", Type: TypeFile, Mode: 0o644}},
		"embedded NUL":        {{Name: "a\x00b", Type: TypeFile, Mode: 0o644}},
		"duplicate name":      {{Name: "a", Type: TypeFile}, {Name: "a", Type: TypeFile}},
		"dir without subtree": {{Name: "d", Type: TypeDir}},
		"dir with chunks":     {{Name: "d", Type: TypeDir, Subtree: chunkID(1), Chunks: []crypto.ID{chunkID(2)}}},
		"symlink no target":   {{Name: "l", Type: TypeSymlink}},
		"symlink with chunks": {{Name: "l", Type: TypeSymlink, Target: "x", Chunks: []crypto.ID{chunkID(2)}}},
		"file with subtree":   {{Name: "f", Type: TypeFile, Subtree: chunkID(1)}},
		"file with target":    {{Name: "f", Type: TypeFile, Target: "x"}},
		"unknown type":        {{Name: "x", Type: NodeType(9)}},
	}

	keys := testKeys(t)
	for name, entries := range cases {
		t.Run(name, func(t *testing.T) {
			// Bypass New's sorting for the ordering cases by constructing
			// the tree directly, which is what a damaged object looks like.
			tr := &Tree{Version: Version, Entries: entries}
			if _, _, err := tr.Encode(&keys.Hash); !errors.Is(err, ErrCorrupt) {
				t.Fatalf("encode: err = %v, want ErrCorrupt", err)
			}
		})
	}
}

func TestValidateRejectsUnsortedEntries(t *testing.T) {
	keys := testKeys(t)
	tr := &Tree{Version: Version, Entries: []Entry{
		{Name: "b", Type: TypeFile, Mode: 0o644},
		{Name: "a", Type: TypeFile, Mode: 0o644},
	}}

	if _, _, err := tr.Encode(&keys.Hash); !errors.Is(err, ErrCorrupt) {
		t.Fatalf("encode: err = %v, want ErrCorrupt", err)
	}
}

func TestNodeTypeString(t *testing.T) {
	for typ, want := range map[NodeType]string{TypeFile: "file", TypeDir: "dir", TypeSymlink: "symlink", NodeType(7): "unknown(7)"} {
		if got := typ.String(); got != want {
			t.Errorf("NodeType(%d).String() = %q, want %q", typ, got, want)
		}
	}
}

func TestGoldenTree(t *testing.T) {
	keys := testKeys(t)

	id, encoded, err := New(sampleEntries()).Encode(&keys.Hash)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}

	var out strings.Builder
	fmt.Fprintf(&out, "tree %s\n", id)
	fmt.Fprintf(&out, "size %d\n", len(encoded))
	fmt.Fprintf(&out, "cbor %s\n", hex.EncodeToString(encoded))

	path := filepath.Join("testdata", "tree.txt")
	if *update {
		if err := os.WriteFile(path, []byte(out.String()), 0o644); err != nil {
			t.Fatalf("write golden: %v", err)
		}
		return
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read golden: %v (run: go test ./internal/tree/ -update)", err)
	}
	if out.String() != string(want) {
		t.Error("the tree object format changed")
	}
}
