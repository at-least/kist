package pack

import (
	"context"
	"fmt"
	"io"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

// tailWindow is how much of a pack's end a reader fetches on the first
// request. A 64 MiB pack holds at most a few hundred chunks, so its
// sealed trailer runs to a few kilobytes; 64 KiB covers every realistic
// pack in one round trip, and a larger trailer just costs a second one.
const tailWindow = 64 << 10

// A Reader reads chunks out of one pack.
//
// It holds the pack's trailer, not its data: each chunk is fetched with a
// ranged read when it is asked for, so restoring one file out of a large
// backup does not transfer the packs it happens to share with others.
type Reader struct {
	backend backend.Backend
	keys    *crypto.Keys
	id      crypto.ID
	size    uint64
	entries []Entry
	byID    map[crypto.ID]Entry
}

// OpenReader fetches and authenticates a pack's trailer.
func OpenReader(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID) (*Reader, error) {
	info, err := b.Stat(ctx, Key(id))
	if err != nil {
		return nil, fmt.Errorf("open pack %s: %w", id, err)
	}
	size, err := sizeOf(info.Size)
	if err != nil {
		return nil, fmt.Errorf("open pack %s: %w", id, err)
	}

	entries, err := readTrailer(ctx, b, keys, id, size)
	if err != nil {
		return nil, err
	}

	byID := make(map[crypto.ID]Entry, len(entries))
	for _, e := range entries {
		byID[e.ID] = e
	}
	return &Reader{backend: b, keys: keys, id: id, size: size, entries: entries, byID: byID}, nil
}

// ID is the pack's content address.
func (r *Reader) ID() crypto.ID { return r.id }

// Size is the pack's length in bytes.
func (r *Reader) Size() uint64 { return r.size }

// Entries returns the pack's trailer index, in the order chunks appear in
// the file. The slice is the reader's; callers must not modify it.
func (r *Reader) Entries() []Entry { return r.entries }

// Lookup finds a chunk's entry in this pack.
func (r *Reader) Lookup(id crypto.ID) (Entry, bool) {
	e, ok := r.byID[id]
	return e, ok
}

// Chunk fetches, decrypts, decompresses and verifies one chunk.
//
// The content address is recomputed from the recovered plaintext and
// compared. The AEAD tag already proves the bytes are the ones that were
// sealed under this ID; this second check proves the ID was not a lie
// when the chunk was written, which is the property `check --read-data`
// exists to establish.
func (r *Reader) Chunk(ctx context.Context, entry Entry) ([]byte, error) {
	if entry.End() > r.size {
		return nil, fmt.Errorf("%w: pack %s: chunk %s spans bytes %d-%d of a %d-byte pack", ErrCorrupt, r.id, entry.ID, entry.Offset, entry.End(), r.size)
	}

	offset, err := offsetOf(entry.Offset)
	if err != nil {
		return nil, fmt.Errorf("read chunk %s from pack %s: %w", entry.ID, r.id, err)
	}
	length, err := lengthOf(entry.Length)
	if err != nil {
		return nil, fmt.Errorf("read chunk %s from pack %s: %w", entry.ID, r.id, err)
	}
	sealed, err := readRange(ctx, r.backend, Key(r.id), offset, length)
	if err != nil {
		return nil, fmt.Errorf("read chunk %s from pack %s: %w", entry.ID, r.id, err)
	}
	return r.decodeChunk(entry, sealed)
}

func (r *Reader) decodeChunk(entry Entry, sealed []byte) ([]byte, error) {
	id := entry.ID

	framed, err := crypto.Open(&r.keys.Chunk, id[:], sealed)
	if err != nil {
		return nil, fmt.Errorf("chunk %s in pack %s: %w", id, r.id, err)
	}
	if len(framed) == 0 {
		return nil, fmt.Errorf("%w: chunk %s in pack %s has no encoding byte", ErrCorrupt, id, r.id)
	}

	plaintext, err := decompress(framed[0], framed[1:])
	if err != nil {
		return nil, fmt.Errorf("chunk %s in pack %s: %w", id, r.id, err)
	}
	if got := crypto.ContentID(&r.keys.Hash, plaintext); got != id {
		return nil, fmt.Errorf("%w: chunk in pack %s is stored as %s but hashes to %s", ErrCorrupt, r.id, id, got)
	}
	return plaintext, nil
}

// VerifyAll re-reads the whole pack and checks every chunk in it. It is
// what `check --read-data` runs, and the only operation that can catch a
// bit flip inside chunk data.
//
// The pack is streamed, not loaded: peak memory is one chunk, not one
// pack. That matters because this is the operation most likely to be
// pointed at a repository with thousands of 64 MiB packs.
func (r *Reader) VerifyAll(ctx context.Context) error {
	fail := func(err error) error { return fmt.Errorf("verify pack %s: %w", r.id, err) }

	body, err := r.backend.Get(ctx, Key(r.id), 0, backend.ReadToEnd)
	if err != nil {
		return fail(err)
	}
	defer func() { _ = body.Close() }()

	// Everything read passes through the hasher, so the pack's name is
	// checked against its whole contents without a second pass.
	hasher := crypto.CiphertextHasher()
	stream := io.TeeReader(body, hasher)

	// v2: the pack opens with the header magic; it passes through the
	// hasher like everything else and is checked byte for byte.
	header := make([]byte, magicSize)
	if _, err := io.ReadFull(stream, header); err != nil {
		return fail(fmt.Errorf("read header: %w", err))
	}
	if string(header) != string(magic[:]) {
		return fail(fmt.Errorf("%w: header magic mismatch", ErrNotAPack))
	}

	buf := make([]byte, maxSealedChunk)
	read := uint64(magicSize)
	for i, entry := range r.entries {
		if entry.Offset != read {
			return fail(fmt.Errorf("%w: entry %d starts at %d, but %d bytes have been read", ErrCorrupt, i, entry.Offset, read))
		}
		if entry.Length > uint64(len(buf)) {
			return fail(fmt.Errorf("%w: entry %d is %d bytes, over the %d a sealed chunk can be", ErrCorrupt, i, entry.Length, len(buf)))
		}
		if _, err := io.ReadFull(stream, buf[:entry.Length]); err != nil {
			return fail(fmt.Errorf("read chunk %s: %w", entry.ID, err))
		}
		if _, err := r.decodeChunk(entry, buf[:entry.Length]); err != nil {
			return fail(err)
		}
		read = entry.End()
	}

	// Drain the trailer and tail through the hasher as well.
	copied, err := io.Copy(io.Discard, stream)
	if err != nil {
		return fail(err)
	}
	rest, err := sizeOf(copied)
	if err != nil {
		return fail(err)
	}
	if total := read + rest; total != r.size {
		return fail(fmt.Errorf("%w: read %d bytes of a pack Stat reported as %d", ErrCorrupt, total, r.size))
	}

	var got crypto.ID
	copy(got[:], hasher.Sum(nil))
	if got != r.id {
		return fail(fmt.Errorf("%w: pack hashes to %s: its contents are not what its name says", ErrCorrupt, got))
	}
	return nil
}

// ReadTrailer returns a pack's entries without keeping a reader open. It
// is what rebuild-index and structural checks use.
func ReadTrailer(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID) ([]Entry, error) {
	info, err := b.Stat(ctx, Key(id))
	if err != nil {
		return nil, fmt.Errorf("read trailer of pack %s: %w", id, err)
	}
	size, err := sizeOf(info.Size)
	if err != nil {
		return nil, fmt.Errorf("read trailer of pack %s: %w", id, err)
	}
	return readTrailer(ctx, b, keys, id, size)
}

func readTrailer(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID, size uint64) ([]Entry, error) {
	fail := func(err error) error { return fmt.Errorf("read trailer of pack %s: %w", id, err) }

	if size < tailSize {
		return nil, fail(fmt.Errorf("%w: pack is %d bytes, shorter than its %d-byte tail", ErrNotAPack, size, tailSize))
	}

	window := uint64(tailWindow)
	if window > size {
		window = size
	}
	windowStart, err := offsetOf(size - window)
	if err != nil {
		return nil, fail(err)
	}
	tail, err := readRange(ctx, b, Key(id), windowStart, int64(window)) //nolint:gosec // window <= tailWindow
	if err != nil {
		return nil, fail(err)
	}

	trailerLen, err := parseTail(tail[len(tail)-tailSize:])
	if err != nil {
		return nil, fail(err)
	}
	if trailerLen+tailSize > size {
		return nil, fail(fmt.Errorf("%w: trailer claims %d bytes but the pack is only %d", ErrCorrupt, trailerLen, size))
	}

	var sealed []byte
	if have := uint64(len(tail)) - tailSize; have >= trailerLen {
		sealed = tail[uint64(len(tail))-tailSize-trailerLen : len(tail)-tailSize]
	} else {
		// A trailer larger than the window: one more ranged read, rather
		// than growing the window for every pack.
		trailerStart, err := offsetOf(size - tailSize - trailerLen)
		if err != nil {
			return nil, fail(err)
		}
		//nolint:gosec // parseTail caps trailerLen at maxTrailerSize
		if sealed, err = readRange(ctx, b, Key(id), trailerStart, int64(trailerLen)); err != nil {
			return nil, fail(err)
		}
	}

	encoded, err := crypto.Open(&keys.Index, []byte(crypto.AADPackTrailer), sealed)
	if err != nil {
		return nil, fail(err)
	}

	var t trailer
	if err := crypto.Unmarshal(encoded, &t); err != nil {
		return nil, fail(err)
	}
	if t.Version != Version {
		return nil, fail(fmt.Errorf("%w: trailer declares version %d", ErrUnsupportedVersion, t.Version))
	}
	if len(t.Entries) == 0 {
		return nil, fail(fmt.Errorf("%w: trailer lists no chunks", ErrCorrupt))
	}

	// The trailer is authenticated, but "authentic" is not "consistent":
	// a pack written by a buggy client is still signed by a valid key.
	// v2: chunk data starts after the header magic.
	dataEnd := size - tailSize - trailerLen
	seen := make(map[crypto.ID]struct{}, len(t.Entries))
	next := uint64(magicSize)
	for i, e := range t.Entries {
		switch {
		case e.Offset != next:
			return nil, fail(fmt.Errorf("%w: entry %d starts at %d, expected %d", ErrCorrupt, i, e.Offset, next))
		case e.Length < crypto.Overhead+1:
			return nil, fail(fmt.Errorf("%w: entry %d is %d bytes, shorter than an empty sealed chunk", ErrCorrupt, i, e.Length))
		case e.Length > maxSealedChunk:
			// Refused here rather than at the allocation in readRange: a
			// trailer claiming a gigabyte chunk should fail on open, not
			// by asking for a gigabyte of memory.
			return nil, fail(fmt.Errorf("%w: entry %d is %d bytes, over the %d a sealed chunk can be", ErrCorrupt, i, e.Length, maxSealedChunk))
		case e.End() > dataEnd:
			return nil, fail(fmt.Errorf("%w: entry %d ends at %d, past the %d bytes of chunk data", ErrCorrupt, i, e.End(), dataEnd))
		}
		if _, dup := seen[e.ID]; dup {
			return nil, fail(fmt.Errorf("%w: chunk %s is listed twice", ErrCorrupt, e.ID))
		}
		seen[e.ID] = struct{}{}
		next = e.End()
	}
	if next != dataEnd {
		return nil, fail(fmt.Errorf("%w: entries cover %d bytes but the pack holds %d of chunk data", ErrCorrupt, next, dataEnd))
	}

	return t.Entries, nil
}

func readRange(ctx context.Context, b backend.Backend, key string, off, length int64) ([]byte, error) {
	r, err := b.Get(ctx, key, off, length)
	if err != nil {
		return nil, err
	}
	defer func() { _ = r.Close() }()

	buf := make([]byte, length)
	if _, err := io.ReadFull(r, buf); err != nil {
		return nil, fmt.Errorf("read %d bytes at offset %d of %s: %w", length, off, key, err)
	}
	return buf, nil
}
