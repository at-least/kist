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
const ConfigVersion = 1

// ErrCorrupt means a repository's config is unusable.
var ErrCorrupt = errors.New("repository config is corrupt")

// ChunkerParams records the chunk sizes a repository was created with.
//
// They are stored even though this build only supports one set, because
// a repository whose chunker differs from the reader's deduplicates
// against nothing and must say so instead of silently doubling in size.
type ChunkerParams struct {
	MinSize uint32 `cbor:"min"`
	AvgSize uint32 `cbor:"avg"`
	MaxSize uint32 `cbor:"max"`
}

func currentChunkerParams() ChunkerParams {
	return ChunkerParams{MinSize: chunker.MinSize, AvgSize: chunker.AvgSize, MaxSize: chunker.MaxSize}
}

// Config is the repository's public parameter block.
//
// It is plaintext on purpose. The KDF parameters and salt must be
// readable before any key exists, and everything else here -- the
// repository ID, the chunk sizes, the creation time -- is already
// derivable by anyone who can list the repository.
type Config struct {
	Version       uint64         `cbor:"v"`
	RepoID        crypto.RepoID  `cbor:"repo_id"`
	CreatedUnixNs int64          `cbor:"created"`
	Chunker       ChunkerParams  `cbor:"chunker"`
	Slot          crypto.KeySlot `cbor:"slot"`
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
	if want := currentChunkerParams(); cfg.Chunker != want {
		return nil, fmt.Errorf("%w: repository was created with chunk sizes min=%d avg=%d max=%d, this build uses min=%d avg=%d max=%d; writing to it would deduplicate against nothing",
			ErrCorrupt, cfg.Chunker.MinSize, cfg.Chunker.AvgSize, cfg.Chunker.MaxSize, want.MinSize, want.AvgSize, want.MaxSize)
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
		Version:       ConfigVersion,
		RepoID:        repoID,
		CreatedUnixNs: now.UTC().UnixNano(),
		Chunker:       currentChunkerParams(),
		Slot:          *slot,
	}
}
