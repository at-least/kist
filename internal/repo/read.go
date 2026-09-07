package repo

import (
	"context"
	"fmt"
	"sync"

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
func (r *Repository) LoadTree(ctx context.Context, id crypto.ID) (*tree.Tree, error) {
	return tree.Load(ctx, r.backend, r.keys, id)
}

// LoadTreeChain reads every segment of a (possibly split) directory and
// returns the entries in on-disk order: the last segment's ID is what a
// parent records.
func (r *Repository) LoadTreeChain(ctx context.Context, last crypto.ID) ([]tree.Entry, error) {
	return tree.LoadChain(ctx, r.backend, r.keys, last)
}

// Refresh reloads the index from the stored blobs, so that a long-lived
// process sees packs written since it opened.
func (r *Repository) Refresh(ctx context.Context, warn func(format string, args ...any)) error {
	if warn == nil {
		warn = func(string, ...any) {}
	}
	return r.refreshIndex(ctx, warn)
}
