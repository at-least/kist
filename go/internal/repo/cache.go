package repo

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
)

// indexCache keeps local copies of index blobs, so that opening a
// repository on a slow backend does not re-download an index that has not
// changed since yesterday.
//
// It decorates a backend and intercepts exactly one shape of request: a
// whole-object Get of indexes/<id>. Everything else passes straight
// through. Blobs are immutable and named by their own hash, so the cache
// has no invalidation problem beyond "delete what the repository no
// longer lists" -- and even that is housekeeping, not correctness, since
// a stale entry is never read unless it is listed.
//
// A cached file is re-hashed before use. A corrupted cache must fall
// through to a fetch, never feed the index.
type indexCache struct {
	backend.Backend
	dir string
}

// DefaultCacheDir is where per-repository caches live when Options does
// not say otherwise.
func DefaultCacheDir() (string, error) {
	dir, err := os.UserCacheDir()
	if err != nil {
		return "", fmt.Errorf("locate the user cache directory: %w", err)
	}
	return filepath.Join(dir, "kist"), nil
}

func newIndexCache(inner backend.Backend, root string, repoID crypto.RepoID) (*indexCache, error) {
	dir := filepath.Join(root, hexRepoID(repoID), "indexes")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, fmt.Errorf("create index cache %s: %w", dir, err)
	}
	return &indexCache{Backend: inner, dir: dir}, nil
}

// Get serves a whole index blob from the cache when it can, and fills
// the cache when it cannot.
func (c *indexCache) Get(ctx context.Context, key string, off, length int64) (io.ReadCloser, error) {
	id, ok := c.blobID(key)
	if !ok || off != 0 || length != backend.ReadToEnd {
		return c.Backend.Get(ctx, key, off, length)
	}

	path := filepath.Join(c.dir, id.String())
	if data, err := os.ReadFile(path); err == nil { //nolint:gosec // path is the cache dir plus a hex ID
		if crypto.CiphertextID(data) == id {
			return io.NopCloser(strings.NewReader(string(data))), nil
		}
		// Damaged on disk. Drop it and fetch; the fetch is verified by
		// index.Load in the usual way.
		_ = os.Remove(path)
	}

	data, err := backend.GetAll(ctx, c.Backend, key)
	if err != nil {
		return nil, err
	}
	if crypto.CiphertextID(data) == id {
		// Best effort: a cache that cannot be written is a slow open,
		// not a failure, and there is no channel to report it on that
		// would not be noise on every open.
		_ = writeFileAtomic(path, data) //nolint:errcheck // best-effort cache fill, documented above
	}
	return io.NopCloser(strings.NewReader(string(data))), nil
}

// prune removes cached blobs the repository no longer lists.
func (c *indexCache) prune(listed []crypto.ID) {
	keep := make(map[string]struct{}, len(listed))
	for _, id := range listed {
		keep[id.String()] = struct{}{}
	}
	entries, err := os.ReadDir(c.dir)
	if err != nil {
		return
	}
	for _, e := range entries {
		if _, ok := keep[e.Name()]; !ok {
			// e.Name() is a single component from ReadDir of the cache
			// directory itself; it cannot name anything outside it.
			_ = os.Remove(filepath.Join(c.dir, e.Name())) //nolint:gosec,errcheck // bounded to the cache dir; housekeeping
		}
	}
}

func (c *indexCache) blobID(key string) (crypto.ID, bool) {
	rest, ok := strings.CutPrefix(key, index.Prefix)
	if !ok {
		return crypto.ID{}, false
	}
	id, err := crypto.ParseID(rest)
	return id, err == nil
}

// writeFileAtomic writes via a temporary name so a crash never leaves a
// half-written cache entry under a valid name. The verification on read
// would catch it anyway; this keeps the cache from filling with junk.
func writeFileAtomic(path string, data []byte) error {
	tmp, err := os.CreateTemp(filepath.Dir(path), ".tmp-*")
	if err != nil {
		return err
	}
	name := tmp.Name()
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		_ = os.Remove(name)
		return err
	}
	if err := tmp.Close(); err != nil {
		_ = os.Remove(name)
		return err
	}
	if err := os.Rename(name, path); err != nil {
		_ = os.Remove(name)
		return err
	}
	return nil
}
