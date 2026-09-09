package repo

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"sync"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/tree"
)

// A ChunkSource reads chunks by ID, keeping one open pack reader per
// pack so that a directory whose files interleave across packs does not
// re-fetch a trailer for every chunk. It is the one place plaintext
// comes out of a pack; restore and mount both read through it.
//
// Safe for concurrent use.
type ChunkSource struct {
	repo *Repository

	mu      sync.Mutex
	readers map[crypto.ID]*pack.Reader
}

// NewChunkSource returns a source over this repository's index.
func (r *Repository) NewChunkSource() *ChunkSource {
	return &ChunkSource{repo: r, readers: make(map[crypto.ID]*pack.Reader)}
}

// Chunk returns the plaintext of one chunk.
func (c *ChunkSource) Chunk(ctx context.Context, id crypto.ID) ([]byte, error) {
	loc, ok := c.repo.index.Lookup(id)
	if !ok {
		return nil, fmt.Errorf("chunk %s: %w", id, index.ErrNotFound)
	}

	reader, err := c.reader(ctx, loc.Pack)
	if err != nil {
		return nil, err
	}
	entry, ok := reader.Lookup(id)
	if !ok {
		return nil, fmt.Errorf("chunk %s: the index says pack %s, whose trailer does not list it", id, loc.Pack)
	}
	return reader.Chunk(ctx, entry)
}

// ChunkList reassembles and decodes an indirect chunk list.
func (c *ChunkSource) ChunkList(ctx context.Context, chunks []crypto.ID) ([]crypto.ID, error) {
	var buf []byte
	for _, id := range chunks {
		data, err := c.Chunk(ctx, id)
		if err != nil {
			return nil, err
		}
		buf = append(buf, data...)
	}
	var list tree.ChunkList
	if err := crypto.Unmarshal(buf, &list); err != nil {
		return nil, fmt.Errorf("%w: chunk list: %w", tree.ErrCorrupt, err)
	}
	if list.Version != tree.Version {
		return nil, fmt.Errorf("%w: chunk list declares version %d, this build reads %d", tree.ErrCorrupt, list.Version, tree.Version)
	}
	return list.Chunks, nil
}

func (c *ChunkSource) reader(ctx context.Context, packID crypto.ID) (*pack.Reader, error) {
	c.mu.Lock()
	reader, ok := c.readers[packID]
	c.mu.Unlock()
	if ok {
		return reader, nil
	}
	// Opened outside the lock: a trailer read is a network round trip,
	// and two files in two packs should not wait on each other. Two
	// concurrent opens of the same pack are wasteful, not wrong.
	reader, err := pack.OpenReader(ctx, c.repo.backend, c.repo.keys, packID)
	if err != nil {
		return nil, err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if existing, ok := c.readers[packID]; ok {
		return existing, nil
	}
	c.readers[packID] = reader
	return reader, nil
}

// LoadTree reads one directory object.
//
// A tree whose primary copy will not read -- missing, or failing its
// verification -- is read from its .r1 replica when the repository keeps
// one: the replica is the same object stored twice, and a copy that can
// serve is worth more than an error that could have been avoided.
func (r *Repository) LoadTree(ctx context.Context, id crypto.ID) (*tree.Tree, error) {
	return r.readTree(ctx, id)
}

// readTree reads and fully verifies a tree, falling back to the .r1
// replica when the primary is missing or corrupt (format-v3-draft.md
// §13.5).
func (r *Repository) readTree(ctx context.Context, id crypto.ID) (*tree.Tree, error) {
	t, err := tree.Load(ctx, r.backend, r.keys, id)
	if err == nil {
		return t, nil
	}
	if !isTreeReadFailure(err) {
		return nil, err
	}
	replica, rerr := tree.LoadAt(ctx, r.backend, r.keys, tree.ReplicaKey(id), id)
	if rerr != nil {
		return nil, err // the primary's failure is the one to report
	}
	r.warnf("tree %s read from its .r1 replica (primary missing or corrupt)", id)
	return replica, nil
}

// isTreeReadFailure reports whether a failed tree read is the kind a
// replica can answer: the object is not there, or the bytes that are
// there do not verify.
func isTreeReadFailure(err error) bool {
	return errors.Is(err, backend.ErrNotFound) ||
		errors.Is(err, tree.ErrCorrupt) ||
		errors.Is(err, crypto.ErrDecrypt)
}

func (r *Repository) warnf(format string, args ...any) {
	if r.warn != nil {
		r.warn(format, args...)
	}
}

// LoadTreeChain reads every segment of a (possibly split) directory and
// returns the entries in on-disk order: the last segment's ID is what a
// parent records.
func (r *Repository) LoadTreeChain(ctx context.Context, last crypto.ID) ([]tree.Entry, error) {
	var parts [][]tree.Entry
	next := &last
	seen := make(map[crypto.ID]struct{})
	for next != nil {
		if _, dup := seen[*next]; dup {
			return nil, fmt.Errorf("load tree chain: %w: loop at %s", tree.ErrCorrupt, next)
		}
		seen[*next] = struct{}{}
		t, err := r.readTree(ctx, *next)
		if err != nil {
			return nil, err
		}
		parts = append(parts, t.Entries)
		next = t.Prev
	}
	slices.Reverse(parts)
	var all []tree.Entry
	for _, p := range parts {
		all = append(all, p...)
	}
	return all, nil
}

// touchTree refreshes a tree's revival signal. The Put overwrites: the
// backend mtime moving forward is the entire signal, and only an
// overwriting write moves it (format-v3-draft.md §13.1).
func (r *Repository) touchTree(ctx context.Context, id crypto.ID) error {
	if err := backend.PutBytes(ctx, r.backend, tree.TouchKey(id), tree.TouchMagic); err != nil {
		return fmt.Errorf("touch tree %s: %w", id, err)
	}
	return nil
}

// headTreeAndTouch is the backup commit gate for marked trees: the tree
// must still exist, and its touch signal must be no older than the mark
// (same second counts as newer -- the safe side, matching prune). Either
// failing means prune may have deleted the tree in the window between
// its own touch check and the delete; committing would point a snapshot
// at data that may be gone. The tree's own mtime does not participate:
// v3 trees are write-once, and the touch is the only revival signal.
func (r *Repository) headTreeAndTouch(ctx context.Context, id crypto.ID, markedAt time.Time) error {
	if _, err := r.backend.Stat(ctx, tree.Key(id)); err != nil {
		return fmt.Errorf("%w: tree %s: %w", ErrTreeMarked, id, err)
	}
	touch, err := r.backend.Stat(ctx, tree.TouchKey(id))
	if err != nil {
		return fmt.Errorf("%w: tree %s: %w", ErrTreeMarked, id, err)
	}
	if touch.Modified.Truncate(time.Second).Before(markedAt) {
		return fmt.Errorf("%w: tree %s has not been touched since it was marked", ErrTreeMarked, id)
	}
	return nil
}

// Refresh reloads the index from the stored blobs, so that a long-lived
// process sees packs written since it opened.
func (r *Repository) Refresh(ctx context.Context, warn func(format string, args ...any)) error {
	if warn == nil {
		warn = func(string, ...any) {}
	}
	return r.refreshIndex(ctx, warn)
}
