package chunker

import (
	"bytes"
	"errors"
	"io"
	"testing"
	"testing/iotest"
)

// FuzzChunker: whatever the input, the chunks concatenate back to it,
// every chunk but the last is within bounds, and the boundaries do not
// depend on how the reader delivers bytes.
func FuzzChunker(f *testing.F) {
	f.Add([]byte{})
	f.Add([]byte("short"))
	f.Add(bytes.Repeat([]byte{0}, MinSize+1))
	seed := make([]byte, MinSize*3)
	x := uint32(2463534242)
	for i := range seed {
		x ^= x << 13
		x ^= x >> 17
		x ^= x << 5
		seed[i] = byte(x)
	}
	f.Add(seed)

	f.Fuzz(func(t *testing.T, data []byte) {
		chunks := chunkReader(t, bytes.NewReader(data))
		var joined []byte
		for i, c := range chunks {
			if len(c) == 0 {
				t.Fatalf("chunk %d is empty", i)
			}
			if len(c) > MaxSize {
				t.Fatalf("chunk %d is %d bytes, over MaxSize", i, len(c))
			}
			if i < len(chunks)-1 && len(c) < MinSize {
				t.Fatalf("chunk %d is %d bytes, under MinSize and not last", i, len(c))
			}
			joined = append(joined, c...)
		}
		if !bytes.Equal(joined, data) {
			t.Fatal("chunks do not reassemble the input")
		}
		if len(data) < 64<<10 { // one-byte reads are slow; keep it to small inputs
			again := chunkReader(t, iotest.OneByteReader(bytes.NewReader(data)))
			if len(again) != len(chunks) {
				t.Fatalf("%d chunks with one-byte reads, %d with a plain reader", len(again), len(chunks))
			}
			for i := range chunks {
				if !bytes.Equal(chunks[i], again[i]) {
					t.Fatalf("chunk %d differs under one-byte reads", i)
				}
			}
		}
	})
}

func chunkReader(t *testing.T, r io.Reader) [][]byte {
	t.Helper()
	c, err := New(r)
	if err != nil {
		t.Fatal(err)
	}
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
