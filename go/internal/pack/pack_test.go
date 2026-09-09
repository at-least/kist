package pack

import (
	"bytes"
	"context"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/chunker"
	"github.com/at-least/kist/internal/crypto"
)

var update = flag.Bool("update", false, "rewrite testdata golden files")

// The frozen inputs the golden pack is built from.
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

// chunkOf returns a payload and the ID it must be stored under.
func chunkOf(keys *crypto.Keys, payload []byte) (crypto.ID, []byte) {
	return crypto.ContentID(&keys.Hash, payload), payload
}

func writePack(t *testing.T, b backend.Backend, keys *crypto.Keys, seed string, payloads ...[]byte) (crypto.ID, []Entry) {
	t.Helper()

	w, err := NewWriter(keys, t.TempDir(), crypto.DeterministicReader(seed))
	if err != nil {
		t.Fatalf("new writer: %v", err)
	}
	defer w.Abort()

	for _, payload := range payloads {
		id, data := chunkOf(keys, payload)
		if err := w.Add(id, data); err != nil {
			t.Fatalf("add: %v", err)
		}
	}

	id, entries, _, err := w.Finish(context.Background(), b)
	if err != nil {
		t.Fatalf("finish: %v", err)
	}
	return id, entries
}

func TestRoundTrip(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	payloads := [][]byte{
		[]byte("the first chunk"),
		bytes.Repeat([]byte("highly compressible "), 5000),
		incompressible(t, "roundtrip", 200000),
		{},
	}
	id, entries := writePack(t, b, keys, "roundtrip", payloads...)

	if len(entries) != len(payloads) {
		t.Fatalf("wrote %d entries, want %d", len(entries), len(payloads))
	}

	r, err := OpenReader(ctx, b, keys, id)
	if err != nil {
		t.Fatalf("open reader: %v", err)
	}
	if r.ID() != id {
		t.Errorf("reader ID = %s, want %s", r.ID(), id)
	}

	for i, payload := range payloads {
		wantID, _ := chunkOf(keys, payload)
		entry, ok := r.Lookup(wantID)
		if !ok {
			t.Fatalf("chunk %d (%s) is not in the trailer", i, wantID)
		}
		got, err := r.Chunk(ctx, entry)
		if err != nil {
			t.Fatalf("chunk %d: %v", i, err)
		}
		if !bytes.Equal(got, payload) {
			t.Errorf("chunk %d differs after a round trip", i)
		}
	}

	if err := r.VerifyAll(ctx); err != nil {
		t.Errorf("verify all: %v", err)
	}
}

// The name is the hash of the bytes, so a reader can prove a pack is
// intact holding no key at all.
func TestPackIsNamedByItsCiphertext(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	id, _ := writePack(t, b, keys, "naming", []byte("payload"))
	raw, err := backend.GetAll(ctx, b, Key(id))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if got := crypto.CiphertextID(raw); got != id {
		t.Errorf("pack stored as %s but hashes to %s", id, got)
	}
	if !bytes.HasSuffix(raw, magic[:]) {
		t.Errorf("pack does not end with the magic: %x", raw[len(raw)-tailSize:])
	}
}

// Two clients that build the same pack must converge, not collide.
func TestIdenticalPackIsDeduplicated(t *testing.T) {
	keys, b := testKeys(t), testBackend(t)

	first, _ := writePack(t, b, keys, "dedup", []byte("same content"))
	second, _ := writePack(t, b, keys, "dedup", []byte("same content"))
	if first != second {
		t.Fatalf("two identical packs got different IDs: %s and %s", first, second)
	}
}

