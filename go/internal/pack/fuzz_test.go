package pack

import (
	"bytes"
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/chunker"
	"github.com/at-least/kist/internal/crypto"
)

// memBackend holds one object in memory: what a fuzz iteration needs,
// and nothing that touches a disk.
type memBackend struct {
	key  string
	data []byte
}

func (m *memBackend) Location() string { return "memory" }
func (m *memBackend) Close() error     { return nil }

func (m *memBackend) Get(_ context.Context, key string, off, length int64) (io.ReadCloser, error) {
	if key != m.key {
		return nil, backend.ErrNotFound
	}
	if off < 0 || off > int64(len(m.data)) {
		return nil, fmt.Errorf("get %s: offset %d out of range", key, off)
	}
	end := int64(len(m.data))
	if length != backend.ReadToEnd && off+length < end {
		end = off + length
	}
	return io.NopCloser(bytes.NewReader(m.data[off:end])), nil
}

func (m *memBackend) Stat(_ context.Context, key string) (backend.FileInfo, error) {
	if key != m.key {
		return backend.FileInfo{}, backend.ErrNotFound
	}
	return backend.FileInfo{Key: key, Size: int64(len(m.data))}, nil
}

func (m *memBackend) Put(context.Context, string, io.Reader, int64) error {
	return errors.ErrUnsupported
}
func (m *memBackend) PutIfAbsent(context.Context, string, io.Reader, int64) error {
	return errors.ErrUnsupported
}
func (m *memBackend) Delete(context.Context, string) error { return errors.ErrUnsupported }
func (m *memBackend) List(context.Context, string, func(backend.FileInfo) error) error {
	return errors.ErrUnsupported
}

// goldenPackBytes returns the pack from testdata, the one real pack the
// fuzzer starts from.
func goldenPackBytes(t testing.TB) []byte {
	t.Helper()
	text, err := os.ReadFile("testdata/pack.txt")
	if err != nil {
		t.Fatal(err)
	}
	for _, line := range strings.Split(string(text), "\n") {
		if rest, ok := strings.CutPrefix(line, "bytes "); ok {
			data, err := hex.DecodeString(strings.TrimSpace(rest))
			if err != nil {
				t.Fatal(err)
			}
			return data
		}
	}
	t.Fatal("no bytes line in testdata/pack.txt")
	return nil
}

func fuzzKeys() *crypto.Keys {
	var master crypto.Key
	for i := range master {
		master[i] = byte(i)
	}
	return crypto.DeriveKeys(master)
}

// FuzzReadTrailer feeds arbitrary bytes to the trailer reader and, when
// it accepts them, to the full reader. Neither may panic, and any
// trailer accepted must describe entries that tile the data region.
func FuzzReadTrailer(f *testing.F) {
	golden := goldenPackBytes(f)
	f.Add(golden)
	f.Add(golden[:len(golden)-1])
	f.Add(golden[1:])
	f.Add([]byte{})
	f.Add([]byte("kistpk\x00\x01"))
	flipped := bytes.Clone(golden)
	flipped[len(flipped)/2] ^= 0x40
	f.Add(flipped)

	keys := fuzzKeys()
	f.Fuzz(func(t *testing.T, data []byte) {
		id := crypto.CiphertextID(data)
		b := &memBackend{key: Key(id), data: data}
		ctx := context.Background()

		entries, err := ReadTrailer(ctx, b, keys, id)
		if err != nil {
			return
		}
		// v2: chunk data begins right after the header magic.
		var next uint64 = magicSize
		for i, e := range entries {
			if e.Offset != next {
				t.Fatalf("entry %d starts at %d, expected %d", i, e.Offset, next)
			}
			if e.Length == 0 {
				t.Fatalf("entry %d is empty", i)
			}
			next = e.End()
		}
		if next > uint64(len(data)) {
			t.Fatalf("entries reach %d, past the %d-byte pack", next, len(data))
		}

		r, err := OpenReader(ctx, b, keys, id)
		if err != nil {
			return
		}
		_ = r.VerifyAll(ctx) //nolint:errcheck // must not panic; an error is a valid answer
		for _, e := range entries {
			if data, err := r.Chunk(ctx, e); err == nil && len(data) > chunker.MaxSize {
				t.Fatalf("chunk decoded to %d bytes, over MaxSize", len(data))
			}
		}
	})
}

// FuzzDecompress: the algorithm byte and payload come off the wire
// after authentication, but a bug in the compressor's own writer is not
// something authentication catches.
func FuzzDecompress(f *testing.F) {
	alg, compressed, err := compress(bytes.Repeat([]byte("compressible "), 10000))
	if err != nil {
		f.Fatal(err)
	}
	f.Add(alg, compressed)
	f.Add(byte(0), []byte("raw bytes"))
	f.Add(byte(1), []byte{0x28, 0xb5, 0x2f, 0xfd})
	f.Add(byte(7), []byte("unknown"))
	f.Fuzz(func(t *testing.T, algorithm byte, payload []byte) {
		out, err := decompress(algorithm, payload)
		if err != nil {
			return
		}
		if len(out) > chunker.MaxSize {
			t.Fatalf("decompressed to %d bytes, over MaxSize", len(out))
		}
		if algorithm == 0 && !bytes.Equal(out, payload) {
			t.Fatal("raw payload came back changed")
		}
	})
}
