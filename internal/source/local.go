package source

import (
	"context"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/at-least/kist/internal/tree"
)

// A LocalSource is a directory (or a single file) on the local
// filesystem, with the full POSIX metadata the kernel maintains: mode,
// ownership, mtime/ctime, and the inode identity that hard links and the
// posix fast path are recorded from.
//
// The metadata is captured when an item is listed. The Rust
// implementation defers each item's lstat to the moment the walker
// reaches it, because its listing contract forbids materialising a
// metadata record per entry; the Go walker has no second metadata list
// to build, so capturing at listing time says the same thing about the
// same instant and costs the same one lstat per entry.
type LocalSource struct {
	root    string
	locator []byte
}

// NewLocalSource roots a source at an absolute path. The locator is the
// path as given (cleaned), not symlink-resolved: a backup of a path
// through a symlink names the path the user asked for, which is what the
// snapshot must record and what restore must rebuild.
func NewLocalSource(root string) (*LocalSource, error) {
	abs, err := filepath.Abs(root)
	if err != nil {
		return nil, fmt.Errorf("local source %s: %w", root, err)
	}
	return &LocalSource{root: filepath.Clean(abs), locator: []byte(filepath.Clean(abs))}, nil
}

// NewLocalSourceAt roots a source at an already-normalised path and
// records locator as the root's name verbatim. The backup planner uses
// it so the locator is exactly the normalised path it would have
// recorded itself.
func NewLocalSourceAt(root, locator string) *LocalSource {
	return &LocalSource{root: root, locator: []byte(locator)}
}

// Locator is the root path as raw bytes.
func (s *LocalSource) Locator() []byte { return s.locator }

// MetaKind is posix: the kernel maintains everything a local entry
// records.
func (s *LocalSource) MetaKind() uint8 { return uint8(tree.MetaPOSIX) }

// LocalPath maps rel onto the local filesystem.
func (s *LocalSource) LocalPath(rel []byte) (string, bool) { return s.join(rel), true }

// Close releases nothing: open files belong to their readers.
func (s *LocalSource) Close() error { return nil }

// join builds the local path for a relative source path.
func (s *LocalSource) join(rel []byte) string {
	parts := strings.Split(string(rel), "/")
	kept := parts[:0]
	for _, p := range parts {
		if p != "" {
			kept = append(kept, p)
		}
	}
	return filepath.Join(append([]string{s.root}, kept...)...)
}

// CapturePosix reads the fields only the kernel's stat record has, for
// callers that hold a FileInfo outside a listing -- the backup walker's
// own item for a file root. The mode stays in Go's fs.FileMode encoding,
// which is what a tree entry stores.
func CapturePosix(info fs.FileInfo) PosixMeta { return capturePosix(info) }

// List reads one directory level. Entries are sorted by name bytes
// (os.ReadDir already returns them sorted), and an entry that disappears
// between the listing and its stat is skipped, exactly as the walker
// skips what disappears between two of its own steps.
func (s *LocalSource) List(_ context.Context, dir []byte) ([]SourceItem, error) {
	path := s.join(dir)
	entries, err := os.ReadDir(path)
	if err != nil {
		return nil, fmt.Errorf("list %s: %w", path, err)
	}

	out := make([]SourceItem, 0, len(entries))
	for _, entry := range entries {
		name := entry.Name()
		full := filepath.Join(path, name)
		info, err := entry.Info() // an lstat, despite the name
		if err != nil {
			continue // vanished mid-listing: skip
		}

		item := SourceItem{Name: []byte(name)}
		switch {
		case info.Mode()&fs.ModeSymlink != 0:
			target, err := os.Readlink(full)
			if err != nil {
				continue // vanished between the stat and the read
			}
			item.Kind = SourceItemKind{Kind: KindSymlink, Target: []byte(target)}
		case info.IsDir():
			item.Kind = SourceItemKind{Kind: KindDir}
		default:
			item.Kind = SourceItemKind{
				Kind:    KindFile,
				Size:    uint64(max(info.Size(), 0)), //nolint:gosec // a length is never negative
				MTimeNs: info.ModTime().UnixNano(),
			}
		}
		meta := capturePosix(info)
		item.Posix = &meta
		out = append(out, item)
	}
	return out, nil
}

// Read opens a file. A symlink is followed, the same way the local
// filesystem reads one.
func (s *LocalSource) Read(_ context.Context, file []byte) (io.ReadCloser, error) {
	path := s.join(file)
	f, err := os.Open(path) //nolint:gosec // path comes from walking what the user asked to back up
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	return f, nil
}
