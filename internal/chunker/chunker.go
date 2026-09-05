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
// bits is derived with integer arithmetic (see roundLog2): a
// floating-point rounding difference between platforms would silently
// fork the format.

// Params are the chunk sizes one repository was created with. They are
// format, not tuning: clients of the same repository must agree, which
// the config and the master-key AAD both enforce.
type Params struct {
	Min uint32
	Avg uint32
	Max uint32
}

// DefaultParams is what repositories created by this build use.
func DefaultParams() Params {
	return Params{Min: MinSize, Avg: AvgSize, Max: MaxSize}
}

func (p Params) maskSmall() uint64 { return 1<<(roundLog2(p.Avg)+2) - 1 }
func (p Params) maskLarge() uint64 { return 1<<(roundLog2(p.Avg)-2) - 1 }

// roundLog2 is round(log2(v)) in exact integer arithmetic: v is rounded
// up to the next power of two when v >= 2^b*sqrt(2), tested as
// v*v >= 2^(2b+1). The Rust implementation derives its masks from the
// same expression; changing either one forks the format.
func roundLog2(v uint32) uint {
	b := uint(0)
	for 1<<(b+1) <= v {
		b++
	}
	vv := uint64(v) * uint64(v)
	if vv >= 1<<(2*b+1) {
		return b + 1
	}
	return b
}

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
	r io.Reader

	params Params

	smallMask uint64
	largeMask uint64

	buf    []byte
	cursor int
	offset int64
	eof    bool
}

// New returns a Chunker reading from r with the default sizes.
func New(r io.Reader) (*Chunker, error) {
	return NewParams(r, DefaultParams())
}

// NewParams returns a Chunker reading from r with a repository's sizes.
// The buffer is sized from the parameters' maximum.
func NewParams(r io.Reader, params Params) (*Chunker, error) {
	if r == nil {
		return nil, errors.New("create chunker: nil reader")
	}
	bufCap := 2 * int64(params.Max)
	return &Chunker{
		r:         r,
		params:    params,
		smallMask: params.maskSmall(),
		largeMask: params.maskLarge(),
		buf:       make([]byte, bufCap),
		cursor:    int(bufCap), // empty: fill on the first Next
	}, nil
}

// Reset makes the chunker read from r, keeping its buffer.
//
// The buffer is 16 MiB at the default sizes, and allocating and zeroing
// one per file is what dominated a backup of a million small files:
// 16 GB of allocation for a thousand one-kilobyte files, 1.8 ms each.
// A backup keeps one chunker and resets it per file. Chunks handed out
// before the reset point into the buffer and are invalid after it, as
// they already are after the next call to Next.
func (c *Chunker) Reset(r io.Reader) error {
	if r == nil {
		return errors.New("reset chunker: nil reader")
	}
	c.r = r
	c.buf = c.buf[:cap(c.buf)]
	c.cursor = len(c.buf)
	c.offset = 0
	c.eof = false
	return nil
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

	length := c.boundary(c.buf[c.cursor:])
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
	max := int(c.params.Max)
	remaining := len(c.buf) - c.cursor
	if remaining >= max {
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
// The gear hash restarts at zero for every chunk and the first Min
// bytes are not hashed at all: a boundary can only be declared once the
// minimum has been passed, so hashing before that would be wasted work
// and, more importantly, would change where the boundaries fall.
func (c *Chunker) boundary(data []byte) int {
	minSize := int(c.params.Min)
	if len(data) <= minSize {
		return len(data)
	}

	limit := min(len(data), int(c.params.Max))
	normal := min(limit, int(c.params.Avg))

	var fp uint64
	i := minSize

	// Below the average size, the stricter mask makes a cut less likely,
	// which suppresses very short chunks.
	for ; i < normal; i++ {
		fp = (fp << 1) + gearTable[data[i]]
		if fp&c.smallMask == 0 {
			return i + 1
		}
	}
	// Above it, the looser mask makes a cut more likely, which suppresses
	// very long ones.
	for ; i < limit; i++ {
		fp = (fp << 1) + gearTable[data[i]]
		if fp&c.largeMask == 0 {
			return i + 1
		}
	}

	return i
}
