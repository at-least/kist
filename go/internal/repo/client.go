package repo

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// ClientIDSize is the length of a client identifier in bytes.
const ClientIDSize = 16

// ClientID identifies the machine writing snapshots, so that two clients
// never contend for one key.
//
// It is a random value stored locally, not the hostname. Hostnames repeat
// across a fleet, change when a machine is renamed, and leak information
// into a repository listing that a backup tool has no reason to publish.
// The hostname is recorded inside the snapshot as a human label instead.
//
// The cost of this choice: a container that starts with an empty
// filesystem every run mints a new client each time, and a repository
// accumulates single-snapshot clients. That is M3's problem -- a client
// with no snapshot for several grace periods is forgotten -- and it is
// noted in ADR 004 so that it is a known cost, not a surprise.
func ClientID(stateDir string, repoID string) (string, error) {
	path := filepath.Join(stateDir, "clients", repoID)

	// The read-mint-write below must be atomic across processes: two
	// concurrent first runs would each mint a different id and the last
	// writer wins, leaving the loser with a snapshot no later client
	// recognizes. An exclusive flock on a sidecar lock file serializes
	// the sequence (the Rust peer's client_id::lock does the same); the
	// flock releases when the process exits or the file closes.
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return "", fmt.Errorf("create client ID directory: %w", err)
	}
	lock, err := os.OpenFile(path+".lock", os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return "", fmt.Errorf("open client ID lock: %w", err)
	}
	defer lock.Close()
	if err := lockClientIDFile(lock); err != nil {
		return "", fmt.Errorf("lock client ID: %w", err)
	}

	switch data, err := os.ReadFile(path); { //nolint:gosec // path is built from a validated repository ID
	case err == nil:
		id := strings.TrimSpace(string(data))
		if err := validateClientID(id); err != nil {
			return "", fmt.Errorf("client ID in %s: %w", path, err)
		}
		return id, nil
	case !errors.Is(err, fs.ErrNotExist):
		return "", fmt.Errorf("read client ID from %s: %w", path, err)
	}

	id, err := newClientID()
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(path, []byte(id+"\n"), 0o600); err != nil {
		return "", fmt.Errorf("write client ID to %s: %w", path, err)
	}
	return id, nil
}

// DefaultStateDir is where kist keeps per-user state that is not part of
// any repository.
func DefaultStateDir() (string, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", fmt.Errorf("locate the user configuration directory: %w", err)
	}
	return filepath.Join(dir, "kist"), nil
}

func newClientID() (string, error) {
	var raw [ClientIDSize]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return "", fmt.Errorf("generate client ID: %w", err)
	}
	return hex.EncodeToString(raw[:]), nil
}

func validateClientID(id string) error {
	if len(id) != 2*ClientIDSize {
		return fmt.Errorf("%w: %q is %d characters, want %d", ErrCorrupt, id, len(id), 2*ClientIDSize)
	}
	if _, err := hex.DecodeString(id); err != nil {
		return fmt.Errorf("%w: %q is not hex: %w", ErrCorrupt, id, err)
	}
	return nil
}