func TestCompressionIsDecidedPerChunk(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	compressible := bytes.Repeat([]byte("aaaabbbbcccc"), 20000)
	random := incompressible(t, "entropy", len(compressible))

	id, entries := writePack(t, b, keys, "compression", compressible, random)

	if entries[0].Length >= uint64(len(compressible))/2 {
		t.Errorf("compressible chunk stored in %d bytes for %d of input; it was not compressed", entries[0].Length, len(compressible))
	}
	// An incompressible chunk must not grow by more than the envelope and
	// the encoding byte: the writer must have kept it raw.
	if want := uint64(len(random) + crypto.Overhead + 1); entries[1].Length != want {
		t.Errorf("incompressible chunk stored in %d bytes, want %d (stored raw)", entries[1].Length, want)
	}

	r, err := OpenReader(ctx, b, keys, id)
	if err != nil {
		t.Fatalf("open reader: %v", err)
	}
	for i, payload := range [][]byte{compressible, random} {
		got, err := r.Chunk(ctx, entries[i])
		if err != nil {
			t.Fatalf("chunk %d: %v", i, err)
		}
		if !bytes.Equal(got, payload) {
			t.Errorf("chunk %d differs after a round trip", i)
		}
	}
}

func TestWriterTracksSizeAndFullness(t *testing.T) {
	keys := testKeys(t)

	w, err := NewWriter(keys, t.TempDir(), crypto.DeterministicReader("size"))
	if err != nil {
		t.Fatalf("new writer: %v", err)
	}
	defer w.Abort()

	// v2 opens the pack with the header magic, so a fresh writer has
	// already written it: chunk offsets are absolute file positions.
	if w.Size() != magicSize || w.Count() != 0 || w.Full() {
		t.Errorf("fresh writer: size %d (want %d), count %d, full %v", w.Size(), magicSize, w.Count(), w.Full())
	}

	payload := incompressible(t, "fullness", 4<<20)
	for i := range 16 {
		id, data := chunkOf(keys, append(payload[:0:0], append(payload, byte(i))...))
		if err := w.Add(id, data); err != nil {
			t.Fatalf("add: %v", err)
		}
		if w.Full() {
			if w.Size() < TargetSize {
				t.Errorf("writer reports full at %d bytes, under the %d target", w.Size(), TargetSize)
			}
			return
		}
	}
	t.Errorf("writer never reported full: %d bytes over %d chunks", w.Size(), w.Count())
}

func TestFinishRefusesAnEmptyPack(t *testing.T) {
	keys, b := testKeys(t), testBackend(t)

	w, err := NewWriter(keys, t.TempDir(), crypto.DeterministicReader("empty"))
	if err != nil {
		t.Fatalf("new writer: %v", err)
	}
	defer w.Abort()

	if _, _, _, err := w.Finish(context.Background(), b); err == nil {
		t.Fatal("finish with no chunks: got nil error")
	}
}

func TestWriterIsSingleUse(t *testing.T) {
	keys, b := testKeys(t), testBackend(t)

	w, err := NewWriter(keys, t.TempDir(), crypto.DeterministicReader("reuse"))
	if err != nil {
		t.Fatalf("new writer: %v", err)
	}
	id, payload := chunkOf(keys, []byte("only chunk"))
	if err := w.Add(id, payload); err != nil {
		t.Fatalf("add: %v", err)
	}
	if _, _, _, err := w.Finish(context.Background(), b); err != nil {
		t.Fatalf("finish: %v", err)
	}

	if err := w.Add(id, payload); err == nil {
		t.Error("add after finish: got nil error")
	}
	if _, _, _, err := w.Finish(context.Background(), b); err == nil {
		t.Error("second finish: got nil error")
	}
	w.Abort() // must not panic or remove anything twice
}

func TestAbortLeavesNoSpoolFile(t *testing.T) {
	keys := testKeys(t)
	dir := t.TempDir()

	w, err := NewWriter(keys, dir, crypto.DeterministicReader("abort"))
	if err != nil {
		t.Fatalf("new writer: %v", err)
	}
	id, payload := chunkOf(keys, []byte("discarded"))
	if err := w.Add(id, payload); err != nil {
		t.Fatalf("add: %v", err)
	}
	w.Abort()

	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("read dir: %v", err)
	}
	if len(entries) != 0 {
		t.Errorf("spool directory still holds %d files after Abort", len(entries))
	}
}

