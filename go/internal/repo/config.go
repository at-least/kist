package repo

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/chunker"
	"github.com/at-least/kist/internal/crypto"
)

// ConfigKey is where a repository's parameters live. It is the only
// object in a repository that is not encrypted, and the only one the
// format permits replacing -- a future `key add` has to rewrite its key
// slots. Nothing does today: it is written with PutIfAbsent, and Init
// checks for it first.
const ConfigKey = "config"

// ConfigVersion is the schema version of the config object. It is also
// this build's format version: the min_reader gate compares a
// repository's requirement against it.
const ConfigVersion = 3

// ErrCorrupt means a repository's config is unusable.
var ErrCorrupt = errors.New("repository config is corrupt")

// ErrConfigTampered means the decrypted invariants disagree with the
// plaintext config: someone edited repo_id or the chunker parameters
// after the repository was created. The authenticated copy is the
// authority; the failure is explicit because a silent one would change
// what every content address means.
var ErrConfigTampered = errors.New("plaintext config does not match the authenticated invariants (repo_id or chunker parameters were tampered)")

// ErrReaderTooOld means the repository requires a newer reader than this
// build (config.min_reader is above the version this build reads).
var ErrReaderTooOld = errors.New("repository requires a newer kist reader")

// ChunkerParams records the chunk sizes a repository was created with.
//
// v3 treats them as invariants: they are bound into the wrapped master
// key's authenticated payload, and a plaintext config that disagrees
// with what was decrypted fails the open with an explicit tamper error.
type ChunkerParams struct {
	MinSize uint32 `cbor:"min"`
	AvgSize uint32 `cbor:"avg"`
	MaxSize uint32 `cbor:"max"`
}

// DefaultChunkerParams is what Init writes.
func DefaultChunkerParams() ChunkerParams {
	return ChunkerParams{MinSize: chunker.MinSize, AvgSize: chunker.AvgSize, MaxSize: chunker.MaxSize}
}

// DefaultPackTargetSize bounds how large a pack grows before it is
// flushed. The default matches the Go v1 constant and the Rust default.
const DefaultPackTargetSize uint64 = 64 << 20

// chunkerParams converts to the chunker package's type.
func (c ChunkerParams) chunkerParams() chunker.Params {
	return chunker.Params{Min: c.MinSize, Avg: c.AvgSize, Max: c.MaxSize}
}

// invariantsChunker is the view of the parameters the wrapped key
// carries.
func (c ChunkerParams) invariantsChunker() crypto.InvariantsChunker {
	return crypto.InvariantsChunker{Min: c.MinSize, Avg: c.AvgSize, Max: c.MaxSize}
}

// validate rejects plaintext parameters that are nonsense before any of
// them reaches the chunker or an allocation.
func (c ChunkerParams) validate() error {
	const (
		minMin = 64
		maxMin = 1 << 20
		minAvg = 256
		maxAvg = 16 << 20
		minMax = 1 << 10
		maxMax = 64 << 20
	)
	switch {
	case c.MinSize < minMin || c.MinSize > maxMin:
		return fmt.Errorf("%w: chunker.min %d is outside %d..%d", ErrCorrupt, c.MinSize, minMin, maxMin)
	case c.AvgSize < minAvg || c.AvgSize > maxAvg:
		return fmt.Errorf("%w: chunker.avg %d is outside %d..%d", ErrCorrupt, c.AvgSize, minAvg, maxAvg)
	case c.MaxSize < minMax || c.MaxSize > maxMax:
		return fmt.Errorf("%w: chunker.max %d is outside %d..%d", ErrCorrupt, c.MaxSize, minMax, maxMax)
	case c.MinSize > c.AvgSize || c.AvgSize > c.MaxSize:
		return fmt.Errorf("%w: chunk sizes %d/%d/%d do not satisfy min <= avg <= max", ErrCorrupt, c.MinSize, c.AvgSize, c.MaxSize)
	}
	return nil
}

