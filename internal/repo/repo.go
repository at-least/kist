package repo

import (
	"context"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
)

// A Repository is an open, unlocked repository.
//
// Opening it costs one config read, one Argon2id derivation and a listing
// of the index blobs. It holds no lock: several clients may have the same
// repository open, and the format is what keeps them from harming each
// other.
type Repository struct {
	backend  backend.Backend
	config   *Config
	keys     *crypto.Keys
	index    *index.Index
	clientID string

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

	// NonceSource seeds the nonces of everything this repository writes.
	// Production leaves it nil, meaning crypto/rand.
	NonceSource io.Reader

	// Now overrides the clock. Production leaves it nil.
	Now func() time.Time
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

	kdf := crypto.DefaultKDFParams()
	if opts.KDF != nil {
		kdf = *opts.KDF
	}
	now := opts.clock()()
	slot, err := crypto.NewKeySlot(opts.Password, repoID, master, kdf, now, opts.NonceSource)
	if err != nil {
		return nil, fmt.Errorf("init repository: %w", err)
	}

	cfg := newConfig(repoID, slot, now)
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

	master, err := cfg.Slot.Unwrap(opts.Password, cfg.RepoID)
	if err != nil {
		return nil, fmt.Errorf("open repository at %s: %w", b.Location(), err)
	}
	return open(ctx, b, cfg, master, opts)
}

func open(ctx context.Context, b backend.Backend, cfg *Config, master crypto.Key, opts Options) (*Repository, error) {
	keys, err := crypto.DeriveKeys(master, cfg.RepoID)
	if err != nil {
		return nil, fmt.Errorf("open repository at %s: %w", b.Location(), err)
	}

	clientID := opts.ClientID
	if clientID == "" {
		stateDir := opts.StateDir
		if stateDir == "" {
			if stateDir, err = DefaultStateDir(); err != nil {
				return nil, err
			}
		}
		if clientID, err = ClientID(stateDir, hexRepoID(cfg.RepoID)); err != nil {
			return nil, err
		}
	}
	if err := validateClientID(clientID); err != nil {
		return nil, err
	}

	ix, err := index.LoadAll(ctx, b, keys)
	if err != nil {
		return nil, fmt.Errorf("open repository at %s: %w", b.Location(), err)
	}

	return &Repository{
		backend:     b,
		config:      cfg,
		keys:        keys,
		index:       ix,
		clientID:    clientID,
		nonceSource: opts.NonceSource,
		now:         opts.clock(),
	}, nil
}

// Backend returns the storage this repository sits on.
func (r *Repository) Backend() backend.Backend { return r.backend }

// Config returns the repository's parameter block.
func (r *Repository) Config() *Config { return r.config }

// ClientID is the identifier this client writes snapshots under.
func (r *Repository) ClientID() string { return r.clientID }

// Index is the chunk index this repository was opened with.
func (r *Repository) Index() *index.Index { return r.index }

// Close releases the backend.
func (r *Repository) Close() error {
	if err := r.backend.Close(); err != nil {
		return fmt.Errorf("close repository: %w", err)
	}
	return nil
}

// RebuildIndex discards the cached index and reconstructs it from pack
// trailers, then writes it back as a single blob.
func (r *Repository) RebuildIndex(ctx context.Context) (int, error) {
	ix, err := index.Rebuild(ctx, r.backend, r.keys)
	if err != nil {
		return 0, err
	}
	r.index = ix
	return ix.Len(), nil
}

func hexRepoID(id crypto.RepoID) string {
	const digits = "0123456789abcdef"

	out := make([]byte, 0, 2*len(id))
	for _, b := range id {
		out = append(out, digits[b>>4], digits[b&0x0f])
	}
	return string(out)
}
