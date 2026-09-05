package repo

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/snapshot"
)

// A Repository is an open, unlocked repository.
//
// Opening it costs one config read, one Argon2id derivation and a listing
// of the index blobs. It holds no lock: several clients may have the same
// repository open, and the format is what keeps them from harming each
// other.
type Repository struct {
	backend backend.Backend
	config  *Config
	keys    *crypto.Keys
	index   *index.Index
	// clientID is the 16-byte machine identity; clientIDHex is what the
	// snapshot keys carry.
	clientID    []byte
	clientIDHex string

	// indexSource is where index blobs are read from: the local cache
	// when there is one, the backend otherwise.
	indexSource backend.Backend

	// nonceSource is nil in production, meaning crypto/rand. Tests set it
	// so that a whole repository can be reproduced byte for byte.
	nonceSource io.Reader

	// now is time.Now in production; tests replace it.
	now func() time.Time
}

// Options configure Init and Open.
type Options struct {
	// Password opens the repository's key slot.
	Password []byte

	// ClientID identifies this machine. Empty means the persisted one for
	// this repository, minted on first use.
	ClientID string

	// StateDir is where the client ID lives. Empty means the user's
	// configuration directory.
	StateDir string

	// KDF sets the Argon2id cost for Init. The zero value means
	// crypto.DefaultKDFParams.
	KDF *crypto.KDFParams

	// Chunker sets the chunk sizes for Init. Nil means
	// config.DefaultChunkerParams. The values are written to the config
	// and bound into the master-key AAD: every client of the repository
	// must agree, and a tampered config fails the unwrap rather than
	// silently breaking deduplication.
	Chunker *ChunkerParams

	// NonceSource seeds the nonces of everything this repository writes.
	// Production leaves it nil, meaning crypto/rand.
	NonceSource io.Reader

	// Now overrides the clock. Production leaves it nil.
	Now func() time.Time

	// CacheDir holds per-repository caches. Empty means the user's cache
	// directory; NoCache disables caching entirely.
	CacheDir string
	NoCache  bool

	// Warnf receives non-fatal problems found while opening: an index
	// blob that will not read, for instance. Those are repairable with
	// rebuild-index and must not stop the repository opening, but they
	// must be said out loud.
	Warnf func(format string, args ...any)
}

func (o Options) warn(format string, args ...any) {
	if o.Warnf != nil {
		o.Warnf(format, args...)
	}
}

func (o Options) clock() func() time.Time {
	if o.Now != nil {
		return o.Now
	}
	return time.Now
}

// ErrAlreadyInitialised means the backend already holds a repository.
var ErrAlreadyInitialised = errors.New("repository already initialised")

// Init creates a repository on an empty backend.
//
// It mints a master key, wraps it under the password, and writes the
// config. Nothing else is written: a repository with no snapshots is a
// config object and nothing more.
func Init(ctx context.Context, b backend.Backend, opts Options) (*Repository, error) {
	switch exists, err := backend.Exists(ctx, b, ConfigKey); {
	case err != nil:
		return nil, fmt.Errorf("init repository at %s: %w", b.Location(), err)
	case exists:
		return nil, fmt.Errorf("init repository at %s: %w", b.Location(), ErrAlreadyInitialised)
	}
	if len(opts.Password) == 0 {
		return nil, errors.New("init repository: a password is required; kist does not support unencrypted repositories")
	}

	var repoID crypto.RepoID
	source := opts.NonceSource
	if source == nil {
		source = rand.Reader
	}
	if _, err := io.ReadFull(source, repoID[:]); err != nil {
		return nil, fmt.Errorf("init repository: generate repository ID: %w", err)
	}

	var master crypto.Key
	if _, err := io.ReadFull(source, master[:]); err != nil {
		return nil, fmt.Errorf("init repository: generate master key: %w", err)
	}

	now := opts.clock()()
	params := crypto.DefaultKDFParams()
	if opts.KDF != nil {
		params = *opts.KDF
	}
	sizes := DefaultChunkerParams()
	if opts.Chunker != nil {
		sizes = *opts.Chunker
	}
	if err := sizes.validate(); err != nil {
		return nil, fmt.Errorf("init repository: %w", err)
	}
	aad := crypto.MasterAAD(repoID, sizes.MinSize, sizes.AvgSize, sizes.MaxSize)
	slot, err := crypto.NewKeySlot(opts.Password, aad, master, params, now, opts.NonceSource)
	if err != nil {
		return nil, fmt.Errorf("init repository: %w", err)
	}

	cfg := newConfig(repoID, slot, now)
	cfg.Chunker = sizes
	if err := saveConfig(ctx, b, cfg); err != nil {
		return nil, fmt.Errorf("init repository: %w", err)
	}

	return open(ctx, b, cfg, master, opts)
}

// Open unlocks an existing repository.
func Open(ctx context.Context, b backend.Backend, opts Options) (*Repository, error) {
	cfg, err := LoadConfig(ctx, b)
	if err != nil {
		return nil, err
	}

	aad := crypto.MasterAAD(cfg.RepoID, cfg.Chunker.MinSize, cfg.Chunker.AvgSize, cfg.Chunker.MaxSize)
	master, err := cfg.Slot.Unwrap(opts.Password, aad)
	if err != nil {
		return nil, fmt.Errorf("open repository at %s: %w", b.Location(), err)
	}
	return open(ctx, b, cfg, master, opts)
}