// reseal replaces a pack's trailer with one this test constructs, keeping
// the chunk data and the tail structure intact.
func reseal(t *testing.T, keys *crypto.Keys, original []byte, entries []Entry, version uint64) []byte {
	t.Helper()

	encoded, err := crypto.Marshal(trailer{Version: version, Entries: entries})
	if err != nil {
		t.Fatalf("marshal trailer: %v", err)
	}
	sealed, err := crypto.Seal(&keys.Index, []byte(crypto.AADPackTrailer), encoded, crypto.DeterministicReader("reseal"))
	if err != nil {
		t.Fatalf("seal trailer: %v", err)
	}

	dataEnd := entries[len(entries)-1].End()
	out := append(bytes.Clone(original[:dataEnd]), sealed...)
	return append(out, encodeTail(uint64(len(sealed)))...)
}

func incompressible(t *testing.T, seed string, n int) []byte {
	t.Helper()

	b := make([]byte, n)
	if _, err := io.ReadFull(crypto.DeterministicReader(seed), b); err != nil {
		t.Fatalf("generate test data: %v", err)
	}
	return b
}

// A complete, tiny pack, byte for byte. If this file changes, the storage
// format changed, and every repository written by an older build is now
// being read by a build that disagrees with it.
func TestGoldenPack(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	id, entries := writePack(t, b, keys, "golden-pack",
		[]byte("first chunk"),
		bytes.Repeat([]byte("compress me "), 64),
		nil,
	)

	raw, err := backend.GetAll(ctx, b, Key(id))
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	var out strings.Builder
	fmt.Fprintf(&out, "pack %s\n", id)
	fmt.Fprintf(&out, "size %d\n", len(raw))
	for i, e := range entries {
		fmt.Fprintf(&out, "entry %d %s %d %d\n", i, e.ID, e.Offset, e.Length)
	}
	fmt.Fprintf(&out, "bytes %s\n", hex.EncodeToString(raw))

	// Decoding the golden back proves the trailer says what the entries
	// say. Without it, a change that encoded the trailer wrongly but
	// consistently would only report "the format changed", with no
	// indication of where.
	decoded, err := ReadTrailer(ctx, b, keys, id)
	if err != nil {
		t.Fatalf("read trailer: %v", err)
	}
	if len(decoded) != len(entries) {
		t.Fatalf("trailer lists %d entries, the writer reported %d", len(decoded), len(entries))
	}
	for i := range entries {
		if decoded[i] != entries[i] {
			t.Errorf("entry %d: trailer says %+v, writer said %+v", i, decoded[i], entries[i])
		}
	}

	path := filepath.Join("testdata", "pack.txt")
	if *update {
		if err := os.WriteFile(path, []byte(out.String()), 0o644); err != nil {
			t.Fatalf("write golden: %v", err)
		}
		return
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read golden: %v (run: go test ./internal/pack/ -update)", err)
	}
	if out.String() != string(want) {
		t.Error("the pack format changed: existing repositories were written by a build that disagrees with this one")
	}
}

