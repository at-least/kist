package backend

import (
	"context"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

func newTestLocal(t *testing.T) Backend {
	t.Helper()
	b, err := CreateLocal(filepath.Join(t.TempDir(), "repo"))
	if err != nil {
		t.Fatalf("create local backend: %v", err)
	}
	t.Cleanup(func() {
		if err := b.Close(); err != nil {
			t.Errorf("close backend: %v", err)
		}
	})
	return b
}

func TestLocalConformance(t *testing.T) {
	runConformance(t, newTestLocal)
}

// Two clients writing the same content-addressed object is the normal
// case, not the exception: exactly one link succeeds and the other is
// told the bytes are already there.
func TestLocalPutIfAbsentHasOneWinner(t *testing.T) {
	ctx := context.Background()
	b := newTestLocal(t)

	const writers = 16
	var (
		wg      sync.WaitGroup
		mu      sync.Mutex
		wins    int
		existed int
	)
	start := make(chan struct{})

	for i := range writers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start

			err := PutBytesIfAbsent(ctx, b, "packs/contended", []byte("shared content"))
			mu.Lock()
			defer mu.Unlock()
			switch {
			case err == nil:
				wins++
			case errors.Is(err, ErrExists):
				existed++
			default:
				t.Errorf("writer %d: %v", i, err)
			}
		}()
	}
	close(start)
	wg.Wait()

	if wins != 1 {
		t.Errorf("%d writers succeeded, want exactly 1", wins)
	}
	if existed != writers-1 {
		t.Errorf("%d writers saw ErrExists, want %d", existed, writers-1)
	}

	got, err := GetAll(ctx, b, "packs/contended")
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(got) != "shared content" {
		t.Errorf("object = %q, want %q", got, "shared content")
	}
}

// A reader that fails halfway is how a crash mid-upload looks from the
// backend's side. Nothing may appear under the real key, and no scratch
// file may be left behind.
func TestLocalFailedPutLeavesNothing(t *testing.T) {
	ctx := context.Background()
	root := filepath.Join(t.TempDir(), "repo")
	b, err := CreateLocal(root)
	if err != nil {
		t.Fatalf("create: %v", err)
	}

	boom := errors.New("disk went away")
	r := io.MultiReader(strings.NewReader("first half"), failingReader{boom})

	if err := b.Put(ctx, "packs/halfwritten", r, 100); !errors.Is(err, boom) {
		t.Fatalf("put: err = %v, want the reader error", err)
	}

	if ok, err := Exists(ctx, b, "packs/halfwritten"); err != nil || ok {
		t.Errorf("object exists after a failed put: %v, %v", ok, err)
	}
	assertNoScratchFiles(t, root)
}

type failingReader struct{ err error }

func (f failingReader) Read([]byte) (int, error) { return 0, f.err }

// A scratch file left by a crash must be invisible to every caller: it is
// not a listed object, and its name is not a key a caller could reach.
func TestLocalIgnoresLeftoverScratchFiles(t *testing.T) {
	ctx := context.Background()
	root := filepath.Join(t.TempDir(), "repo")
	b, err := CreateLocal(root)
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if err := PutBytesIfAbsent(ctx, b, "packs/real", []byte("real")); err != nil {
		t.Fatalf("put: %v", err)
	}

	// Simulate the debris of a crash between spool and link.
	if err := os.WriteFile(filepath.Join(root, "packs", ".tmp-deadbeef"), []byte("debris"), 0o600); err != nil {
		t.Fatalf("write scratch file: %v", err)
	}
	if err := os.MkdirAll(filepath.Join(root, ".cache", "nested"), 0o700); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(root, ".cache", "nested", "junk"), []byte("junk"), 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}

	var listed []string
	if err := b.List(ctx, "", func(fi FileInfo) error {
		listed = append(listed, fi.Key)
		return nil
	}); err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(listed) != 1 || listed[0] != "packs/real" {
		t.Errorf("list = %v, want [packs/real]", listed)
	}
}

func TestLocalPutCreatesNestedDirectories(t *testing.T) {
	ctx := context.Background()
	b := newTestLocal(t)

	if err := PutBytesIfAbsent(ctx, b, "snapshots/deadbeef/20260102t030405.000000000z", []byte("snap")); err != nil {
		t.Fatalf("put: %v", err)
	}
	if ok, err := Exists(ctx, b, "snapshots/deadbeef/20260102t030405.000000000z"); err != nil || !ok {
		t.Errorf("exists = %v, %v; want true, nil", ok, err)
	}
}

func TestCreateLocalRefusesNonEmptyDirectory(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "something"), nil, 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}

	if _, err := CreateLocal(dir); err == nil || !strings.Contains(err.Error(), "not empty") {
		t.Fatalf("create over a non-empty directory: err = %v, want a 'not empty' error", err)
	}
}

func TestOpenLocalRequiresADirectory(t *testing.T) {
	dir := t.TempDir()

	if _, err := OpenLocal(filepath.Join(dir, "absent")); !errors.Is(err, fs.ErrNotExist) {
		t.Errorf("open a missing directory: err = %v, want fs.ErrNotExist", err)
	}

	file := filepath.Join(dir, "afile")
	if err := os.WriteFile(file, nil, 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}
	if _, err := OpenLocal(file); err == nil || !strings.Contains(err.Error(), "not a directory") {
		t.Errorf("open a file: err = %v, want a 'not a directory' error", err)
	}
}

func TestLocalLocationIsAbsolute(t *testing.T) {
	b := newTestLocal(t)
	if !filepath.IsAbs(b.Location()) {
		t.Errorf("location = %q, want an absolute path", b.Location())
	}
}

func assertNoScratchFiles(t *testing.T, root string) {
	t.Helper()

	err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if strings.HasPrefix(d.Name(), ".tmp-") {
			t.Errorf("scratch file left behind: %s", path)
		}
		return nil
	})
	if err != nil {
		t.Fatalf("walk: %v", err)
	}
}