func open(ctx context.Context, b backend.Backend, cfg *Config, master crypto.Key, opts Options) (*Repository, error) {
	keys := crypto.DeriveKeys(master)

	clientID := opts.ClientID
	if clientID == "" {
		stateDir := opts.StateDir
		if stateDir == "" {
			dir, err := DefaultStateDir()
			if err != nil {
				return nil, err
			}
			stateDir = dir
		}
		id, err := ClientID(stateDir, hexRepoID(cfg.RepoID))
		if err != nil {
			return nil, err
		}
		clientID = id
	}
	if err := validateClientID(clientID); err != nil {
		return nil, err
	}

	// Index blobs are read through a local cache. The backend the
	// repository keeps for everything else is the undecorated one: only
	// the index-loading path benefits, and a decorator on every call is
	// one more thing for a reader of Backup to think about.
	source := b
	var cache *indexCache
	if !opts.NoCache {
		root := opts.CacheDir
		if root == "" {
			dir, err := DefaultCacheDir()
			if err != nil {
				return nil, err
			}
			root = dir
		}
		c, err := newIndexCache(b, root, cfg.RepoID)
		if err != nil {
			opts.warn("%v; continuing without an index cache", err)
		} else {
			cache = c
			source = cache
		}
	}

	ix, skipped, err := index.LoadAll(ctx, source, keys)
	if err != nil {
		return nil, fmt.Errorf("open repository at %s: %w", b.Location(), err)
	}
	for _, s := range skipped {
		opts.warn("%v; run `kist rebuild-index` to repair the index", s)
	}
	if cache != nil {
		if listed, _, err := index.List(ctx, b); err == nil {
			cache.prune(listed)
		}
	}

	// Every nonce this repository writes comes out of one stream, so a
	// caller that hands over a source which does not advance still cannot
	// reuse a nonce. pack.Writer already did this for chunks; trees,
	// snapshots and index blobs need it just as much.
	nonces, err := crypto.NonceStream(opts.NonceSource)
	if err != nil {
		return nil, fmt.Errorf("open repository at %s: %w", b.Location(), err)
	}

	var clientBytes []byte
	if raw, err := hex.DecodeString(clientID); err == nil && len(raw) == 16 {
		clientBytes = raw
	} else {
		clientBytes = []byte(clientID) // tests may set arbitrary ids
	}
	return &Repository{
		backend:     b,
		config:      cfg,
		keys:        keys,
		index:       ix,
		clientID:    clientBytes,
		clientIDHex: clientID,
		indexSource: source,
		nonceSource: nonces,
		now:         opts.clock(),
	}, nil
}

// refreshIndex reloads the index from the stored blobs.
//
// A process that stays open across backups would otherwise deduplicate
// against a view from when it opened, and prune's safety argument
// assumes a backup's view of the index is no older than the backup.
func (r *Repository) refreshIndex(ctx context.Context, warn func(string, ...any)) error {
	ix, skipped, err := index.LoadAll(ctx, r.indexSource, r.keys)
	if err != nil {
		return err
	}
	for _, s := range skipped {
		warn("%v; run `kist rebuild-index` to repair the index", s)
	}
	r.index = ix
	return nil
}

// Backend returns the storage this repository sits on.
func (r *Repository) Backend() backend.Backend { return r.backend }

// Config returns the repository's parameter block.
func (r *Repository) Config() *Config { return r.config }

// ClientID is the identifier this client writes snapshots under (hex).
func (r *Repository) ClientID() string { return r.clientIDHex }

// Index is the chunk index this repository was opened with.
func (r *Repository) Index() *index.Index { return r.index }

// Close releases the backend.
func (r *Repository) Close() error {
	if err := r.backend.Close(); err != nil {
		return fmt.Errorf("close repository: %w", err)
	}
	return nil
}

// RebuildIndex discards the cached index, reconstructs it by reading
// every pack trailer, and replaces the stored index blobs with one blob
// describing everything.
func (r *Repository) RebuildIndex(ctx context.Context) (int, error) {
	stale, unusable, err := index.List(ctx, r.backend)
	if err != nil {
		return 0, err
	}
	_, packs, err := index.Rebuild(ctx, r.backend, r.keys)
	if err != nil {
		return 0, err
	}
	if err := r.replaceIndex(ctx, stale, unusable, packs); err != nil {
		return 0, fmt.Errorf("rebuild index: %w", err)
	}
	return r.index.Len(), nil
}

func hexRepoID(id crypto.RepoID) string {
	const digits = "0123456789abcdef"

	out := make([]byte, 0, 2*len(id))
	for _, b := range id {
		out = append(out, digits[b>>4], digits[b&0x0f])
	}
	return string(out)
}

// Snapshots lists the repository's snapshots, oldest first. An empty
// clientID lists every client's.
func (r *Repository) Snapshots(ctx context.Context, clientID string) ([]snapshot.Handle, error) {
	return snapshot.List(ctx, r.backend, clientID)
}

// LoadSnapshot reads one snapshot.
//
// This exists so that callers never need the repository's keys. Handing
// out crypto.Keys would make every future caller a place key material can
// escape from, and the CLI has no reason to hold it.
func (r *Repository) LoadSnapshot(ctx context.Context, key string) (*snapshot.Snapshot, error) {
	return snapshot.Load(ctx, r.backend, r.keys, key)
}
