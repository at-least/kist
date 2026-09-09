package index

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"slices"
	"strings"
	"sync"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
)

// Version is the index blob schema version.
const Version = 3

// Encoding bytes for the plaintext framing of an index blob: the
// algorithm byte that leads the plaintext (0 raw, 1 zstd), matching the
// chunk payload framing so every reader handles one convention.
const (
	encodingRaw  byte = 0
	encodingZstd byte = 1
)

// ErrNotFound means a chunk is not in the index. It does not mean the
// chunk is absent from the repository: an index is a cache, and the
// authority is the set of pack trailers.
var ErrNotFound = errors.New("chunk not in index")

// ErrCorrupt means an index blob is structurally invalid.
var ErrCorrupt = errors.New("index blob is corrupt")

// A Location says where one chunk's bytes are.
//
// Offset, Length and RawLen are exactly pack.Entry's, so an index entry
// and the trailer it came from can never disagree about what they mean.
type Location struct {
	Pack   crypto.ID
	Offset uint64
	Length uint64
	RawLen uint64
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
// Two packs holding the same chunk is normal -- two clients can pack the
// same content at the same moment -- and either copy serves. Which one
// the index points at is decided by pack ID, smallest wins, and not by
// which pack happened to be added first. That makes the mapping a pure
// function of the set of packs: an index loaded from blobs in blob order
// and one rebuilt from packs in pack order agree on every chunk. Prune
// depends on it, because "which packs are live" is derived from exactly
// this mapping, and a rebuild between two prune runs must not move the
// live copy.
func (ix *Index) AddPack(packID crypto.ID, entries []pack.Entry) {
	ix.mu.Lock()
	defer ix.mu.Unlock()

	ix.sealed[packID] = struct{}{}
	for _, e := range entries {
		if existing, ok := ix.byID[e.ID]; ok && bytes.Compare(existing.Pack[:], packID[:]) <= 0 {
			continue
		}
		ix.byID[e.ID] = Location{Pack: packID, Offset: e.Offset, Length: e.Length, RawLen: e.RawLen}
	}
}

// blob is the on-disk form of an index: packs, each with its entries and
// its total size. Grouping by pack rather than listing flat triples keeps
// the pack ID out of every entry, which is most of the blob's size.
type blob struct {
	Version    uint64      `cbor:"v"`
	Packs      []blobPack  `cbor:"packs"`
	Supersedes []crypto.ID `cbor:"supersedes,omitempty"`
}

type blobPack struct {
	ID      crypto.ID    `cbor:"id"`
	Size    uint64       `cbor:"size"`
	Entries []pack.Entry `cbor:"entries"`
}

// Key returns the repository key an index blob is stored under.
func Key(id crypto.ID) string { return "indexes/" + id.String() }

// Prefix is the repository prefix index blobs live under.
const Prefix = "indexes/"

// PackInfo is one pack as recorded in a blob: its entries and its total
// size, so `check` can catch a truncated or swapped pack with a HEAD.
type PackInfo struct {
	Size    uint64       `cbor:"-"`
	Entries []pack.Entry `cbor:"-"`
}

// A NamedPack pairs a pack ID with its index information, in an order
// the caller chose. SaveOrdered writes them in exactly that order; Save
// sorts by ID.
type NamedPack struct {
	ID   crypto.ID
	Info PackInfo
}

// Save writes the given packs as one index blob and returns its ID,
// ordered by pack ID. See SaveOrdered for why the order is a parameter
// at all.
func Save(ctx context.Context, b backend.Backend, keys *crypto.Keys, packs map[crypto.ID]PackInfo, supersedes []crypto.ID, nonceSource io.Reader) (crypto.ID, error) {
	named := make([]NamedPack, 0, len(packs))
	for id, info := range packs {
		named = append(named, NamedPack{ID: id, Info: info})
	}
	// Map iteration order is randomised, so without this the same set of
	// packs would encode differently on every run and two clients writing
	// the same index would collide instead of deduplicating.
	slices.SortFunc(named, func(a, b NamedPack) int { return bytes.Compare(a.ID[:], b.ID[:]) })
	return SaveOrdered(ctx, b, keys, named, supersedes, nonceSource)
}

// SaveOrdered writes the given packs as one index blob, keeping the
// caller's order.
//
// It is called once at the end of a backup, after the last pack is
// uploaded and before the snapshot is committed. A crash between the
// packs and this call leaves orphaned packs, not a broken repository.
// supersedes lists the blobs this one replaces (prune and rebuild-index);
// readers ignore any blob another effective blob supersedes.
//
// Order matters to readers that take a chunk's first position: prune
// puts unmarked packs ahead of marked ones so a chunk is never resolved
// into a pack that is on its way out.
func SaveOrdered(ctx context.Context, b backend.Backend, keys *crypto.Keys, packs []NamedPack, supersedes []crypto.ID, nonceSource io.Reader) (crypto.ID, error) {
	if len(packs) == 0 {
		return crypto.ID{}, errors.New("save index: no packs to record")
	}

	doc := blob{Version: Version, Packs: make([]blobPack, 0, len(packs)), Supersedes: supersedes}
	for _, np := range packs {
		doc.Packs = append(doc.Packs, blobPack{ID: np.ID, Size: np.Info.Size, Entries: np.Info.Entries})
	}

	encoded, err := crypto.Marshal(doc)
	if err != nil {
		return crypto.ID{}, fmt.Errorf("save index: %w", err)
	}
	// Frame the plaintext with the compression byte; zstd is kept under
	// the same save-more-than-1/16 rule as chunk payloads. Measured on
	// hash-like (incompressible) IDs this still saves ~1.8x.
	framed := frame(encoded)

	sealed, err := crypto.Seal(&keys.Index, []byte(crypto.AADIndexBlob), framed, nonceSource)
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

func frame(plain []byte) []byte {
	compressed := compressIndex(plain)
	if len(compressed) < len(plain)-len(plain)/16 {
		return append([]byte{encodingZstd}, compressed...)
	}
	return append([]byte{encodingRaw}, plain...)
}

func unframe(framed []byte) ([]byte, error) {
	if len(framed) == 0 {
		return nil, fmt.Errorf("index blob: empty plaintext")
	}
	switch framed[0] {
	case encodingRaw:
		return framed[1:], nil
	case encodingZstd:
		out, err := decompressIndex(framed[1:])
		if err != nil {
			return nil, fmt.Errorf("index blob: %w", err)
		}
		return out, nil
	default:
		return nil, fmt.Errorf("index blob: unknown encoding byte %d", framed[0])
	}
}

// load is Load without the merge: it returns the decoded document.
func load(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID) (*blob, error) {
	sealed, err := backend.GetAll(ctx, b, Key(id))
	if err != nil {
		return nil, fmt.Errorf("load index %s: %w", id, err)
	}
	if got := crypto.CiphertextID(sealed); got != id {
		return nil, fmt.Errorf("load index %s: %w: blob hashes to %s", id, ErrCorrupt, got)
	}

	framed, err := crypto.Open(&keys.Index, []byte(crypto.AADIndexBlob), sealed)
	if err != nil {
		return nil, fmt.Errorf("load index %s: %w", id, err)
	}
	encoded, err := unframe(framed)
	if err != nil {
		return nil, fmt.Errorf("load index %s: %w", id, err)
	}

	var doc blob
	if err := crypto.Unmarshal(encoded, &doc); err != nil {
		return nil, fmt.Errorf("load index %s: %w", id, err)
	}
	if doc.Version != Version {
		return nil, fmt.Errorf("load index %s: %w: blob declares version %d, this build reads %d", id, ErrCorrupt, doc.Version, Version)
	}
	for _, p := range doc.Packs {
		if len(p.Entries) == 0 {
			return nil, fmt.Errorf("load index %s: %w: pack %s has no entries", id, ErrCorrupt, p.ID)
		}
	}
	return &doc, nil
}

// Load reads one index blob into ix.
func Load(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID, ix *Index) error {
	doc, err := load(ctx, b, keys, id)
	if err != nil {
		return err
	}
	for _, p := range doc.Packs {
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

// LoadAll merges every effective index blob in the repository.
//
// A blob that cannot be read is skipped and returned in the second
// result, not treated as a failure. The index is a cache: a damaged blob
// means "this repository needs rebuild-index", and making it fatal would
// be a catch-22, because opening the repository is how you get to run
// that command. Only a failure to list is fatal, because then nothing is
// known about what is there.
//
// Blobs named in any surviving blob's supersedes are ignored entirely:
// when prune or rebuild-index has written a replacement but the old blobs
// have not been collected yet, the replacement wins. This is what makes
// overlapping prunes and a rebuild during a prune safe.
func LoadAll(ctx context.Context, b backend.Backend, keys *crypto.Keys) (*Index, []error, error) {
	ids, unusable, err := List(ctx, b)
	if err != nil {
		return nil, nil, err
	}

	var skipped []error
	for _, key := range unusable {
		skipped = append(skipped, fmt.Errorf("%w: %q is not named like an index blob", ErrCorrupt, key))
	}

	docs := make(map[crypto.ID]*blob, len(ids))
	for _, id := range ids {
		doc, err := load(ctx, b, keys, id)
		if err != nil {
			skipped = append(skipped, err)
			continue
		}
		docs[id] = doc
	}

	superseded := make(map[crypto.ID]struct{})
	for _, doc := range docs {
		for _, old := range doc.Supersedes {
			superseded[old] = struct{}{}
		}
	}

	ix := New()
	for _, id := range ids {
		doc, ok := docs[id]
		if !ok {
			continue // already reported
		}
		if _, gone := superseded[id]; gone {
			continue
		}
		for _, p := range doc.Packs {
			ix.AddPack(p.ID, p.Entries)
		}
	}
	return ix, skipped, nil
}

// Rebuild reconstructs an index by reading every pack trailer, ignoring
// the index blobs entirely. It is what proves an index is only a cache.
//
// It returns the per-pack entries (with the sizes a rebuilt blob records)
// as well as the index, because the index itself cannot give them back:
// a chunk stored in two packs is recorded once, so reconstructing the map
// from it would silently drop the second pack's copy.
func Rebuild(ctx context.Context, b backend.Backend, keys *crypto.Keys) (*Index, map[crypto.ID]PackInfo, error) {
	var infos []backend.FileInfo
	err := b.List(ctx, pack.Prefix, func(fi backend.FileInfo) error {
		infos = append(infos, fi)
		return nil
	})
	if err != nil {
		return nil, nil, fmt.Errorf("list packs: %w", err)
	}
	slices.SortFunc(infos, func(a, b backend.FileInfo) int { return strings.Compare(a.Key, b.Key) })

	ix := New()
	packs := make(map[crypto.ID]PackInfo, len(infos))
	for _, fi := range infos {
		id, err := crypto.ParseID(fi.Key[len(pack.Prefix):])
		if err != nil {
			return nil, nil, fmt.Errorf("pack %q: %w", fi.Key, err)
		}
		entries, err := pack.ReadTrailer(ctx, b, keys, id)
		if err != nil {
			return nil, nil, fmt.Errorf("rebuild index: %w", err)
		}
		ix.AddPack(id, entries)
		packs[id] = PackInfo{Size: uint64(max(fi.Size, 0)), Entries: entries}
	}
	return ix, packs, nil
}
