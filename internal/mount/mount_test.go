//go:build linux || darwin

package mount

import (
	"bytes"
	"context"
	"crypto/sha256"
	"errors"
	iofs "io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/snapshot"
)

func cheapKDF() *crypto.KDFParams {
	p := crypto.DefaultKDFParams()
	p.Time, p.MemoryKiB, p.Threads = 1, 8, 1
	return &p
}

func randomBytes(seed string, n int) []byte {
	out := make([]byte, 0, n)
	h := sha256.Sum256([]byte(seed))
	for len(out) < n {
		h = sha256.Sum256(h[:])
		out = append(out, h[:]...)
	}
	return out[:n]
}

// fixture is a source tree with what a mount has to get right: a file
// spanning several chunks, an empty file, a symlink, setuid and sticky
// bits, a nested directory, a known mtime.
func fixture(t *testing.T) (string, []byte) {
	t.Helper()
	src := filepath.Join(t.TempDir(), "src")
	blob := randomBytes("blob", 6<<20)
	files := map[string][]byte{
		"readme.txt":        []byte("hello mount\n"),
		"empty.bin":         nil,
		"deep/nested/blob":  blob,
		"deep/small.bin":    randomBytes("small", 1024),
		"deep/nested/x.txt": []byte("x"),
		"setuid.bin":        []byte("pretend"),
		"sticky/keep.txt":   []byte("sticky dir"),
	}
	for name, data := range files {
		full := filepath.Join(src, filepath.FromSlash(name))
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, data, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.Chmod(filepath.Join(src, "setuid.bin"), 0o755|iofs.ModeSetuid); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(filepath.Join(src, "sticky"), 0o1777); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("readme.txt", filepath.Join(src, "link")); err != nil {
		t.Fatal(err)
	}
	when := time.Date(2020, 5, 6, 7, 8, 9, 0, time.UTC)
	if err := os.Chtimes(filepath.Join(src, "readme.txt"), when, when); err != nil {
		t.Fatal(err)
	}
	return src, blob
}

func mounted(t *testing.T) (*repo.Repository, string, string, []byte) {
	t.Helper()
	f, err := os.OpenFile("/dev/fuse", os.O_RDWR, 0)
	if err != nil {
		t.Skipf("FUSE is not usable here: %v", err)
	}
	_ = f.Close()

	ctx := context.Background()
	b, err := backend.CreateLocal(filepath.Join(t.TempDir(), "repo"))
	if err != nil {
		t.Fatal(err)
	}
	r, err := repo.Init(ctx, b, repo.Options{
		Password: []byte("mount-test"), ClientID: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
		StateDir: t.TempDir(), CacheDir: t.TempDir(), KDF: cheapKDF(),
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := r.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})

	src, blob := fixture(t)
	if _, _, err := r.Backup(ctx, []string{src}, repo.BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatal(err)
	}

	dir := filepath.Join(t.TempDir(), "mnt")
	if err := os.Mkdir(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	srv, err := Mount(ctx, r, dir, Options{Warnf: func(f string, a ...any) { t.Logf("warning: "+f, a...) }})
	if err != nil {
		t.Fatalf("mount: %v", err)
	}
	// Registered before anything can fail: a leaked mount makes the
	// temporary directory undeletable and poisons every later run.
	t.Cleanup(func() {
		if err := srv.Unmount(); err != nil {
			t.Logf("unmount: %v; trying fusermount3", err)
			if out, err := exec.Command("fusermount3", "-u", dir).CombinedOutput(); err != nil {
				t.Errorf("fusermount3 -u: %v: %s", err, out)
			}
		}
		srv.Wait()
	})
	return r, dir, src, blob
}

func TestMountServesTheSnapshot(t *testing.T) {
	r, dir, src, blob := mounted(t)
	ctx := context.Background()

	clients, err := os.ReadDir(dir)
	if err != nil || len(clients) != 1 || clients[0].Name() != r.ClientID() || !clients[0].IsDir() {
		t.Fatalf("root: %v %v", clients, err)
	}
	stamps, err := os.ReadDir(filepath.Join(dir, r.ClientID()))
	if err != nil || len(stamps) != 1 {
		t.Fatalf("client dir: %v %v", stamps, err)
	}
	handles, err := r.Snapshots(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if got, want := stamps[0].Name(), handles[0].Time.UTC().Format(snapshot.TimeFormat); got != want {
		t.Errorf("timestamp dir %q, want %q", got, want)
	}
	root := filepath.Join(dir, r.ClientID(), stamps[0].Name(), filepath.Base(src))

	// Every regular file, byte for byte, with its mode bits.
	err = filepath.WalkDir(src, func(path string, d iofs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		got := filepath.Join(root, rel)
		wantInfo, err := os.Lstat(path)
		if err != nil {
			return err
		}
		gotInfo, err := os.Lstat(got)
		if err != nil {
			t.Errorf("%s: %v", rel, err)
			return nil
		}
		if wantInfo.Mode() != gotInfo.Mode() {
			t.Errorf("%s: mode %v, want %v", rel, gotInfo.Mode(), wantInfo.Mode())
		}
		switch {
		case d.Type()&iofs.ModeSymlink != 0:
			target, err := os.Readlink(got)
			if err != nil || target != "readme.txt" {
				t.Errorf("%s: readlink %q %v", rel, target, err)
			}
		case d.Type().IsRegular():
			want, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			data, err := os.ReadFile(got)
			if err != nil {
				t.Errorf("%s: %v", rel, err)
				return nil
			}
			if !bytes.Equal(data, want) {
				t.Errorf("%s: content differs (%d vs %d bytes)", rel, len(data), len(want))
			}
			if !gotInfo.ModTime().Equal(wantInfo.ModTime()) {
				t.Errorf("%s: mtime %s, want %s", rel, gotInfo.ModTime(), wantInfo.ModTime())
			}
			if gotInfo.Size() != wantInfo.Size() {
				t.Errorf("%s: size %d, want %d", rel, gotInfo.Size(), wantInfo.Size())
			}
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}

	// Random access inside the multi-chunk blob: the end, the middle,
	// back to the start, and a read straddling what is probably a chunk
	// boundary.
	f, err := os.Open(filepath.Join(root, "deep", "nested", "blob"))
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = f.Close() }()
	for _, tc := range []struct{ off, n int }{
		{len(blob) - 1, 1}, {3 << 20, 4096}, {0, 16}, {(2 << 20) - 100, 200}, {len(blob) - 10, 100}, {len(blob), 10},
	} {
		buf := make([]byte, tc.n)
		n, err := f.ReadAt(buf, int64(tc.off))
		wantN := min(tc.n, max(len(blob)-tc.off, 0))
		if n != wantN {
			t.Errorf("ReadAt(%d, %d): n = %d (%v), want %d", tc.off, tc.n, n, err, wantN)
			continue
		}
		if !bytes.Equal(buf[:n], blob[tc.off:tc.off+n]) {
			t.Errorf("ReadAt(%d, %d): bytes differ", tc.off, tc.n)
		}
	}

	// Read-only means read-only.
	if _, err := os.OpenFile(filepath.Join(root, "readme.txt"), os.O_WRONLY, 0); !errors.Is(err, syscall.EROFS) && !errors.Is(err, iofs.ErrPermission) {
		t.Errorf("open for writing: err = %v, want EROFS", err)
	}
	if _, err := os.Stat(filepath.Join(root, "no-such-file")); !errors.Is(err, iofs.ErrNotExist) {
		t.Errorf("stat of a missing name: %v", err)
	}
	if _, err := os.Stat(filepath.Join(dir, "nobody")); !errors.Is(err, iofs.ErrNotExist) {
		t.Errorf("stat of an unknown client: %v", err)
	}
}

// A backup committed while mounted shows up, and its data reads, even
// though the mount's index predates it.
func TestMountSeesANewSnapshot(t *testing.T) {
	r, dir, _, _ := mounted(t)
	ctx := context.Background()

	other := filepath.Join(t.TempDir(), "later")
	if err := os.MkdirAll(other, 0o755); err != nil {
		t.Fatal(err)
	}
	payload := randomBytes("later", 700<<10)
	if err := os.WriteFile(filepath.Join(other, "late.bin"), payload, 0o644); err != nil {
		t.Fatal(err)
	}
	// Another process, as far as the mount is concerned: a second
	// repository handle over the same storage.
	b, err := backend.OpenLocal(r.Backend().Location())
	if err != nil {
		t.Fatal(err)
	}
	writer, err := repo.Open(ctx, b, repo.Options{
		Password: []byte("mount-test"), ClientID: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
		StateDir: t.TempDir(), CacheDir: t.TempDir(), KDF: cheapKDF(),
	})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := writer.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	}()
	_, handle, err := writer.Backup(ctx, []string{other}, repo.BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}

	path := filepath.Join(dir, handle.ClientID, handle.Time.UTC().Format(snapshot.TimeFormat), "later", "late.bin")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read a file from a snapshot committed after mount: %v", err)
	}
	if !bytes.Equal(data, payload) {
		t.Error("content differs")
	}
}

func TestFuseMode(t *testing.T) {
	cases := []struct {
		in   iofs.FileMode
		want uint32
	}{
		{0o644, syscall.S_IFREG | 0o644},
		{0o755 | iofs.ModeSetuid, syscall.S_IFREG | syscall.S_ISUID | 0o755},
		{0o2755 | iofs.ModeSetgid, syscall.S_IFREG | syscall.S_ISGID | 0o755},
		{iofs.ModeDir | iofs.ModeSticky | 0o1777, syscall.S_IFDIR | syscall.S_ISVTX | 0o777},
		{iofs.ModeSymlink | 0o777, syscall.S_IFLNK | 0o777},
	}
	for _, tc := range cases {
		if got := fuseMode(tc.in); got != tc.want {
			t.Errorf("fuseMode(%v) = %#o, want %#o", tc.in, got, tc.want)
		}
	}
}

func TestLRUEvictsTheOldest(t *testing.T) {
	c := newLRU(2)
	c.put(crypto.ID{1}, []byte("1"))
	c.put(crypto.ID{2}, []byte("2"))
	c.get(crypto.ID{1}) // 1 is now the most recent
	c.put(crypto.ID{3}, []byte("3"))
	if _, ok := c.get(crypto.ID{2}); ok {
		t.Error("2 should have been evicted")
	}
	if _, ok := c.get(crypto.ID{1}); !ok {
		t.Error("1 should have survived")
	}
}
