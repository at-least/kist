package pack

import (
	"context"
	"errors"
	"fmt"
	"io"
	"math"
	"os"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

// A Writer assembles one pack.
//
// Chunks are sealed as they arrive and appended to a spool file on local
// disk, with the pack's BLAKE3 running alongside. Spooling rather than
// buffering is forced by the naming rule: a pack is named by the hash of
// its finished bytes, so the name does not exist until the last byte is
// written, and holding 64 MiB per concurrent writer in memory is not a
// trade worth making.
//
// A Writer is not safe for concurrent use. A backup runs several, one per
// goroutine, and each produces an independent pack.
type Writer struct {
	keys        *crypto.Keys
	nonceSource io.Reader

	spool   *os.File
	hasher  io.Writer
	digest  interface{ Sum(b []byte) []byte }
	entries []Entry
	seen    map[crypto.ID]struct{}
	size    uint64

	finished bool
}

// NewWriter starts a pack, spooling to a temporary file in dir. Passing
// an empty dir uses the system temporary directory.
//
// nonceSource seeds the writer's nonces; production callers pass nil,
// meaning crypto/rand. The seed is expanded through crypto.NonceStream,
// so every chunk in a pack gets a distinct nonce even if the caller hands
// over a source that does not advance -- nonce reuse under one key is the
// one mistake this package cannot survive, so it is made unreachable
// rather than documented against.
//
// Two writers seeded identically do produce identical nonces. That is
// what makes golden files possible, and it is harmless: identical seed,
// key and chunks give a byte-identical pack, which is the deduplication
// case, not a reuse.
//
// Callers must call Finish or Abort.
func NewWriter(keys *crypto.Keys, dir string, nonceSource io.Reader) (*Writer, error) {
	nonces, err := crypto.NonceStream(nonceSource)
	if err != nil {
		return nil, fmt.Errorf("create pack writer: %w", err)
	}

	spool, err := os.CreateTemp(dir, "kist-pack-*.tmp")
	if err != nil {
		return nil, fmt.Errorf("create pack spool file: %w", err)
	}
	// The spool file is ours alone and is removed by Finish or Abort;
	// unlinking it now would break the reopen-for-upload step.

	hasher := crypto.CiphertextHasher()
	return &Writer{
		keys:        keys,
		nonceSource: nonces,
		spool:       spool,
		hasher:      io.MultiWriter(spool, hasher),
		digest:      hasher,
		seen:        make(map[crypto.ID]struct{}),
	}, nil
}

// Add seals plaintext under the chunk key and appends it.
//
// id must be the chunk's content address; it is used as the AAD, so a
// sealed chunk cannot be moved to another chunk's slot even by someone
// holding the key.
//
// Adding a chunk this pack already holds returns ErrDuplicateChunk. It is
// a caller error, not a condition to route around: a trailer that lists
// one chunk twice fails its own consistency check, so silently accepting
// it would produce an unreadable pack. Callers deduplicate before they
// get here.
func (w *Writer) Add(id crypto.ID, plaintext []byte) error {
	if w.finished {
		return errors.New("add to pack: writer is already finished")
	}
	if _, dup := w.seen[id]; dup {
		return fmt.Errorf("add chunk %s: %w", id, ErrDuplicateChunk)
	}

	algorithm, payload, err := compress(plaintext)
	if err != nil {
		return fmt.Errorf("add chunk %s: %w", id, err)
	}

	framed := make([]byte, 0, 1+len(payload))
	framed = append(framed, algorithm)
	framed = append(framed, payload...)

	sealed, err := crypto.Seal(&w.keys.Chunk, id[:], framed, w.nonceSource)
	if err != nil {
		return fmt.Errorf("add chunk %s: %w", id, err)
	}

	// A chunk is bounded by the chunker's maximum, so this cannot happen
	// for a chunk that came from the chunker; it is checked because the
	// length field is uint32 and a silent truncation here would produce a
	// pack whose trailer disagrees with its own bytes.
	if len(sealed) > math.MaxUint32 {
		return fmt.Errorf("add chunk %s: sealed chunk is %d bytes, over the %d format limit", id, len(sealed), uint64(math.MaxUint32))
	}

	if _, err := w.hasher.Write(sealed); err != nil {
		return fmt.Errorf("add chunk %s: write spool file: %w", id, err)
	}

	w.seen[id] = struct{}{}
	w.entries = append(w.entries, Entry{ID: id, Offset: w.size, Length: uint32(len(sealed))}) //nolint:gosec // bounds-checked against MaxUint32 just above
	w.size += uint64(len(sealed))
	return nil
}

// Size is the number of bytes written so far, excluding the trailer.
func (w *Writer) Size() uint64 { return w.size }

// Count is the number of chunks added so far.
func (w *Writer) Count() int { return len(w.entries) }

// Full reports whether the pack has reached its target size and should be
// flushed. A writer never refuses a chunk for being over the target: a
// chunk is indivisible, so the target is a threshold, not a limit.
func (w *Writer) Full() bool { return w.size >= TargetSize }

// Finish writes the trailer, uploads the pack and returns its ID and the
// entries it contains.
//
// The upload is PutIfAbsent, so an identical pack built by another client
// is not an error: the bytes are already there and the entries returned
// still describe them. Finish removes the spool file either way.
func (w *Writer) Finish(ctx context.Context, b backend.Backend) (crypto.ID, []Entry, error) {
	if w.finished {
		return crypto.ID{}, nil, errors.New("finish pack: writer is already finished")
	}
	if len(w.entries) == 0 {
		return crypto.ID{}, nil, errors.New("finish pack: no chunks were added")
	}
	defer w.cleanup()
	w.finished = true

	encoded, err := crypto.Marshal(trailer{Version: Version, Entries: w.entries})
	if err != nil {
		return crypto.ID{}, nil, fmt.Errorf("finish pack: encode trailer: %w", err)
	}
	sealed, err := crypto.Seal(&w.keys.Index, []byte(crypto.AADPackTrailer), encoded, w.nonceSource)
	if err != nil {
		return crypto.ID{}, nil, fmt.Errorf("finish pack: seal trailer: %w", err)
	}
	if _, err := w.hasher.Write(sealed); err != nil {
		return crypto.ID{}, nil, fmt.Errorf("finish pack: write trailer: %w", err)
	}
	if _, err := w.hasher.Write(encodeTail(uint64(len(sealed)))); err != nil {
		return crypto.ID{}, nil, fmt.Errorf("finish pack: write tail: %w", err)
	}

	total, err := offsetOf(w.size + uint64(len(sealed)) + tailSize)
	if err != nil {
		return crypto.ID{}, nil, fmt.Errorf("finish pack: %w", err)
	}

	var id crypto.ID
	copy(id[:], w.digest.Sum(nil))

	if err := w.spool.Sync(); err != nil {
		return crypto.ID{}, nil, fmt.Errorf("finish pack %s: sync spool file: %w", id, err)
	}
	if _, err := w.spool.Seek(0, io.SeekStart); err != nil {
		return crypto.ID{}, nil, fmt.Errorf("finish pack %s: rewind spool file: %w", id, err)
	}

	switch err := b.PutIfAbsent(ctx, Key(id), w.spool, total); {
	case err == nil, errors.Is(err, backend.ErrExists):
		// An identical pack already stored is a successful deduplication:
		// the name is the hash of the bytes, so what is there is what we
		// were about to write.
	default:
		return crypto.ID{}, nil, fmt.Errorf("finish pack %s: %w", id, err)
	}

	return id, w.entries, nil
}

// Abort discards the pack and its spool file. It is safe to call after
// Finish, which makes it usable as a deferred cleanup.
func (w *Writer) Abort() {
	if w.finished {
		return
	}
	w.finished = true
	w.cleanup()
}

func (w *Writer) cleanup() {
	name := w.spool.Name()
	_ = w.spool.Close()
	_ = os.Remove(name)
}
