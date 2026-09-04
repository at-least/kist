package index

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"slices"
	"sync"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
)

// Version is the index blob schema version.
const Version = 1

// ErrNotFound means a chunk is not in the index. It does not mean the
// chunk is absent from the repository: an index is a cache, and the
// authority is the set of pack trailers.
var ErrNotFound = errors.New("chunk not in index")

// ErrCorrupt means an index blob is structurally invalid.
var ErrCorrupt = errors.New("index blob is corrupt")

// A Location says where one chunk's bytes are.
//
// Offset and Length are exactly pack.Entry's, so an index entry and the
// trailer it came from can never disagree about what they mean.
type Location struct {
	Pack   crypto.ID
	Offset uint64
	Length uint32
}

// An Index answers "do I already have this chunk, and where is it".
//
// It is safe for concurrent use: a backup looks chunks up from every
// worker goroutine and adds to it as packs are finished.
type Index struct {
	mu     sync.RWMutex
	byID   map[crypto.ID]Location
	sealed map[crypto.ID]struct{} // packs already represented here
}

// New returns an empty index.
func New() *Index {
	return &Index{
		byID:   make(map[crypto.ID]Location),
		sealed: make(map[crypto.ID]struct{}),
	}
}

// Lookup finds a chunk.
func (ix *Index) Lookup(id crypto.ID) (Location, bool) {
	ix.mu.RLock()
	defer ix.mu.RUnlock()

	loc, ok := ix.byID[id]
	return loc, ok
}

// Has reports whether the index already accounts for a chunk. It is the
// question a backup asks for every chunk it produces.
func (ix *Index) Has(id crypto.ID) bool {
	ix.mu.RLock()
	defer ix.mu.RUnlock()

	_, ok := ix.byID[id]
	return ok
}

// Len is the number of distinct chunks the index knows about.
func (ix *Index) Len() int {
	ix.mu.RLock()
	defer ix.mu.RUnlock()

	return len(ix.byID)
}

// Packs returns the IDs of every pack the index refers to.
func (ix *Index) Packs() []crypto.ID {
	ix.mu.RLock()
	defer ix.mu.RUnlock()

	out := make([]crypto.ID, 0, len(ix.sealed))
	for id := range ix.sealed {
		out = append(out, id)
	}
	return out
}

// AddPack records every chunk in one pack.
//
// A chunk already known keeps its existing location. Two packs holding
// the same chunk is normal -- two clients can pack the same content at
// the same moment -- and either copy is as good as the other, so the
// first one wins and the duplicate simply becomes unreferenced.
func (ix *Index) AddPack(packID crypto.ID, entries []pack.Entry) {
	ix.mu.Lock()
	defer ix.mu.Unlock()

	ix.sealed[packID] = struct{}{}
	for _, e := range entries {
		if _, ok := ix.byID[e.ID]; ok {
			continue
		}
		ix.byID[e.ID] = Location{Pack: packID, Offset: e.Offset, Length: e.Length}
	}
}

// blob is the on-disk form of an index: packs, each with its entries.
// Grouping by pack rather than listing flat triples keeps the pack ID out
// of every entry, which is most of the blob's size.
type blob struct {
	Version uint64     `cbor:"v"`
	Packs   []blobPack `cbor:"packs"`
}

type blobPack struct {
	_ struct{} `cbor:",toarray"`

	ID      crypto.ID
	Entries []pack.Entry
}

// Key returns the repository key an index blob is stored under.
func Key(id crypto.ID) string { return "indexes/" + id.String() }

// Prefix is the repository prefix index blobs live under.
const Prefix = "indexes/"

// Save writes the given packs as one index blob and returns its ID.
//
// It is called once at the end of a backup, after the last pack is
// uploaded and before the snapshot is committed. A crash between the
// packs and this call leaves orphaned packs, not a broken repository.
func Save(ctx context.Context, b backend.Backend, keys *crypto.Keys, packs map[crypto.ID][]pack.Entry, nonceSource io.Reader) (crypto.ID, error) {
	if len(packs) == 0 {
		return crypto.ID{}, errors.New("save index: no packs to record")
	}

	doc := blob{Version: Version, Packs: make([]blobPack, 0, len(packs))}
	for id, entries := range packs {
		doc.Packs = append(doc.Packs, blobPack{ID: id, Entries: entries})
	}
	// Map iteration order is randomised, so without this the same set of
	// packs would encode differently on every run and two clients writing
	// the same index would collide instead of deduplicating.
	slices.SortFunc(doc.Packs, func(a, b blobPack) int {
		return bytes.Compare(a.ID[:], b.ID[:])
	})

	encoded, err := crypto.Marshal(doc)
	if err != nil {
		return crypto.ID{}, fmt.Errorf("save index: %w", err)
	}
	sealed, err := crypto.Seal(&keys.Index, []byte(crypto.AADIndexBlob), encoded, nonceSource)
	if err != nil {
		return crypto.ID{}, fmt.Errorf("save index: %w", err)
	}

	id := crypto.CiphertextID(sealed)
	switch err := backend.PutBytesIfAbsent(ctx, b, Key(id), sealed); {
	case err == nil, errors.Is(err, backend.ErrExists):
		return id, nil
	default:
		return crypto.ID{}, fmt.Errorf("save index %s: %w", id, err)
	}
}

