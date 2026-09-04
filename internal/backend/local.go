package backend

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// Local stores objects as files under a directory, one file per key.
//
// Every visible name is written by linking or renaming a fully written,
// fsynced scratch file into place, so a crash can leave a stray
// ".tmp-<random>" file but never a truncated object under a real key.
// Scratch files are written in the destination directory, which keeps the
// link within one filesystem, and are hidden from List by the rule that
// no key segment may start with a dot.
type Local struct {
	root string
}

// Local implements Backend.
var _ Backend = (*Local)(nil)

// OpenLocal returns a backend over an existing directory.
func OpenLocal(root string) (*Local, error) {
	abs, err := filepath.Abs(root)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", root, err)
	}
	info, err := os.Stat(abs)
	if err != nil {
		return nil, fmt.Errorf("open local backend at %s: %w", abs, err)
	}
	if !info.IsDir() {
		return nil, fmt.Errorf("open local backend at %s: not a directory", abs)
	}
	return &Local{root: abs}, nil
}

// CreateLocal creates the directory, along with any parents, and returns a
// backend over it. An existing directory is accepted; an existing
// non-empty one is not, because writing a second repository over the top
// of a first is never what the caller meant.
func CreateLocal(root string) (*Local, error) {
	abs, err := filepath.Abs(root)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", root, err)
	}

	switch entries, err := os.ReadDir(abs); {
	case err == nil && len(entries) > 0:
		return nil, fmt.Errorf("create local backend at %s: directory is not empty", abs)
	case err != nil && !errors.Is(err, fs.ErrNotExist):
		return nil, fmt.Errorf("create local backend at %s: %w", abs, err)
	}

	if err := os.MkdirAll(abs, 0o700); err != nil {
		return nil, fmt.Errorf("create local backend at %s: %w", abs, err)
	}
	return &Local{root: abs}, nil
}

// Location reports the root directory.
func (l *Local) Location() string { return l.root }

// Close releases nothing: a local backend holds no handle between calls.
func (l *Local) Close() error { return nil }

func (l *Local) path(key string) (string, error) {
	if err := ValidateKey(key); err != nil {
		return "", err
	}
	return filepath.Join(l.root, filepath.FromSlash(key)), nil
}

// Get opens a ranged reader over the object.
func (l *Local) Get(_ context.Context, key string, off, length int64) (io.ReadCloser, error) {
	path, err := l.path(key)
	if err != nil {
		return nil, err
	}
	if off < 0 {
		return nil, fmt.Errorf("get %s: offset %d is negative", key, off)
	}

	f, err := os.Open(path) //nolint:gosec // path is built from a validated key under the backend root
	if err != nil {
		return nil, l.wrap("get", key, err)
	}

	if off > 0 {
		if _, err := f.Seek(off, io.SeekStart); err != nil {
			_ = f.Close()
			return nil, fmt.Errorf("get %s: seek to %d: %w", key, off, err)
		}
	}
	if length == ReadToEnd {
		return f, nil
	}
	if length < 0 {
		_ = f.Close()
		return nil, fmt.Errorf("get %s: length %d is negative", key, length)
	}
	return sectionReader{Reader: io.LimitReader(f, length), closer: f}, nil
}

type sectionReader struct {
	io.Reader
	closer io.Closer
}

func (s sectionReader) Close() error { return s.closer.Close() }

// Put writes the object, replacing whatever was there. size is advisory:
// it is checked against what was actually read so that a short reader
// cannot quietly produce a truncated object.
func (l *Local) Put(_ context.Context, key string, r io.Reader, size int64) error {
	path, err := l.path(key)
	if err != nil {
		return err
	}

	tmp, written, err := l.spool(filepath.Dir(path), r)
	if err != nil {
		return fmt.Errorf("put %s: %w", key, err)
	}
	defer func() { _ = os.Remove(tmp) }()

	if size >= 0 && written != size {
		return fmt.Errorf("put %s: read %d bytes, expected %d", key, written, size)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("put %s: %w", key, err)
	}
	if err := syncDir(filepath.Dir(path)); err != nil {
		return fmt.Errorf("put %s: %w", key, err)
	}
	return nil
}