// Config is the repository's public parameter block.
//
// It is plaintext on purpose. The KDF parameters and salt must be
// readable before any key exists, and everything else here -- the
// repository ID, the chunk sizes, the creation time -- is already
// derivable by anyone who can list the repository. The repo_id and the
// chunker parameters are ALSO in the wrapped key's authenticated
// payload; this copy is a hint that must match it.
//
// Field order is the spec table (docs/format.md §4.1): v, repo_id,
// created, chunker, pack_target, min_reader, replicas, slot.
type Config struct {
	Version uint64 `cbor:"v"`

	RepoID crypto.RepoID `cbor:"repo_id"`

	CreatedUnixNs int64 `cbor:"created"`

	Chunker ChunkerParams `cbor:"chunker"`

	// PackTargetSize is how large a pack grows before it is flushed. It
	// is adjustable per repository (not an invariant) because it changes
	// no content address, only batching.
	PackTargetSize uint64 `cbor:"pack_target"`

	// MinReader is the lowest format version that can safely read this
	// repository. A client whose version is lower refuses at open with an
	// explicit error instead of half-reading through ignored fields. It
	// only ever goes up.
	MinReader uint16 `cbor:"min_reader"`

	// Replicas is 1 when trees and snapshots are also stored at "<key>.r1"
	// (identical bytes, PutIfAbsent), 0 otherwise. It is write-side
	// policy, not an invariant.
	Replicas uint8 `cbor:"replicas"`

	Slot crypto.KeySlot `cbor:"slot"`
}

// LoadConfig reads and validates a repository's config.
func LoadConfig(ctx context.Context, b backend.Backend) (*Config, error) {
	encoded, err := backend.GetAll(ctx, b, ConfigKey)
	if err != nil {
		return nil, fmt.Errorf("read config from %s: %w", b.Location(), err)
	}

	var cfg Config
	if err := crypto.Unmarshal(encoded, &cfg); err != nil {
		return nil, fmt.Errorf("read config from %s: %w", b.Location(), err)
	}
	if cfg.Version != ConfigVersion {
		return nil, fmt.Errorf("%w: repository declares format version %d, this build reads %d", ErrCorrupt, cfg.Version, ConfigVersion)
	}
	if err := cfg.Chunker.validate(); err != nil {
		return nil, err
	}
	const (
		minPack = 64 << 10
		maxPack = 4 << 30
	)
	if cfg.PackTargetSize < minPack || cfg.PackTargetSize > maxPack || cfg.PackTargetSize < uint64(cfg.Chunker.MaxSize) {
		return nil, fmt.Errorf("%w: pack_target %d is outside %d..=%d or below chunker.max", ErrCorrupt, cfg.PackTargetSize, minPack, maxPack)
	}
	// The min_reader gate (docs/format.md §11): a repository written
	// by a newer format says so, and this build refuses rather than
	// guessing. The range check keeps a corrupted value from being
	// meaningless in the other direction too.
	if cfg.MinReader < ConfigVersion || uint64(cfg.MinReader) > cfg.Version {
		return nil, fmt.Errorf("%w: min_reader %d is outside %d..=%d", ErrCorrupt, cfg.MinReader, ConfigVersion, cfg.Version)
	}
	if cfg.MinReader > ConfigVersion {
		return nil, fmt.Errorf("%w: repository requires a reader of format v%d or newer; this build reads v%d", ErrReaderTooOld, cfg.MinReader, ConfigVersion)
	}
	if cfg.Replicas > 1 {
		return nil, fmt.Errorf("%w: replicas %d is outside 0..=1", ErrCorrupt, cfg.Replicas)
	}
	return &cfg, nil
}

// checkMatches compares the decrypted invariants with this config. Any
// mismatch is a tampered config, and must fail loudly: the invariants
// decide what every content address means.
func (c *Config) checkMatches(inv crypto.Invariants) error {
	if err := inv.Validate(); err != nil {
		return fmt.Errorf("%w: %w", ErrCorrupt, err)
	}
	sameRepo := len(inv.RepoID) == len(c.RepoID) && string(inv.RepoID) == string(c.RepoID[:])
	if !sameRepo || inv.Chunker != c.Chunker.invariantsChunker() {
		return ErrConfigTampered
	}
	return nil
}

// saveConfig writes the config object.
func saveConfig(ctx context.Context, b backend.Backend, cfg *Config) error {
	encoded, err := crypto.Marshal(cfg)
	if err != nil {
		return fmt.Errorf("encode config: %w", err)
	}
	if err := backend.PutBytesIfAbsent(ctx, b, ConfigKey, encoded); err != nil {
		return fmt.Errorf("write config to %s: %w", b.Location(), err)
	}
	return nil
}

func newConfig(repoID crypto.RepoID, slot *crypto.KeySlot, now time.Time, replicas uint8) *Config {
	return &Config{
		Version:        ConfigVersion,
		RepoID:         repoID,
		CreatedUnixNs:  now.UTC().UnixNano(),
		Chunker:        DefaultChunkerParams(),
		PackTargetSize: DefaultPackTargetSize,
		MinReader:      ConfigVersion,
		Replicas:       replicas,
		Slot:           *slot,
	}
}
