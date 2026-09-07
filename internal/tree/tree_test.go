package tree

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
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
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

func chunkID(b byte) crypto.ID {
	var id crypto.ID
	id[0] = b
	return id
}

// idPtr is a helper for the pointer-valued Subtree and Prev fields: Go's
// omitempty needs a pointer to tell "absent" from "the zero ID".
func idPtr(id crypto.ID) *crypto.ID { return &id }

func file(name string, chunks ...crypto.ID) Entry {
	return Entry{Name: []byte(name), Type: uint8(TypeFile), Mode: 0o644, Chunks: chunks}
}

func sampleEntries() []Entry {
	return []Entry{
		{Name: []byte("notes.txt"), Type: uint8(TypeFile), Mode: 0o644, UID: 1000, GID: 1000, MTimeNs: 1767225845000000000, Size: 12, Chunks: []crypto.ID{chunkID(1), chunkID(2)}},
		{Name: []byte("docs"), Type: uint8(TypeDir), Mode: 0o755 | uint32(os.ModeDir), Subtree: idPtr(chunkID(9))},
		{Name: []byte("link"), Type: uint8(TypeSymlink), Mode: 0o777 | uint32(os.ModeSymlink), Target: []byte("notes.txt")},
		{Name: []byte("empty"), Type: uint8(TypeFile), Mode: 0o600},
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

	if got := namesJoined(forward.Entries); got != "docs,empty,link,notes.txt" {
		t.Errorf("sorted names = %s, want docs,empty,link,notes.txt", got)
	}
}

func namesJoined(entries []Entry) string {
	names := make([]string, len(entries))
	for i, e := range entries {
		names[i] = string(e.Name)
	}
	return strings.Join(names, ",")
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
		if !bytes.Equal(got.Name, want.Name) || got.Type != want.Type || got.Mode != want.Mode ||
			got.Size != want.Size || !bytes.Equal(got.Target, want.Target) ||
			!equalPtrs(got.Subtree, want.Subtree) ||
			got.MTimeNs != want.MTimeNs || len(got.Chunks) != len(want.Chunks) {
			t.Errorf("entry %d: got %+v, want %+v", i, got, want)
		}
	}
}

func equalPtrs(a, b *crypto.ID) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
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
		"renamed":     func(e []Entry) []Entry { e[0].Name = []byte("notes2.txt"); return e },
		"remoded":     func(e []Entry) []Entry { e[0].Mode = 0o600; return e },
		"resized":     func(e []Entry) []Entry { e[0].Size = 13; return e },
		"rechunked":   func(e []Entry) []Entry { e[0].Chunks[1] = chunkID(3); return e },
		"retimed":     func(e []Entry) []Entry { e[0].MTimeNs++; return e },
		"new subtree": func(e []Entry) []Entry { e[1].Subtree = idPtr(chunkID(8)); return e },
		"new target":  func(e []Entry) []Entry { e[2].Target = []byte("docs"); return e },
		"entry gone":  func(e []Entry) []Entry { return e[1:] },
		"entry added": func(e []Entry) []Entry {
			return append(e, Entry{Name: []byte("extra"), Type: uint8(TypeFile), Mode: 0o644})
		},
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
		"empty name":           {{Name: nil, Type: uint8(TypeFile), Mode: 0o644}},
		"dot":                  {{Name: []byte("."), Type: uint8(TypeFile), Mode: 0o644}},
		"dotdot":               {{Name: []byte(".."), Type: uint8(TypeFile), Mode: 0o644}},
		"path separator":       {{Name: []byte("a/b"), Type: uint8(TypeFile), Mode: 0o644}},
		"embedded NUL":         {{Name: []byte("a\x00b"), Type: uint8(TypeFile), Mode: 0o644}},
		"duplicate name":       {{Name: []byte("a"), Type: uint8(TypeFile)}, {Name: []byte("a"), Type: uint8(TypeFile)}},
		"dir without subtree":  {{Name: []byte("d"), Type: uint8(TypeDir)}},
		"dir with chunks":      {{Name: []byte("d"), Type: uint8(TypeDir), Subtree: idPtr(chunkID(1)), Chunks: []crypto.ID{chunkID(2)}}},
		"symlink no target":    {{Name: []byte("l"), Type: uint8(TypeSymlink)}},
		"symlink with chunks":  {{Name: []byte("l"), Type: uint8(TypeSymlink), Target: []byte("x"), Chunks: []crypto.ID{chunkID(2)}}},
		"file with subtree":    {{Name: []byte("f"), Type: uint8(TypeFile), Subtree: idPtr(chunkID(1))}},
		"file with target":     {{Name: []byte("f"), Type: uint8(TypeFile), Target: []byte("x")}},
		"unknown type":         {{Name: []byte("x"), Type: uint8(NodeType(9))}},
		"unknown content type": {{Name: []byte("f"), Type: uint8(TypeFile), ContentType: 2}},
		// Absolute paths are the ROOT tree's naming rule; they must still
		// be made of clean components.
		"relative parent component": {{Name: []byte("/a/../b"), Type: uint8(TypeFile)}},
		"empty absolute component":  {{Name: []byte("/a//b"), Type: uint8(TypeFile)}},
		"trailing slash":            {{Name: []byte("/a/"), Type: uint8(TypeFile)}},
		"root itself":               {{Name: []byte("/"), Type: uint8(TypeFile)}},
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

