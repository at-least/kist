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

// ConfigVersion is the schema version of the config object.
const ConfigVersion = 2

// ErrCorrupt means a repository's config is unusable.
var ErrCorrupt = errors.New("repository config is corrupt")

// ChunkerParams records the chunk sizes a repository was created with.
//
// v2 reads them as parameters, not constants: a repository's clients must
// agree on them (they are bound into the master-key AAD, so tampering
// with the plaintext config fails the unwrap rather than silently
// breaking deduplication), but different repositories may differ within
// the validated ranges.
type ChunkerParams struct {
	MinSize uint32 `cbor:"min"`
	AvgSize uint32 `cbor:"avg"`
	MaxSize uint32 `cbor:"max"`
}

// DefaultChunkerParams is what Init writes.
func DefaultChunkerParams() ChunkerParams {
	return ChunkerParams{MinSize: chunker.MinSize, AvgSize: chunker.AvgSize, MaxSize: chunker.MaxSize}
}

// PackTargetSize bounds how large a pack grows before it is flushed.
// The default matches the Go v1 constant and the Rust default.
const DefaultPackTargetSize uint64 = 64 << 20

// chunkerParams converts to the chunker package's type.
func (c ChunkerParams) chunkerParams() chunker.Params {
	return chunker.Params{Min: c.MinSize, Avg: c.AvgSize, Max: c.MaxSize}
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
// derivable by anyone who can list the repository.
type Config struct {
	Version uint64 `cbor:"v"`

	RepoID crypto.RepoID `cbor:"repo_id"`

	CreatedUnixNs int64 `cbor:"created"`

	Chunker ChunkerParams `cbor:"chunker"`

	// PackTargetSize is how large a pack grows before it is flushed. It
	// is adjustable per repository (not bound into any AAD) because it
	// changes no content address, only batching.
	PackTargetSize uint64 `cbor:"pack_target"`

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
	return &cfg, nil
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

func newConfig(repoID crypto.RepoID, slot *crypto.KeySlot, now time.Time) *Config {
	return &Config{
		Version:        ConfigVersion,
		RepoID:         repoID,
		CreatedUnixNs:  now.UTC().UnixNano(),
		Chunker:        DefaultChunkerParams(),
		PackTargetSize: DefaultPackTargetSize,
		Slot:           *slot,
	}
}
