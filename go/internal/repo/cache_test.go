package repo

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/index"
)

// countingBackend records how many index blobs were fetched from the
// real backend, which is the number the cache exists to drive to zero.
type countingBackend struct {
	backend.Backend
	indexGets atomic.Int64
}

func (c *countingBackend) Get(ctx context.Context, key string, off, length int64) (io.ReadCloser, error) {
	if strings.HasPrefix(key, index.Prefix) {
		c.indexGets.Add(1)
	}
	return c.Backend.Get(ctx, key, off, length)
}

func openCounting(t *testing.T, dir, cacheDir string, noCache bool) (*Repository, *countingBackend) {
	t.Helper()

	inner, err := backend.OpenLocal(dir)
	if err != nil {
		t.Fatalf("open backend: %v", err)
	}
	counting := &countingBackend{Backend: inner}

	opts := testOptions(t, "cache")
	opts.CacheDir = cacheDir
	opts.NoCache = noCache
	r, err := Open(context.Background(), counting, opts)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() {
		if err := r.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return r, counting
}

func TestIndexCacheAvoidsRefetching(t *testing.T) {
	_, dir, _ := backedUpRepo(t, "cache")
	cacheDir := t.TempDir()

	first, counting := openCounting(t, dir, cacheDir, false)
	if counting.indexGets.Load() != 1 {
		t.Fatalf("first open fetched %d index blobs, want 1", counting.indexGets.Load())
	}
	want := first.Index().Len()

	second, counting := openCounting(t, dir, cacheDir, false)
	if counting.indexGets.Load() != 0 {
		t.Errorf("second open fetched %d index blobs, want 0: the cache was not used", counting.indexGets.Load())
	}
	if second.Index().Len() != want {
		t.Errorf("index from cache holds %d chunks, want %d", second.Index().Len(), want)
	}
}

// A corrupted cache entry must be a cache miss, never a poisoned index.
func TestIndexCacheRejectsACorruptedEntry(t *testing.T) {
	r, dir, _ := backedUpRepo(t, "cache-corrupt")
	cacheDir := t.TempDir()
	want := r.Index().Len()

	openCounting(t, dir, cacheDir, false) // fill

	entries := cacheEntries(t, cacheDir)
	if len(entries) != 1 {
		t.Fatalf("cache holds %d entries, want 1", len(entries))
	}
	if err := os.WriteFile(entries[0], []byte("not the blob"), 0o600); err != nil {
		t.Fatalf("corrupt cache: %v", err)
	}

	repaired, counting := openCounting(t, dir, cacheDir, false)
	if counting.indexGets.Load() != 1 {
		t.Errorf("open with a corrupted cache fetched %d blobs, want 1", counting.indexGets.Load())
	}
	if repaired.Index().Len() != want {
		t.Errorf("index holds %d chunks after a corrupted cache, want %d", repaired.Index().Len(), want)
	}

	// And the fetch refilled the cache with the right bytes.
	_, counting = openCounting(t, dir, cacheDir, false)
	if counting.indexGets.Load() != 0 {
		t.Errorf("open after the refill fetched %d blobs, want 0", counting.indexGets.Load())
	}
}

func TestIndexCacheCanBeDisabled(t *testing.T) {
	_, dir, _ := backedUpRepo(t, "cache-off")
	cacheDir := t.TempDir()

	for i := range 2 {
		_, counting := openCounting(t, dir, cacheDir, true)
		if counting.indexGets.Load() != 1 {
			t.Errorf("open %d with NoCache fetched %d blobs, want 1", i, counting.indexGets.Load())
		}
	}
	if entries := cacheEntries(t, cacheDir); len(entries) != 0 {
		t.Errorf("NoCache still wrote %d cache entries", len(entries))
	}
}

// After rebuild-index the old blobs are gone from the repository; the
// cache must not keep them around forever.
func TestIndexCachePrunesUnlistedBlobs(t *testing.T) {
	r, dir, _ := backedUpRepo(t, "cache-prune")
	cacheDir := t.TempDir()
	ctx := context.Background()

	openCounting(t, dir, cacheDir, false)
	before := cacheEntries(t, cacheDir)
	if len(before) != 1 {
		t.Fatalf("cache holds %d entries, want 1", len(before))
	}

	if _, err := r.RebuildIndex(ctx); err != nil {
		t.Fatalf("rebuild: %v", err)
	}
	openCounting(t, dir, cacheDir, false)

	after := cacheEntries(t, cacheDir)
	if len(after) != 1 {
		t.Fatalf("cache holds %d entries after a rebuild, want 1", len(after))
	}
	if after[0] == before[0] {
		t.Errorf("the pre-rebuild blob %s is still cached", filepath.Base(before[0]))
	}
}

func cacheEntries(t *testing.T, cacheDir string) []string {
	t.Helper()
	entries, err := filepath.Glob(filepath.Join(cacheDir, "*", "indexes", "*"))
	if err != nil {
		t.Fatalf("glob cache: %v", err)
	}
	return entries
}
