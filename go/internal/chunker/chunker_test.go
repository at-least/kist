package chunker

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"testing/iotest"

	"lukechampine.com/blake3"

	"github.com/at-least/kist/internal/crypto"
)

// pseudorandom returns n incompressible bytes that are the same on every
// run and on every platform, so that boundary goldens mean something.
func pseudorandom(t *testing.T, seed string, n int) []byte {
	t.Helper()

	b := make([]byte, n)
	if _, err := io.ReadFull(crypto.DeterministicReader(seed), b); err != nil {
		t.Fatalf("generate test data: %v", err)
	}
	return b
}

func chunkAll(t *testing.T, data []byte) [][]byte {
	t.Helper()

	c, err := New(bytes.NewReader(data))
	if err != nil {
		t.Fatalf("new chunker: %v", err)
	}

	var chunks [][]byte
	var offset int64
	for {
		chunk, err := c.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if chunk.Offset != offset {
			t.Fatalf("chunk offset = %d, want %d", chunk.Offset, offset)
		}
		offset += int64(len(chunk.Data))
		chunks = append(chunks, bytes.Clone(chunk.Data))
	}
	if offset != int64(len(data)) {
		t.Fatalf("chunks cover %d bytes, want %d", offset, len(data))
	}
	return chunks
}

func TestChunksReassembleTheInput(t *testing.T) {
	for _, size := range []int{0, 1, 1000, MinSize - 1, MinSize, MinSize + 1, 3 * AvgSize, 2*MaxSize + 12345} {
		t.Run(fmt.Sprintf("%d bytes", size), func(t *testing.T) {
			data := pseudorandom(t, "reassemble", size)

			var got []byte
			for _, chunk := range chunkAll(t, data) {
				got = append(got, chunk...)
			}
			if !bytes.Equal(got, data) {
				t.Errorf("reassembled %d bytes, want %d identical", len(got), len(data))
			}
		})
	}
}

func TestEmptyInputProducesNoChunks(t *testing.T) {
	if chunks := chunkAll(t, nil); len(chunks) != 0 {
		t.Errorf("empty input produced %d chunks, want 0", len(chunks))
	}
}

// Every chunk but the last must sit inside the configured bounds. A
// chunker that ignores its minimum would explode the index; one that
// ignores its maximum would break the fixed-size buffers downstream.
func TestChunkSizesRespectBounds(t *testing.T) {
	data := pseudorandom(t, "bounds", 40<<20)
	chunks := chunkAll(t, data)

	if len(chunks) < 2 {
		t.Fatalf("got %d chunks from 40 MiB, expected many", len(chunks))
	}
	for i, chunk := range chunks {
		if len(chunk) > MaxSize {
			t.Errorf("chunk %d is %d bytes, over the %d maximum", i, len(chunk), MaxSize)
		}
		if i < len(chunks)-1 && len(chunk) < MinSize {
			t.Errorf("chunk %d is %d bytes, under the %d minimum", i, len(chunk), MinSize)
		}
	}
}

// The whole point: inserting bytes at the front must not renumber every
// boundary after it. A fixed-size splitter would fail this outright.
func TestBoundariesSurviveAnInsertionAtTheFront(t *testing.T) {
	base := pseudorandom(t, "shift", 24<<20)
	shifted := append(pseudorandom(t, "prefix", 4096), base...)

	shared := map[[32]byte]bool{}
	for _, chunk := range chunkAll(t, base) {
		shared[blake3.Sum256(chunk)] = true
	}

	var reused int
	after := chunkAll(t, shifted)
	for _, chunk := range after {
		if shared[blake3.Sum256(chunk)] {
			reused++
		}
	}

	// Resynchronisation should cost one or two chunks, not the file.
	if reused < len(after)-3 {
		t.Errorf("only %d of %d chunks were reused after a 4 KiB insertion; the splitter is not content-defined", reused, len(after))
	}
}

func TestChunkingIsDeterministic(t *testing.T) {
	data := pseudorandom(t, "determinism", 12<<20)

	first := chunkAll(t, data)
	second := chunkAll(t, data)
	if len(first) != len(second) {
		t.Fatalf("two runs produced %d and %d chunks", len(first), len(second))
	}
	for i := range first {
		if !bytes.Equal(first[i], second[i]) {
			t.Fatalf("chunk %d differs between two runs of the same input", i)
		}
	}
}

// Reading the input in small pieces must not move a boundary: the
// splitter has to depend on content alone, not on how the bytes arrived.
func TestBoundariesDoNotDependOnReadSizes(t *testing.T) {
	data := pseudorandom(t, "readsize", 12<<20)
	want := chunkAll(t, data)

	c, err := New(iotest.OneByteReader(bytes.NewReader(data)))
	if err != nil {
		t.Fatalf("new chunker: %v", err)
	}
	var got [][]byte
	for {
		chunk, err := c.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		got = append(got, bytes.Clone(chunk.Data))
	}

	if len(got) != len(want) {
		t.Fatalf("dripped input produced %d chunks, want %d", len(got), len(want))
	}
	for i := range want {
		if !bytes.Equal(got[i], want[i]) {
			t.Fatalf("chunk %d differs when the input is read one byte at a time", i)
		}
	}
}