// A root-level entry is named by its full absolute path (the v2 rule, so
// the Go and Rust implementations agree on the root tree's name), and a
// file whose chunk list would run past MaxInlineChunks must be indirect.
func TestValidateAcceptsAbsoluteRootNames(t *testing.T) {
	keys := testKeys(t)

	entries := []Entry{
		{Name: []byte("/srv/data"), Type: uint8(TypeDir), Mode: 0o755, Subtree: idPtr(chunkID(9))},
		{Name: []byte("/tmp/poc/go2-data"), Type: uint8(TypeFile), Mode: 0o644, Chunks: []crypto.ID{chunkID(1)}},
	}
	if _, _, err := (&Tree{Version: Version, Entries: entries}).Encode(&keys.Hash); err != nil {
		t.Fatalf("encode a root tree with absolute names: %v", err)
	}

	var many []crypto.ID
	for i := range MaxInlineChunks + 1 {
		many = append(many, chunkID(byte(i)))
	}
	direct := []Entry{{Name: []byte("big"), Type: uint8(TypeFile), Chunks: many}}
	if _, _, err := (&Tree{Version: Version, Entries: direct}).Encode(&keys.Hash); !errors.Is(err, ErrCorrupt) {
		t.Fatalf("encode an over-long inline chunk list: err = %v, want ErrCorrupt", err)
	}
	indirect := []Entry{{Name: []byte("big"), Type: uint8(TypeFile), Chunks: many, ContentType: uint8(ContentIndirect)}}
	if _, _, err := (&Tree{Version: Version, Entries: indirect}).Encode(&keys.Hash); err != nil {
		t.Fatalf("encode an indirect chunk list: %v", err)
	}
}

func TestValidateRejectsUnsortedEntries(t *testing.T) {
	keys := testKeys(t)
	tr := &Tree{Version: Version, Entries: []Entry{
		{Name: []byte("b"), Type: uint8(TypeFile), Mode: 0o644},
		{Name: []byte("a"), Type: uint8(TypeFile), Mode: 0o644},
	}}

	if _, _, err := tr.Encode(&keys.Hash); !errors.Is(err, ErrCorrupt) {
		t.Fatalf("encode: err = %v, want ErrCorrupt", err)
	}
}

// A huge directory is split into segments linked by Prev; LoadChain walks
// them backwards and returns the entries oldest first, and rejects a loop.
func TestLoadChainReassemblesSegments(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	older := New([]Entry{file("a"), file("b")})
	first, err := older.Save(ctx, b, keys, crypto.DeterministicReader("chain-1"))
	if err != nil {
		t.Fatalf("save segment 1: %v", err)
	}

	newer := New([]Entry{file("c"), file("d")})
	newer.Prev = &first
	last, err := newer.Save(ctx, b, keys, crypto.DeterministicReader("chain-2"))
	if err != nil {
		t.Fatalf("save segment 2: %v", err)
	}

	entries, err := LoadChain(ctx, b, keys, last)
	if err != nil {
		t.Fatalf("load chain: %v", err)
	}
	if got := namesJoined(entries); got != "a,b,c,d" {
		t.Fatalf("chain entries = %s, want a,b,c,d", got)
	}

	// An unsegmented tree is the chain of length one: Prev nil, entries as
	// they were saved.
	single, err := New([]Entry{file("solo")}).Save(ctx, b, keys, crypto.DeterministicReader("chain-0"))
	if err != nil {
		t.Fatalf("save single: %v", err)
	}
	solo, err := LoadChain(ctx, b, keys, single)
	if err != nil {
		t.Fatalf("load single: %v", err)
	}
	if got := namesJoined(solo); got != "solo" {
		t.Fatalf("single tree chain = %s, want solo", got)
	}
}

