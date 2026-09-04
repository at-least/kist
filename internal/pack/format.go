package pack

import (
	"encoding/binary"
	"errors"
	"fmt"
	"math"

	"github.com/at-least/kist/internal/crypto"
)

// Format constants. Everything here is frozen: changing a value changes
// what an existing repository means, which is what the version byte in
// the magic exists to negotiate.
const (
	// Version is the pack format version, carried in the last byte of the
	// magic so that a reader can reject a future pack before it tries to
	// parse one.
	Version = 1

	// TargetSize is the size a writer aims for before flushing. Large
	// enough that per-object overhead on object storage disappears, small
	// enough that a single failed upload is cheap to repeat.
	TargetSize = 64 << 20

	// magicSize and lengthSize describe the fixed tail every pack ends
	// with: an 8-byte big-endian trailer length followed by the magic.
	magicSize  = 8
	lengthSize = 8

	// tailSize is what a reader must have in hand to locate the trailer.
	tailSize = lengthSize + magicSize

	// maxTrailerSize bounds what a reader will allocate from a length
	// field it has not yet authenticated. A 64 MiB pack of minimum-size
	// chunks holds at most 128 entries; 16 MiB is many orders of
	// magnitude above any legitimate trailer.
	maxTrailerSize = 16 << 20
)

// magic is the last 8 bytes of every pack: "kistpk" then two version
// bytes, so a corrupted or foreign object is rejected by inspection
// rather than by a confusing parse failure.
var magic = [magicSize]byte{'k', 'i', 's', 't', 'p', 'k', 0x00, Version}

// Chunk payload encodings. The algorithm is the first byte of the
// authenticated plaintext, which makes a sealed chunk self-describing
// once it is decrypted and keeps the trailer to a plain three-field tuple.
const (
	algorithmRaw  byte = 0
	algorithmZstd byte = 1
)

// Errors a caller may want to distinguish. A pack that fails any of these
// is damaged or is not a pack; `check` reports them, it does not repair.
var (
	// ErrNotAPack means the object does not end with the pack magic.
	ErrNotAPack = errors.New("not a pack file")

	// ErrUnsupportedVersion means the magic is right but the version is
	// one this build does not know how to read.
	ErrUnsupportedVersion = errors.New("unsupported pack version")

	// ErrCorrupt means the pack is structurally invalid: a trailer that
	// does not fit, an entry that points outside the file, and so on.
	ErrCorrupt = errors.New("pack is corrupt")
)

// An Entry locates one chunk inside its pack.
//
// Offset points at the first byte of the sealed chunk -- its nonce -- and
// Length covers the whole sealed form, so (Offset, Length) is exactly the
// byte range a reader must fetch. It is the same tuple the repository
// index stores, so a trailer and an index can never disagree about what
// an entry means.
type Entry struct {
	_ struct{} `cbor:",toarray"`

	ID     crypto.ID
	Offset uint64
	Length uint32
}

// End returns the offset one past the last byte of the chunk.
func (e Entry) End() uint64 { return e.Offset + uint64(e.Length) }

// A trailer is the index a pack carries about itself.
type trailer struct {
	Version uint64  `cbor:"v"`
	Entries []Entry `cbor:"entries"`
}

// encodeTail renders the fixed 16 bytes that close a pack.
func encodeTail(trailerLen uint64) []byte {
	tail := make([]byte, 0, tailSize)
	tail = binary.BigEndian.AppendUint64(tail, trailerLen)
	return append(tail, magic[:]...)
}

// parseTail validates the magic and returns the sealed trailer's length.
func parseTail(tail []byte) (uint64, error) {
	if len(tail) != tailSize {
		return 0, fmt.Errorf("%w: tail is %d bytes, want %d", ErrCorrupt, len(tail), tailSize)
	}

	got := tail[lengthSize:]
	if string(got[:6]) != "kistpk" {
		return 0, fmt.Errorf("%w: tail magic is %x", ErrNotAPack, got)
	}
	if got[6] != 0x00 || got[7] != Version {
		return 0, fmt.Errorf("%w: pack declares version %d.%d, this build reads %d.%d", ErrUnsupportedVersion, got[6], got[7], 0, Version)
	}

	trailerLen := binary.BigEndian.Uint64(tail[:lengthSize])
	if trailerLen > maxTrailerSize {
		return 0, fmt.Errorf("%w: trailer claims %d bytes, over the %d limit", ErrCorrupt, trailerLen, maxTrailerSize)
	}
	if trailerLen < crypto.Overhead {
		return 0, fmt.Errorf("%w: trailer claims %d bytes, shorter than the %d-byte envelope", ErrCorrupt, trailerLen, crypto.Overhead)
	}
	return trailerLen, nil
}

// Key returns the repository key a pack is stored under.
func Key(id crypto.ID) string { return "packs/" + id.String() }

// The format speaks in unsigned sizes and the backend API in signed
// offsets, and the values crossing between them come from a pack's own
// trailer -- which is authenticated, but may still have been written by a
// broken client. These two conversions are checked rather than assumed,
// so a nonsensical size is a corruption error instead of a wrapped
// integer and a wild read.

func offsetOf(v uint64) (int64, error) {
	if v > math.MaxInt64 {
		return 0, fmt.Errorf("%w: offset %d is outside the addressable range", ErrCorrupt, v)
	}
	return int64(v), nil
}

func sizeOf(v int64) (uint64, error) {
	if v < 0 {
		return 0, fmt.Errorf("%w: backend reported a size of %d", ErrCorrupt, v)
	}
	return uint64(v), nil
}