func TestNextPropagatesReadErrors(t *testing.T) {
	boom := errors.New("device fell off the bus")

	c, err := New(io.MultiReader(bytes.NewReader(pseudorandom(t, "err", 1<<20)), failingReader{boom}))
	if err != nil {
		t.Fatalf("new chunker: %v", err)
	}
	if _, err := c.Next(); !errors.Is(err, boom) {
		t.Fatalf("next: err = %v, want the reader error", err)
	}
}

type failingReader struct{ err error }

func (f failingReader) Read([]byte) (int, error) { return 0, f.err }

// Chunkers share nothing, so many of them run at once without
// coordination. This is the test that failed with a data race while the
// splitter was still fastcdc-go, whose package-level table it mutates in
// its constructor; see ADR 002.
func TestConcurrentChunkersAreSafe(t *testing.T) {
	data := pseudorandom(t, "concurrent", 6<<20)
	want := chunkAll(t, data)

	var wg sync.WaitGroup
	for range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()

			c, err := New(bytes.NewReader(data))
			if err != nil {
				t.Errorf("new chunker: %v", err)
				return
			}
			for i := 0; ; i++ {
				chunk, err := c.Next()
				if errors.Is(err, io.EOF) {
					if i != len(want) {
						t.Errorf("got %d chunks, want %d", i, len(want))
					}
					return
				}
				if err != nil {
					t.Errorf("next: %v", err)
					return
				}
				if i < len(want) && !bytes.Equal(chunk.Data, want[i]) {
					t.Errorf("chunk %d differs under concurrency", i)
					return
				}
			}
		}()
	}
	wg.Wait()
}

// Boundary offsets are part of the frozen format even though nothing
// writes them down: two builds that disagree here deduplicate against
// nothing the other wrote. This golden is what would catch an upstream
// change to fastcdc-go, or an accidental edit to the size constants.
func TestGoldenBoundaries(t *testing.T) {
	data := pseudorandom(t, "golden-boundaries", 64<<20)

	var b strings.Builder
	fmt.Fprintf(&b, "# fastcdc min=%d avg=%d max=%d maskS=%#x maskL=%#x\n", MinSize, AvgSize, MaxSize, DefaultParams().maskSmall(), DefaultParams().maskLarge())
	fmt.Fprintf(&b, "# input: 64 MiB of crypto.DeterministicReader(\"golden-boundaries\")\n")
	fmt.Fprintf(&b, "# offset length\n")

	var offset int64
	for _, chunk := range chunkAll(t, data) {
		fmt.Fprintf(&b, "%d %d\n", offset, len(chunk))
		offset += int64(len(chunk))
	}

	path := filepath.Join("testdata", "boundaries.txt")
	if *update {
		if err := os.WriteFile(path, []byte(b.String()), 0o644); err != nil {
			t.Fatalf("write golden: %v", err)
		}
		return
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read golden: %v (run: go test ./internal/chunker/ -update)", err)
	}
	if b.String() != string(want) {
		t.Error("chunk boundaries changed: the on-disk format has moved and existing repositories would stop deduplicating")
	}
}

// The gear table is format: one changed entry moves every boundary in
// every future backup. It was extracted verbatim from fastcdc-go v0.2.0,
// and kist's output was verified byte-for-byte identical to that library
// over nine inputs (empty, 1 B, MinSize-1/MinSize/MinSize+1, 3*AvgSize,
// 20 MiB random, 24 MiB of repeated text, 30 MiB of zeroes) before the
// dependency was dropped. This digest is what is left of that check.
func TestGearTableDigest(t *testing.T) {
	var buf [8 * len(gearTable)]byte
	for i, v := range gearTable {
		binary.BigEndian.PutUint64(buf[i*8:], v)
	}

	digest := blake3.Sum256(buf[:])

	const want = "d125031f927a16217e0e3f94a1308789a3a6ce83623c1a6a76514ed67fcf4504"
	if got := hex.EncodeToString(digest[:]); got != want {
		t.Errorf("gear table digest = %s, want %s", got, want)
	}
}

// A reset chunker must find exactly the boundaries a fresh one finds,
// whatever it read before: nothing of the previous file may leak into
// the next.
func TestResetChunksLikeAFreshChunker(t *testing.T) {
	first := pseudorandom(t, "reset-first", 3*MinSize+123)
	second := pseudorandom(t, "reset-second", 5*MinSize+7)
	small := []byte("tiny")

	c, err := New(bytes.NewReader(first))
	if err != nil {
		t.Fatal(err)
	}
	drain := func() [][]byte {
		var out [][]byte
		for {
			chunk, err := c.Next()
			if errors.Is(err, io.EOF) {
				return out
			}
			if err != nil {
				t.Fatal(err)
			}
			out = append(out, bytes.Clone(chunk.Data))
		}
	}
	// Leave the first input half-consumed, then reset: a reset in the
	// middle of a file must not carry the rest of it over.
	if _, err := c.Next(); err != nil {
		t.Fatal(err)
	}
	for _, data := range [][]byte{second, small, nil, first} {
		if err := c.Reset(bytes.NewReader(data)); err != nil {
			t.Fatal(err)
		}
		got := drain()
		want := chunkAll(t, data)
		if len(got) != len(want) {
			t.Fatalf("%d chunks after reset, fresh chunker gives %d", len(got), len(want))
		}
		for i := range want {
			if !bytes.Equal(got[i], want[i]) {
				t.Fatalf("chunk %d differs after reset", i)
			}
		}
	}
	if err := c.Reset(nil); err == nil {
		t.Error("Reset(nil) succeeded")
	}
}
