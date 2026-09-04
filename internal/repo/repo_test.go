package repo

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

const testPassword = "correct horse battery staple"

// cheapKDF keeps Argon2id out of the way of tests that are about
// something else. Production parameters are exercised in the crypto
// package.
func cheapKDF() *crypto.KDFParams {
	p := crypto.DefaultKDFParams()
	p.Time, p.MemoryKiB, p.Threads = 1, 8, 1
	return &p
}

func testOptions(t *testing.T, seed string) Options {
	t.Helper()

	at := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	return Options{
		Password:    []byte(testPassword),
		ClientID:    "00112233445566778899aabbccddeeff",
		StateDir:    t.TempDir(),
		KDF:         cheapKDF(),
		NonceSource: crypto.DeterministicReader(seed),
		Now: func() time.Time {
			at = at.Add(time.Second)
			return at
		},
	}
}

func initRepo(t *testing.T, seed string) (*Repository, string) {
	t.Helper()

	dir := filepath.Join(t.TempDir(), "repo")
	b, err := backend.CreateLocal(dir)
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}

	r, err := Init(context.Background(), b, testOptions(t, seed))
	if err != nil {
		t.Fatalf("init: %v", err)
	}
	t.Cleanup(func() {
		if err := r.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return r, dir
}

func reopen(t *testing.T, dir, seed string) *Repository {
	t.Helper()

	b, err := backend.OpenLocal(dir)
	if err != nil {
		t.Fatalf("open backend: %v", err)
	}
	r, err := Open(context.Background(), b, testOptions(t, seed))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() {
		if err := r.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return r
}

func TestInitThenOpen(t *testing.T) {
	r, dir := initRepo(t, "init")

	if r.Config().Version != ConfigVersion {
		t.Errorf("config version = %d, want %d", r.Config().Version, ConfigVersion)
	}
	if r.Config().Chunker != currentChunkerParams() {
		t.Errorf("chunker params = %+v, want %+v", r.Config().Chunker, currentChunkerParams())
	}

	// The config is the only plaintext object, and it must be readable
	// before any key exists.
	raw, err := backend.GetAll(context.Background(), r.Backend(), ConfigKey)
	if err != nil {
		t.Fatalf("read config: %v", err)
	}
	var cfg Config
	if err := crypto.Unmarshal(raw, &cfg); err != nil {
		t.Fatalf("config is not readable as plain CBOR: %v", err)
	}
	if cfg.Slot.KDF.Alg != crypto.KDFAlgArgon2id {
		t.Errorf("kdf = %q, want %q", cfg.Slot.KDF.Alg, crypto.KDFAlgArgon2id)
	}

	reopened := reopen(t, dir, "reopen")
	if reopened.Config().RepoID != r.Config().RepoID {
		t.Error("reopening produced a different repository ID")
	}
}

func TestInitRefusesToOverwrite(t *testing.T) {
	_, dir := initRepo(t, "overwrite")

	b, err := backend.OpenLocal(dir)
	if err != nil {
		t.Fatalf("open backend: %v", err)
	}
	if _, err := Init(context.Background(), b, testOptions(t, "again")); !errors.Is(err, ErrAlreadyInitialised) {
		t.Fatalf("init over an existing repository: err = %v, want ErrAlreadyInitialised", err)
	}
}

func TestInitRequiresAPassword(t *testing.T) {
	b, err := backend.CreateLocal(filepath.Join(t.TempDir(), "repo"))
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}

	opts := testOptions(t, "nopass")
	opts.Password = nil
	if _, err := Init(context.Background(), b, opts); err == nil || !strings.Contains(err.Error(), "password") {
		t.Fatalf("init without a password: err = %v, want a password error", err)
	}
}

func TestOpenRejectsTheWrongPassword(t *testing.T) {
	_, dir := initRepo(t, "password")

	b, err := backend.OpenLocal(dir)
	if err != nil {
		t.Fatalf("open backend: %v", err)
	}
	opts := testOptions(t, "wrong")
	opts.Password = []byte("not the password")

	if _, err := Open(context.Background(), b, opts); !errors.Is(err, crypto.ErrWrongPassword) {
		t.Fatalf("open with the wrong password: err = %v, want ErrWrongPassword", err)
	}
}

// A repository written with different chunk sizes deduplicates against
// nothing. Refusing to open it is better than silently doubling it.
func TestOpenRejectsForeignChunkerParameters(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "chunker")

	cfg := *r.Config()
	cfg.Chunker.AvgSize *= 2
	encoded, err := crypto.Marshal(&cfg)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if err := r.Backend().Put(ctx, ConfigKey, strings.NewReader(string(encoded)), int64(len(encoded))); err != nil {
		t.Fatalf("write config: %v", err)
	}

	b, err := backend.OpenLocal(dir)
	if err != nil {
		t.Fatalf("open backend: %v", err)
	}
	if _, err := Open(ctx, b, testOptions(t, "foreign")); !errors.Is(err, ErrCorrupt) {
		t.Fatalf("open: err = %v, want ErrCorrupt", err)
	}
}

func TestClientIDIsPersistedAndReused(t *testing.T) {
	stateDir := t.TempDir()

	first, err := ClientID(stateDir, "repo0001")
	if err != nil {
		t.Fatalf("client id: %v", err)
	}
	if len(first) != 2*ClientIDSize {
		t.Errorf("client id %q is %d characters, want %d", first, len(first), 2*ClientIDSize)
	}

	again, err := ClientID(stateDir, "repo0001")
	if err != nil {
		t.Fatalf("client id: %v", err)
	}
	if again != first {
		t.Errorf("second call returned %q, want the persisted %q", again, first)
	}

	// A different repository gets a different client ID, so that one
	// machine's identity in one repository says nothing about another.
	other, err := ClientID(stateDir, "repo0002")
	if err != nil {
		t.Fatalf("client id: %v", err)
	}
	if other == first {
		t.Error("two repositories share a client ID")
	}
}

func TestClientIDRejectsGarbageOnDisk(t *testing.T) {
	stateDir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(stateDir, "clients"), 0o700); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(stateDir, "clients", "repo0001"), []byte("not-a-client-id"), 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}

	if _, err := ClientID(stateDir, "repo0001"); !errors.Is(err, ErrCorrupt) {
		t.Fatalf("client id: err = %v, want ErrCorrupt", err)
	}
}

// createLocalAt and openLocalAt keep the acceptance test from importing
// the backend package under a name that shadows its own helpers.
func createLocalAt(dir string) (backend.Backend, error) { return backend.CreateLocal(dir) }
func openLocalAt(dir string) (backend.Backend, error)   { return backend.OpenLocal(dir) }
