package pack

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/parity"
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
	target      uint64
	nonceSource io.Reader

	spool   *os.File
	hasher  io.Writer
	digest  interface{ Sum(b []byte) []byte }
	entries []Entry
	seen    map[crypto.ID]struct{}
	size    uint64

	finished bool

	// parity is how many Reed-Solomon parity shards to write beside the
	// pack, 0 for none; parityWarn hears about a parity that could not
	// be written, which is not a failed backup.
	parity     int
	parityWarn func(format string, args ...any)
}

// SetParity asks Finish to write a parity object with m shards beside
// the pack. A parity that fails to write is reported to warn and does
// not fail the pack: the data is safe, the redundancy is what is missing.
func (w *Writer) SetParity(m int, warn func(format string, args ...any)) {
	w.parity = m
	w.parityWarn = warn
}

// NewWriter starts a pack with the default target size; see
// NewWriterParams.
func NewWriter(keys *crypto.Keys, dir string, nonceSource io.Reader) (*Writer, error) {
	return NewWriterParams(keys, dir, TargetSize, nonceSource)
}

// NewWriterParams starts a pack, spooling to a temporary file in dir and
// flushing when it reaches target bytes. Passing an empty dir uses the
// system temporary directory. The target comes from the repository's
// config, not a constant: two clients of one repository need not agree
// on batching, but each should honour what its user configured.
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
func NewWriterParams(keys *crypto.Keys, dir string, target uint64, nonceSource io.Reader) (*Writer, error) {
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
	w := &Writer{
		keys:        keys,
		target:      target,
		nonceSource: nonces,
		spool:       spool,
		hasher:      io.MultiWriter(spool, hasher),
		digest:      hasher,
		seen:        make(map[crypto.ID]struct{}),
	}
	// v2: the pack opens with the magic (trailer offsets are absolute
	// file positions, so this must be written before the first chunk).
	if _, err := w.hasher.Write(magic[:]); err != nil {
		return nil, fmt.Errorf("create pack writer: write header: %w", err)
	}
	w.size = uint64(len(magic))
	return w, nil
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

	if _, err := w.hasher.Write(sealed); err != nil {
		return fmt.Errorf("add chunk %s: write spool file: %w", id, err)
	}

	w.seen[id] = struct{}{}
	w.entries = append(w.entries, Entry{ID: id, Offset: w.size, Length: uint64(len(sealed)), RawLen: uint64(len(plaintext))})
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
func (w *Writer) Full() bool { return w.size >= w.target }

// Finish writes the trailer, uploads the pack and returns its ID, the
// entries it contains, and its total size (what an index blob records so
// `check` can catch a truncated pack with a HEAD).
//
// The upload is PutIfAbsent, so an identical pack built by another client
// is not an error: the bytes are already there and the entries returned
// still describe them. Finish removes the spool file either way.
func (w *Writer) Finish(ctx context.Context, b backend.Backend) (crypto.ID, []Entry, uint64, error) {
	if w.finished {
		return crypto.ID{}, nil, 0, errors.New("finish pack: writer is already finished")
	}
	if len(w.entries) == 0 {
		return crypto.ID{}, nil, 0, errors.New("finish pack: no chunks were added")
	}
	defer w.cleanup()
	w.finished = true

	encoded, err := crypto.Marshal(trailer{Version: Version, Entries: w.entries})
	if err != nil {
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack: encode trailer: %w", err)
	}
	sealed, err := crypto.Seal(&w.keys.Index, []byte(crypto.AADPackTrailer), encoded, w.nonceSource)
	if err != nil {
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack: seal trailer: %w", err)
	}
	if _, err := w.hasher.Write(sealed); err != nil {
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack: write trailer: %w", err)
	}
	if _, err := w.hasher.Write(encodeTail(uint64(len(sealed)))); err != nil {
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack: write tail: %w", err)
	}

	totalInt, err := offsetOf(w.size + uint64(len(sealed)) + tailSize)
	if err != nil {
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack: %w", err)
	}
	total := uint64(totalInt) //nolint:gosec // offsetOf bounds it to [0, MaxInt64]

	var id crypto.ID
	copy(id[:], w.digest.Sum(nil))

	if err := w.spool.Sync(); err != nil {
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack %s: sync spool file: %w", id, err)
	}
	if _, err := w.spool.Seek(0, io.SeekStart); err != nil {
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack %s: rewind spool file: %w", id, err)
	}

	switch err := b.PutIfAbsent(ctx, Key(id), w.spool, int64(total)); { //nolint:gosec // bounds-checked above
	case err == nil, errors.Is(err, backend.ErrExists):
		// An identical pack already stored is a successful deduplication:
		// the name is the hash of the bytes, so what is there is what we
		// were about to write.
	default:
		return crypto.ID{}, nil, 0, fmt.Errorf("finish pack %s: %w", id, err)
	}

	if w.parity > 0 {
		if err := w.writeParity(ctx, b, id); err != nil && w.parityWarn != nil {
			w.parityWarn("pack %s is stored but its parity is not: %v", id, err)
		}
	}
	return id, w.entries, total, nil
}

// writeParity computes the parity object from the spool file, which is
// still on disk, and stores it beside the pack.
func (w *Writer) writeParity(ctx context.Context, b backend.Backend, id crypto.ID) error {
	if _, err := w.spool.Seek(0, io.SeekStart); err != nil {
		return fmt.Errorf("rewind spool file: %w", err)
	}
	data, err := io.ReadAll(w.spool)
	if err != nil {
		return fmt.Errorf("read spool file: %w", err)
	}
	encoded, err := parity.Encode(id, data, w.parity)
	if err != nil {
		return err
	}
	if err := backend.PutBytesIfAbsent(ctx, b, parity.Key(id), encoded); err != nil && !errors.Is(err, backend.ErrExists) {
		return err
	}
	return nil
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