// Load reads one index blob into ix.
func Load(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID, ix *Index) error {
	sealed, err := backend.GetAll(ctx, b, Key(id))
	if err != nil {
		return fmt.Errorf("load index %s: %w", id, err)
	}
	if got := crypto.CiphertextID(sealed); got != id {
		return fmt.Errorf("load index %s: %w: blob hashes to %s", id, ErrCorrupt, got)
	}

	encoded, err := crypto.Open(&keys.Index, []byte(crypto.AADIndexBlob), sealed)
	if err != nil {
		return fmt.Errorf("load index %s: %w", id, err)
	}

	var doc blob
	if err := crypto.Unmarshal(encoded, &doc); err != nil {
		return fmt.Errorf("load index %s: %w", id, err)
	}
	if doc.Version != Version {
		return fmt.Errorf("load index %s: %w: blob declares version %d, this build reads %d", id, ErrCorrupt, doc.Version, Version)
	}

	for _, p := range doc.Packs {
		if len(p.Entries) == 0 {
			return fmt.Errorf("load index %s: %w: pack %s has no entries", id, ErrCorrupt, p.ID)
		}
		ix.AddPack(p.ID, p.Entries)
	}
	return nil
}

// List returns the ID of every index blob in the repository, and the keys
// of any object under the prefix that is not named like one.
func List(ctx context.Context, b backend.Backend) ([]crypto.ID, []string, error) {
	var (
		ids      []crypto.ID
		unusable []string
	)
	err := b.List(ctx, Prefix, func(fi backend.FileInfo) error {
		// An object under this prefix that is not named like a blob is
		// reported, not returned as an error: List's job is to say what
		// is there, and the caller decides what an unusable entry means.
		id, parseErr := crypto.ParseID(fi.Key[len(Prefix):])
		if parseErr != nil {
			unusable = append(unusable, fi.Key)
			return nil //nolint:nilerr // a misnamed object is a finding for the caller, not a listing failure
		}
		ids = append(ids, id)
		return nil
	})
	if err != nil {
		return nil, nil, fmt.Errorf("list index blobs: %w", err)
	}
	return ids, unusable, nil
}

// LoadAll merges every index blob in the repository.
//
// A blob that cannot be read is skipped and returned in the second
// result, not treated as a failure. The index is a cache: a damaged blob
// means "this repository needs rebuild-index", and making it fatal would
// be a catch-22, because opening the repository is how you get to run
// that command. Only a failure to list is fatal, because then nothing is
// known about what is there.
func LoadAll(ctx context.Context, b backend.Backend, keys *crypto.Keys) (*Index, []error, error) {
	ids, unusable, err := List(ctx, b)
	if err != nil {
		return nil, nil, err
	}

	ix := New()
	var skipped []error
	for _, key := range unusable {
		skipped = append(skipped, fmt.Errorf("%w: %q is not named like an index blob", ErrCorrupt, key))
	}
	for _, id := range ids {
		if err := Load(ctx, b, keys, id, ix); err != nil {
			skipped = append(skipped, err)
		}
	}
	return ix, skipped, nil
}

// Rebuild reconstructs an index by reading every pack trailer, ignoring
// the index blobs entirely. It is what proves an index is only a cache.
//
// It returns the per-pack entries as well as the index, because the index
// itself cannot give them back: a chunk stored in two packs is recorded
// once, so reconstructing the map from it would silently drop the second
// pack's copy.
func Rebuild(ctx context.Context, b backend.Backend, keys *crypto.Keys) (*Index, map[crypto.ID][]pack.Entry, error) {
	var ids []crypto.ID
	err := b.List(ctx, pack.Prefix, func(fi backend.FileInfo) error {
		id, err := crypto.ParseID(fi.Key[len(pack.Prefix):])
		if err != nil {
			return fmt.Errorf("pack %q: %w", fi.Key, err)
		}
		ids = append(ids, id)
		return nil
	})
	if err != nil {
		return nil, nil, fmt.Errorf("list packs: %w", err)
	}

	ix := New()
	packs := make(map[crypto.ID][]pack.Entry, len(ids))
	for _, id := range ids {
		entries, err := pack.ReadTrailer(ctx, b, keys, id)
		if err != nil {
			return nil, nil, fmt.Errorf("rebuild index: %w", err)
		}
		ix.AddPack(id, entries)
		packs[id] = entries
	}
	return ix, packs, nil
}