func TestNewChunkListDocument(t *testing.T) {
	chunks := []crypto.ID{chunkID(1), chunkID(2), chunkID(3)}

	list := NewChunkList(chunks)
	if list.Version != Version || !slices.Equal(list.Chunks, chunks) {
		t.Errorf("chunk list = %+v", list)
	}

	encoded, err := crypto.Marshal(list)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var back ChunkList
	if err := crypto.Unmarshal(encoded, &back); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if back.Version != Version || !slices.Equal(back.Chunks, chunks) {
		t.Errorf("round trip = %+v", back)
	}
}

// Xattrs carry byte-string keys (xattr names are not required to be
// UTF-8) and must encode as a CBOR map sorted bytewise on the key, so two
// encoders agree on one directory's name.
func TestXattrsMarshalSortedByteStringKeys(t *testing.T) {
	x := Xattrs{
		{Name: []byte("user.z"), Value: []byte("vz")},
		{Name: []byte{0xff, 0xfe}, Value: []byte("non-utf8 name")},
		{Name: []byte("user.a"), Value: []byte("va")},
	}

	encoded, err := x.MarshalCBOR()
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	// Keys must come out sorted by their encoded bytes: "user.a" < "user.z"
	// < 0xff 0xfe (0xff is above every ASCII byte).
	var back Xattrs
	if err := back.UnmarshalCBOR(encoded); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(back) != 3 || !bytes.Equal(back[0].Name, []byte("user.a")) ||
		!bytes.Equal(back[1].Name, []byte("user.z")) || !bytes.Equal(back[2].Name, []byte{0xff, 0xfe}) {
		t.Errorf("keys not sorted bytewise: %+v", back)
	}
	if !bytes.Equal(back[2].Value, []byte("non-utf8 name")) {
		t.Errorf("value round trip = %q", back[2].Value)
	}

	// The encoding is a CBOR map with byte-string keys, not text strings.
	if len(encoded) == 0 || encoded[0]>>5 != 5 {
		t.Errorf("xattrs did not encode as a CBOR map: %x", encoded)
	}

	for name, junk := range map[string][]byte{
		"trailing bytes": append(slices.Clone(encoded), 0x00),
		"truncated":      encoded[:len(encoded)-1],
		"not a map":      {0x40},
	} {
		t.Run(name, func(t *testing.T) {
			var out Xattrs
			if err := out.UnmarshalCBOR(junk); err == nil {
				t.Fatal("unmarshal: got nil error")
			}
		})
	}
}

// An entry with xattrs round-trips through the tree encoding, and an
// entry without them decodes back to none. (Whether the empty field is
// omitted on the wire is pinned by the golden test, which uses entries
// with no xattrs.)
func TestXattrsInTreeRoundTrip(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	entries := []Entry{
		{Name: []byte("f"), Type: uint8(TypeFile), Mode: 0o644,
			Xattrs: Xattrs{{Name: []byte("user.comment"), Value: []byte("hello")}}},
	}
	id, err := New(entries).Save(ctx, b, keys, crypto.DeterministicReader("xattrs"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	loaded, err := Load(ctx, b, keys, id)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	got := loaded.Entries[0].Xattrs
	if len(got) != 1 || !bytes.Equal(got[0].Name, []byte("user.comment")) || !bytes.Equal(got[0].Value, []byte("hello")) {
		t.Errorf("xattrs round trip = %+v", got)
	}

	_, plain, err := New([]Entry{file("g")}).Encode(&keys.Hash)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	var bare Tree
	if err := crypto.Unmarshal(plain, &bare); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(bare.Entries) != 1 || len(bare.Entries[0].Xattrs) != 0 {
		t.Errorf("an entry without xattrs decoded to %+v", bare.Entries)
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

// docs/format.md: integers take their shortest encoding. The head form
// changes at 24, 2^8, 2^16 and 2^32; the last step is the one a
// four-byte-only writer would silently truncate.
func TestWriteHeadUsesTheShortestForm(t *testing.T) {
	cases := []struct {
		length uint64
		want   string
	}{
		{0, "40"},
		{23, "57"},
		{24, "5818"},
		{0xff, "58ff"},
		{0x100, "590100"},
		{0xffff, "59ffff"},
		{0x10000, "5a00010000"},
		{0xffffffff, "5affffffff"},
		{0x100000000, "5b0000000100000000"},
	}
	for _, tc := range cases {
		var buf bytes.Buffer
		writeHead(&buf, 2, tc.length)
		if got := hex.EncodeToString(buf.Bytes()); got != tc.want {
			t.Errorf("writeHead(2, %#x) = %s, want %s", tc.length, got, tc.want)
		}
	}
}
