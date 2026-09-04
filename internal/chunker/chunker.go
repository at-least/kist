package chunker

import (
	"errors"
	"fmt"
	"io"
)

// Chunk size parameters. These are format, not tuning: a repository
// written with different numbers deduplicates against nothing written
// with these.
const (
	// MinSize is the shortest chunk the splitter will emit, except for a
	// final short chunk at end of file.
	MinSize = 512 << 10 // 512 KiB

	// AvgSize is the target chunk size the gear hash masks are tuned to.
	AvgSize = 2 << 20 // 2 MiB

	// MaxSize is the longest chunk, and therefore the largest buffer any
	// single-chunk operation has to hold.
	MaxSize = 8 << 20 // 8 MiB

	// bufSize is the working buffer. It must exceed MaxSize so that the
	// splitter always sees a full maximum-length window before deciding a
	// boundary; that is what makes boundaries independent of how the
	// input happens to arrive from the reader.
	bufSize = 2 * MaxSize
)

// FastCDC normalisation, level 2. The splitter uses a stricter mask
// before the average size is reached and a looser one after, which pulls
// the size distribution in towards AvgSize instead of the long tail a
// single mask produces.
//
//	bits      = round(log2(AvgSize)) = round(log2(2^21)) = 21
//	maskSmall = 1<<(bits+2) - 1      = 1<<23 - 1
//	maskLarge = 1<<(bits-2) - 1      = 1<<19 - 1
//
// Computed once here rather than with math.Log2 at run time, because a
// floating-point rounding difference between platforms would silently
// fork the format.
const (
	maskSmall uint64 = 1<<23 - 1
	maskLarge uint64 = 1<<19 - 1
)

// A Chunk is one content-defined span of the input.
type Chunk struct {
	// Offset is the position of the chunk in the stream.
	Offset int64

	// Data is the chunk contents. It points into the chunker's internal
	// buffer and is only valid until the next call to Next: a caller that
	// keeps it must copy it.
	Data []byte
}

// A Chunker splits a stream into content-defined chunks.
//
// The algorithm is FastCDC (Xia et al., USENIX ATC '16). kist implements
// it rather than importing one, for two reasons recorded in ADR 002: the
// boundary function is part of the frozen storage format, so an upstream
// patch release must not be able to move it; and fastcdc-go v0.2.0 XORs
// its seed into a package-level table, which `go test -race` shows to be
// a live data race between one goroutine constructing a chunker and
// another chunking. The output here is byte-for-byte identical to that
// library at kist's parameters, which was verified against it before the
// dependency was dropped.
//
// A Chunker is not safe for concurrent use; chunking many files at once
// means one Chunker per file, which share nothing.
type Chunker struct {
	r      io.Reader
	buf    []byte
	cursor int
	offset int64
	eof    bool
}

// New returns a Chunker reading from r.
func New(r io.Reader) (*Chunker, error) {
	if r == nil {
		return nil, errors.New("create chunker: nil reader")
	}
	return &Chunker{
		r:      r,
		buf:    make([]byte, bufSize),
		cursor: bufSize, // empty: fill on the first Next
	}, nil
}

// Next returns the next chunk, or io.EOF after the last one.
//
// An empty input yields io.EOF immediately: a zero-length file has no
// chunks and is described entirely by its tree entry.
func (c *Chunker) Next() (Chunk, error) {
	if err := c.fill(); err != nil {
		return Chunk{}, fmt.Errorf("read chunk at offset %d: %w", c.offset, err)
	}
	if len(c.buf) == 0 {
		return Chunk{}, io.EOF
	}

	length := boundary(c.buf[c.cursor:])
	chunk := Chunk{Offset: c.offset, Data: c.buf[c.cursor : c.cursor+length]}

	c.cursor += length
	c.offset += int64(length)
	return chunk, nil
}

// fill guarantees that at least MaxSize bytes are available after the
// cursor, unless the input is exhausted. Without that guarantee a
// boundary could be decided on a short window and would then depend on
// the size of the reader's writes rather than on the content.
func (c *Chunker) fill() error {
	remaining := len(c.buf) - c.cursor
	if remaining >= MaxSize {
		return nil
	}

	copy(c.buf[:remaining], c.buf[c.cursor:])
	c.cursor = 0
	if c.eof {
		c.buf = c.buf[:remaining]
		return nil
	}

	n, err := io.ReadFull(c.r, c.buf[remaining:cap(c.buf)])
	switch {
	case errors.Is(err, io.EOF), errors.Is(err, io.ErrUnexpectedEOF):
		c.buf = c.buf[:remaining+n]
		c.eof = true
	case err != nil:
		return err
	default:
		c.buf = c.buf[:cap(c.buf)]
	}
	return nil
}

// boundary returns the length of the next chunk starting at data[0].
//
// The gear hash restarts at zero for every chunk and the first MinSize
// bytes are not hashed at all: a boundary can only be declared once the
// minimum has been passed, so hashing before that would be wasted work
// and, more importantly, would change where the boundaries fall.
func boundary(data []byte) int {
	if len(data) <= MinSize {
		return len(data)
	}

	limit := min(len(data), MaxSize)
	normal := min(limit, AvgSize)

	var fp uint64
	i := MinSize

	// Below the average size, the stricter mask makes a cut less likely,
	// which suppresses very short chunks.
	for ; i < normal; i++ {
		fp = (fp << 1) + gearTable[data[i]]
		if fp&maskSmall == 0 {
			return i + 1
		}
	}
	// Above it, the looser mask makes a cut more likely, which suppresses
	// very long ones.
	for ; i < limit; i++ {
		fp = (fp << 1) + gearTable[data[i]]
		if fp&maskLarge == 0 {
			return i + 1
		}
	}

	return i
}