// PutIfAbsent stores data unless the key is taken, in which case it
// returns ErrExists.
//
// The conditional step is os.Link, not an O_EXCL open of the final name.
// Link fails atomically when the destination exists on POSIX and on NTFS,
// while O_EXCL would create the real name first and fill it afterwards --
// leaving a window, and after a crash a permanent truncated object under
// a name that is supposed to be immutable.
func (l *Local) PutIfAbsent(_ context.Context, key string, r io.Reader, size int64) error {
	path, err := l.path(key)
	if err != nil {
		return err
	}

	tmp, written, err := l.spool(filepath.Dir(path), r)
	if err != nil {
		return fmt.Errorf("put %s: %w", key, err)
	}
	defer func() { _ = os.Remove(tmp) }()

	if size >= 0 && written != size {
		return fmt.Errorf("put %s: read %d bytes, expected %d", key, written, size)
	}

	switch err := os.Link(tmp, path); {
	case err == nil:
	case errors.Is(err, fs.ErrExist):
		return fmt.Errorf("put %s: %w", key, ErrExists)
	default:
		return fmt.Errorf("put %s: %w", key, err)
	}

	if err := syncDir(filepath.Dir(path)); err != nil {
		return fmt.Errorf("put %s: %w", key, err)
	}
	return nil
}

// spool writes r into a fully synced scratch file in dir and returns its
// path. The caller renames or links it into place; until then nothing is
// visible under a real key.
func (l *Local) spool(dir string, r io.Reader) (_ string, _ int64, err error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", 0, fmt.Errorf("create directory %s: %w", dir, err)
	}

	var suffix [8]byte
	if _, err := rand.Read(suffix[:]); err != nil {
		return "", 0, fmt.Errorf("name scratch file: %w", err)
	}
	// Held in its own variable, not a named return: the error paths below
	// return "" for the path, and the cleanup still has to know which file
	// to remove.
	scratch := filepath.Join(dir, ".tmp-"+hex.EncodeToString(suffix[:]))

	f, err := os.OpenFile(scratch, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600) //nolint:gosec // path is under the backend root
	if err != nil {
		return "", 0, fmt.Errorf("create scratch file: %w", err)
	}
	defer func() {
		if err != nil {
			_ = f.Close()
			_ = os.Remove(scratch)
		}
	}()

	written, err := io.Copy(f, r)
	if err != nil {
		return "", 0, fmt.Errorf("write scratch file: %w", err)
	}
	if err = f.Sync(); err != nil {
		return "", 0, fmt.Errorf("sync scratch file: %w", err)
	}
	if err = f.Close(); err != nil {
		return "", 0, fmt.Errorf("close scratch file: %w", err)
	}
	return scratch, written, nil
}

// List walks every object whose key starts with prefix.
func (l *Local) List(ctx context.Context, prefix string, fn func(FileInfo) error) error {
	if err := ValidatePrefix(prefix); err != nil {
		return err
	}

	// Descend from the deepest directory the prefix names, so that
	// listing packs/ does not walk the whole repository.
	base, _ := filepath.Split(prefix)
	root := filepath.Join(l.root, filepath.FromSlash(base))

	err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		switch {
		case errors.Is(err, fs.ErrNotExist) && path == root:
			return fs.SkipAll // nothing stored under this prefix yet
		case err != nil:
			return err
		case ctx.Err() != nil:
			return ctx.Err()
		}

		name := d.Name()
		if strings.HasPrefix(name, ".") {
			if d.IsDir() {
				return fs.SkipDir
			}
			return nil // scratch file from an in-flight Put
		}
		if d.IsDir() {
			return nil
		}

		rel, err := filepath.Rel(l.root, path)
		if err != nil {
			return fmt.Errorf("relative path of %s: %w", path, err)
		}
		key := filepath.ToSlash(rel)
		if !strings.HasPrefix(key, prefix) {
			return nil
		}

		info, err := d.Info()
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				return nil // deleted while we walked
			}
			return fmt.Errorf("stat %s: %w", key, err)
		}
		return fn(FileInfo{Key: key, Size: info.Size()})
	})
	if err != nil {
		return fmt.Errorf("list %q in %s: %w", prefix, l.root, err)
	}
	return nil
}

// Stat reports on one object.
func (l *Local) Stat(_ context.Context, key string) (FileInfo, error) {
	path, err := l.path(key)
	if err != nil {
		return FileInfo{}, err
	}

	info, err := os.Stat(path)
	if err != nil {
		return FileInfo{}, l.wrap("stat", key, err)
	}
	if info.IsDir() {
		return FileInfo{}, fmt.Errorf("stat %s: %w", key, ErrNotFound)
	}
	return FileInfo{Key: key, Size: info.Size()}, nil
}

// Delete removes an object, treating an absent object as already deleted.
func (l *Local) Delete(_ context.Context, key string) error {
	path, err := l.path(key)
	if err != nil {
		return err
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("delete %s: %w", key, err)
	}
	return nil
}

func (l *Local) wrap(op, key string, err error) error {
	if errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("%s %s: %w", op, key, ErrNotFound)
	}
	return fmt.Errorf("%s %s: %w", op, key, err)
}
