package source

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"slices"
	"strings"
	"sync"

	"github.com/at-least/kist/internal/tree"
)

// A MemorySource is an in-memory Source: an s3-shaped or sftp-shaped
// filesystem for tests and in-process use, with controllable contents,
// modification times and etags.
//
// Its paths are relative to the locator's own path part -- locator
// "s3://bucket/prefix" serves "readme.txt" from the stored key
// "prefix/readme.txt", exactly what a real object-store source does.
//
// Listing follows the same one-level shape as the remote sources, and a
// path that names a file lists that file itself, so a single-file root
// triggers the walker's file-root rule exactly as it would against real
// object storage.
type MemorySource struct {
	locator  []byte
	root     string // the locator's path part: the prefix every path hangs under
	metaKind uint8

	mu      sync.Mutex
	entries map[string]*memEntry
}

type memEntry struct {
	dir     bool
	data    []byte
	mtimeNs int64
	etag    []byte
	vern    []byte
}

// NewMemorySource builds an empty source whose root records locator and
// whose entries carry metaKind's metadata family -- tree.MetaS3 for an
// etag-bearing source, tree.MetaSFTP for a time-only one, tree.MetaGeneric
// for the conservative kind.
func NewMemorySource(locator string, metaKind uint8) *MemorySource {
	// The locator's path part is the prefix everything is stored under,
	// mirroring how the s3 and sftp sources root themselves.
	root := locator
	if i := strings.Index(locator, "://"); i >= 0 {
		root = locator[i+3:]
	}
	if i := strings.IndexByte(root, '/'); i >= 0 {
		root = root[i+1:]
	} else {
		root = ""
	}
	return &MemorySource{
		locator:  []byte(locator),
		root:     strings.Trim(root, "/"),
		metaKind: metaKind,
		entries:  make(map[string]*memEntry),
	}
}

// Locator is the locator the source was built with.
func (m *MemorySource) Locator() []byte { return m.locator }

// MetaKind is the kind the source was built with.
func (m *MemorySource) MetaKind() uint8 { return m.metaKind }

// Close releases nothing.
func (m *MemorySource) Close() error { return nil }

func (m *MemorySource) clean(path string) string {
	path = strings.Trim(path, "/")
	if m.root == "" {
		return path
	}
	if path == "" {
		return m.root
	}
	return m.root + "/" + path
}

// AddDir creates a directory along with any parents.
func (m *MemorySource) AddDir(path string) {
	path = m.clean(path)
	if path == "" {
		return // the root exists implicitly
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	for {
		if _, ok := m.entries[path]; !ok {
			m.entries[path] = &memEntry{dir: true}
		}
		if i := strings.LastIndexByte(path, '/'); i >= 0 {
			path = path[:i]
		} else {
			return
		}
	}
}

// AddFile stores a file's contents. An s3-kind source derives an etag
// from the contents, the way object storage derives one; other kinds
// record none. A file's parent directories need not exist.
func (m *MemorySource) AddFile(path string, data []byte, mtimeNs int64) {
	path = m.clean(path)
	etag := m.deriveEtag(data)
	m.mu.Lock()
	defer m.mu.Unlock()
	m.entries[path] = &memEntry{data: data, mtimeNs: mtimeNs, etag: etag}
}

// deriveEtag is the fake object-store fingerprint: a quoted hex digest,
// like a real ETag, recomputed on every write.
func (m *MemorySource) deriveEtag(data []byte) []byte {
	if m.metaKind != uint8(tree.MetaS3) {
		return nil
	}
	sum := sha256.Sum256(data)
	return []byte(`"` + hex.EncodeToString(sum[:8]) + `"`)
}

// Overwrite replaces a file's contents and modification time. The etag
// is left as it was, unlike a real store, so a test can decide
// separately whether the source claims the new contents are the old
// ones.
func (m *MemorySource) Overwrite(path string, data []byte, mtimeNs int64) {
	path = m.clean(path)
	m.mu.Lock()
	defer m.mu.Unlock()
	e, ok := m.entries[path]
	if !ok {
		m.entries[path] = &memEntry{data: data, mtimeNs: mtimeNs}
		return
	}
	e.data = data
	e.mtimeNs = mtimeNs
}

// SetModified moves a file's modification time without touching its
// contents or etag: the "renamed but not changed" case.
func (m *MemorySource) SetModified(path string, mtimeNs int64) {
	path = m.clean(path)
	m.mu.Lock()
	defer m.mu.Unlock()
	if e, ok := m.entries[path]; ok {
		e.mtimeNs = mtimeNs
	}
}

// SetEtag replaces the etag a file reports, overriding the derived one.
func (m *MemorySource) SetEtag(path string, etag []byte) {
	path = m.clean(path)
	m.mu.Lock()
	defer m.mu.Unlock()
	if e, ok := m.entries[path]; ok {
		e.etag = etag
	}
}

// List returns one directory level. A path that names a file lists that
// file itself -- the contract the walker's file-root detection relies
// on; a directory lists its children, subdirectories as bare names.
func (m *MemorySource) List(_ context.Context, dir []byte) ([]SourceItem, error) {
	base := m.clean(string(dir))
	name := lastComponent(base)
	if m.root != "" && base == m.root {
		name = lastComponent(m.root)
	}

	m.mu.Lock()
	defer m.mu.Unlock()

	if e, ok := m.entries[base]; ok && base != "" && !e.dir {
		return []SourceItem{fileItem(name, e)}, nil
	}

	prefix := ""
	if base != "" {
		prefix = base + "/"
	}

	byName := make(map[string]SourceItem)
	for path, e := range m.entries {
		if !strings.HasPrefix(path, prefix) || path == base {
			continue
		}
		rest := path[len(prefix):]
		name, isDir := rest, false
		if i := strings.IndexByte(rest, '/'); i >= 0 {
			name, isDir = rest[:i], true
			if name == "" {
				continue
			}
		}
		if _, dup := byName[name]; dup && !isDir {
			continue // a dir entry and a file cannot share a name; dirs win
		}
		if isDir {
			if e, also := m.entries[prefix+name]; also && !e.dir {
				continue // the file wins over a directory marker
			}
			byName[name] = SourceItem{Name: []byte(name), Kind: SourceItemKind{Kind: KindDir}}
			continue
		}
		byName[name] = fileItem(name, e)
	}

	out := make([]SourceItem, 0, len(byName))
	for _, item := range byName {
		out = append(out, item)
	}
	slices.SortFunc(out, func(a, b SourceItem) int { return bytes.Compare(a.Name, b.Name) })
	return out, nil
}

// fileItem builds a listed file from a stored entry, copying the bytes
// that outlive the call.
func fileItem(name string, e *memEntry) SourceItem {
	return makeFileItem(name, uint64(len(e.data)), e.mtimeNs, e.etag, e.vern) //nolint:gosec // a length is never negative
}

// Read opens a file's contents. Each call gets an independent reader, so
// a file may be read (and re-read) as often as the walk needs.
func (m *MemorySource) Read(_ context.Context, file []byte) (io.ReadCloser, error) {
	path := m.clean(string(file))
	m.mu.Lock()
	e, ok := m.entries[path]
	var data []byte
	if ok && !e.dir {
		data = e.data
	}
	m.mu.Unlock()
	if !ok {
		return nil, fmt.Errorf("read %s: not found", path)
	}
	if e.dir {
		return nil, fmt.Errorf("read %s: is a directory", path)
	}
	return io.NopCloser(bytes.NewReader(data)), nil
}