// Structural damage a reader must refuse, one mutation at a time.
func TestReaderRejectsDamagedPacks(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	id, entries := writePack(t, b, keys, "damage",
		[]byte("chunk one"),
		incompressible(t, "damage-payload", 5000),
	)
	original, err := backend.GetAll(ctx, b, Key(id))
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	cases := []struct {
		name    string
		mutate  func([]byte) []byte
		wantErr error
		// structural means the damage is visible without reading chunk
		// data, so opening the pack already fails.
		structural bool
	}{
		{"truncated tail", func(p []byte) []byte { return p[:len(p)-1] }, ErrNotAPack, true},
		{"truncated to nothing", func([]byte) []byte { return nil }, ErrNotAPack, true},
		{"corrupted magic", func(p []byte) []byte {
			p = bytes.Clone(p)
			p[len(p)-3] ^= 0xff
			return p
		}, ErrNotAPack, true},
		{"future version", func(p []byte) []byte {
			p = bytes.Clone(p)
			p[len(p)-1] = Version + 1
			return p
		}, ErrUnsupportedVersion, true},
		{"absurd trailer length", func(p []byte) []byte {
			p = bytes.Clone(p)
			for i := range lengthSize {
				p[len(p)-tailSize+i] = 0xff
			}
			return p
		}, ErrCorrupt, true},
		{"trailer from another version", func(p []byte) []byte {
			p = bytes.Clone(p)
			// Keep the tail valid but re-seal a trailer claiming v2.
			return reseal(t, keys, p, entries, Version+1)
		}, ErrUnsupportedVersion, true},
		{"flipped trailer byte", func(p []byte) []byte {
			p = bytes.Clone(p)
			p[len(p)-tailSize-1] ^= 0x01
			return p
		}, crypto.ErrDecrypt, true},
		{"flipped chunk byte", func(p []byte) []byte {
			p = bytes.Clone(p)
			p[entries[1].Offset+crypto.NonceSize+3] ^= 0x01
			return p
		}, crypto.ErrDecrypt, false},
		{"flipped nonce byte", func(p []byte) []byte {
			p = bytes.Clone(p)
			p[entries[0].Offset] ^= 0x01
			return p
		}, crypto.ErrDecrypt, false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			damaged := testBackend(t)
			if err := backend.PutBytesIfAbsent(ctx, damaged, Key(id), tc.mutate(original)); err != nil {
				t.Fatalf("store damaged pack: %v", err)
			}

			r, err := OpenReader(ctx, damaged, keys, id)
			if tc.structural {
				if !errors.Is(err, tc.wantErr) {
					t.Fatalf("open: err = %v, want %v", err, tc.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("open: %v", err)
			}

			// Damage inside chunk data is invisible to a structural read
			// and only surfaces when the data itself is verified.
			if err := r.VerifyAll(ctx); !errors.Is(err, tc.wantErr) && !errors.Is(err, ErrCorrupt) {
				t.Fatalf("verify all: err = %v, want %v", err, tc.wantErr)
			}
		})
	}
}

// The trailer is authenticated, but authentic is not the same as
// consistent: a pack written by a buggy client carries a valid tag.
func TestReaderRejectsInconsistentTrailer(t *testing.T) {
	ctx := context.Background()
	keys := testKeys(t)

	build := func(t *testing.T, entries []Entry, data []byte) error {
		t.Helper()
		b := testBackend(t)

		encoded, err := crypto.Marshal(trailer{Version: Version, Entries: entries})
		if err != nil {
			t.Fatalf("marshal: %v", err)
		}
		sealed, err := crypto.Seal(&keys.Index, []byte(crypto.AADPackTrailer), encoded, crypto.DeterministicReader("forged"))
		if err != nil {
			t.Fatalf("seal: %v", err)
		}

		// v2: the pack opens with the header magic and chunk data starts at
		// offset magicSize, so every forged entry is placed after it.
		raw := append(bytes.Clone(magic[:]), data...)
		raw = append(raw, sealed...)
		raw = append(raw, encodeTail(uint64(len(sealed)))...)

		id := crypto.CiphertextID(raw)
		if err := backend.PutBytesIfAbsent(ctx, b, Key(id), raw); err != nil {
			t.Fatalf("store: %v", err)
		}
		_, err = OpenReader(ctx, b, keys, id)
		return err
	}

	sealedLen := uint64(crypto.Overhead + 1)
	data := make([]byte, 2*sealedLen)
	dataStart := uint64(magicSize)

	cases := map[string][]Entry{
		"gap between chunks": {
			{ID: crypto.ID{1}, Offset: dataStart, Length: sealedLen},
			{ID: crypto.ID{2}, Offset: dataStart + sealedLen + 1, Length: sealedLen - 1},
		},
		"entry past the data": {
			{ID: crypto.ID{1}, Offset: dataStart, Length: sealedLen},
			{ID: crypto.ID{2}, Offset: dataStart + sealedLen, Length: sealedLen + 100},
		},
		"entry shorter than an envelope": {
			{ID: crypto.ID{1}, Offset: dataStart, Length: crypto.Overhead},
		},
		"first entry not after the header": {
			{ID: crypto.ID{1}, Offset: 0, Length: sealedLen},
			{ID: crypto.ID{2}, Offset: sealedLen, Length: sealedLen},
		},
		"duplicate chunk": {
			{ID: crypto.ID{1}, Offset: dataStart, Length: sealedLen},
			{ID: crypto.ID{1}, Offset: dataStart + sealedLen, Length: sealedLen},
		},
		"entries do not cover the data": {
			{ID: crypto.ID{1}, Offset: dataStart, Length: sealedLen},
		},
		"no entries": {},
		"chunk longer than the format allows": {
			{ID: crypto.ID{1}, Offset: dataStart, Length: maxSealedChunk + 1},
		},
	}

	for name, entries := range cases {
		t.Run(name, func(t *testing.T) {
			if err := build(t, entries, data); !errors.Is(err, ErrCorrupt) {
				t.Fatalf("open: err = %v, want ErrCorrupt", err)
			}
		})
	}
}

// A pack written for one repository must not open in another.
func TestTrailerIsBoundToTheRepositoryKeys(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)
	id, _ := writePack(t, b, keys, "keys", []byte("payload"))

	otherMaster := goldenMaster
	otherMaster[0] ^= 0xff
	other := crypto.DeriveKeys(otherMaster)

	if _, err := OpenReader(ctx, b, other, id); !errors.Is(err, crypto.ErrDecrypt) {
		t.Fatalf("open with the wrong keys: err = %v, want ErrDecrypt", err)
	}
}

// A chunk is never larger than the chunker's maximum, so a zstd frame
// claiming more is a decompression bomb. The decoder is configured to
// refuse it rather than allocate what it asks for.
func TestDecompressionBombIsRefused(t *testing.T) {
	oversized := make([]byte, 2*chunker.MaxSize)

	enc, err := getEncoder()
	if err != nil {
		t.Fatalf("encoder: %v", err)
	}
	bomb := enc.EncodeAll(oversized, nil)
	t.Logf("frame is %d bytes and claims to decode to %d", len(bomb), len(oversized))

	out, err := decompress(algorithmZstd, bomb)
	if err == nil {
		t.Fatalf("decompress returned %d bytes; the decoder is not bounded by chunker.MaxSize", len(out))
	}
	t.Logf("refused: %v", err)
}

func TestDecompressRejectsAnUnknownEncoding(t *testing.T) {
	if _, err := decompress(0xfe, []byte("payload")); !errors.Is(err, ErrCorrupt) {
		t.Fatalf("decompress with an unknown algorithm byte: err = %v, want ErrCorrupt", err)
	}
}

// A pack listing one chunk twice fails its own trailer consistency check
// and is unreadable, so the writer refuses the duplicate rather than
// producing it. Callers deduplicate before they get here; this is the
// backstop that turns a caller bug into an error instead of a corrupt
// object.
func TestWriterRefusesADuplicateChunk(t *testing.T) {
	keys := testKeys(t)

	w, err := NewWriter(keys, t.TempDir(), crypto.DeterministicReader("duplicate"))
	if err != nil {
		t.Fatalf("new writer: %v", err)
	}
	defer w.Abort()

	id, payload := chunkOf(keys, []byte("repeated content"))
	if err := w.Add(id, payload); err != nil {
		t.Fatalf("first add: %v", err)
	}
	if err := w.Add(id, payload); !errors.Is(err, ErrDuplicateChunk) {
		t.Fatalf("second add: err = %v, want ErrDuplicateChunk", err)
	}
	if w.Count() != 1 {
		t.Errorf("pack holds %d chunks, want 1", w.Count())
	}
}
