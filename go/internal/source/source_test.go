package source

import (
	"bytes"
	"context"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"testing"

	"github.com/at-least/kist/internal/tree"
)

func testDir(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "zsub"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "a.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "m.bin"), make([]byte, 10), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

func TestLocalSourceListsSorted(t *testing.T) {
	dir := testDir(t)
	src, err := NewLocalSource(dir)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.HasPrefix(src.Locator(), []byte("/")) {
		t.Errorf("locator %q is not absolute", src.Locator())
	}
	if got := src.MetaKind(); got != uint8(tree.MetaPOSIX) {
		t.Errorf("meta kind = %d, want posix", got)
	}

	items, err := src.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	var file *SourceItem
	for i := range items {
		names = append(names, string(items[i].Name))
		if string(items[i].Name) == "a.txt" {
			file = &items[i]
		}
	}
	if !slices.Equal(names, []string{"a.txt", "m.bin", "zsub"}) {
		t.Errorf("names = %s, want sorted a.txt m.bin zsub", names)
	}

	if file == nil || file.Kind.Kind != KindFile {
		t.Fatalf("a.txt listed as %v, want a file", file)
	}
	if file.Kind.Size != 5 {
		t.Errorf("size = %d, want 5", file.Kind.Size)
	}
	if file.Kind.MTimeNs == 0 {
		t.Error("mtime not recorded")
	}
	if file.Posix == nil {
		t.Fatal("local items carry posix metadata")
	}
	if !fs.FileMode(file.Posix.Mode).IsRegular() {
		t.Errorf("mode = %o, want a regular file", file.Posix.Mode)
	}

	rc, err := src.Read(context.Background(), []byte("a.txt"))
	if err != nil {
		t.Fatal(err)
	}
	defer rc.Close()
	content, err := io.ReadAll(rc)
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "hello" {
		t.Errorf("read = %q, want hello", content)
	}
	if err := src.Close(); err != nil {
		t.Error(err)
	}
}

func TestLocalSourceListsSubdirectory(t *testing.T) {
	dir := testDir(t)
	if err := os.WriteFile(filepath.Join(dir, "zsub", "inner.txt"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	src, err := NewLocalSource(dir)
	if err != nil {
		t.Fatal(err)
	}
	items, err := src.List(context.Background(), []byte("zsub"))
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 1 || string(items[0].Name) != "inner.txt" {
		t.Errorf("subdirectory listing = %v, want inner.txt", items)
	}
}

func TestLocalSourceReportsSymlinksWithTargets(t *testing.T) {
	dir := t.TempDir()
	if err := os.Symlink("target.txt", filepath.Join(dir, "link")); err != nil {
		t.Fatal(err)
	}
	src, err := NewLocalSource(dir)
	if err != nil {
		t.Fatal(err)
	}
	items, err := src.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 1 || string(items[0].Name) != "link" {
		t.Fatalf("listing = %v, want link", items)
	}
	if items[0].Kind.Kind != KindSymlink {
		t.Fatalf("kind = %v, want symlink", items[0].Kind.Kind)
	}
	if string(items[0].Kind.Target) != "target.txt" {
		t.Errorf("target = %q, want target.txt", items[0].Kind.Target)
	}
}

func TestLocalSourceMapsLocalPaths(t *testing.T) {
	dir := testDir(t)
	src, err := NewLocalSource(dir)
	if err != nil {
		t.Fatal(err)
	}
	path, ok := LocalPathOf(src, []byte("zsub/inner.txt"))
	if !ok {
		t.Fatal("the local source maps local paths")
	}
	if path != filepath.Join(dir, "zsub", "inner.txt") {
		t.Errorf("local path = %s, want %s", path, filepath.Join(dir, "zsub", "inner.txt"))
	}
}

func TestMemorySourceListsOneLevelSorted(t *testing.T) {
	m := NewMemorySource("s3://bucket/prefix", uint8(tree.MetaS3))
	m.AddDir("dir")
	m.AddFile("z.txt", []byte("zz"), 2)
	m.AddFile("dir/a.txt", []byte("a"), 1)

	items, err := m.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 2 {
		t.Fatalf("got %d items, want 2: %v", len(items), items)
	}
	if string(items[0].Name) != "dir" || items[0].Kind.Kind != KindDir {
		t.Errorf("first item = %v %v, want dir", items[0].Name, items[0].Kind.Kind)
	}
	if string(items[1].Name) != "z.txt" || items[1].Kind.Kind != KindFile {
		t.Errorf("second item = %v %v, want z.txt file", items[1].Name, items[1].Kind.Kind)
	}
	if items[1].Kind.Size != 2 || items[1].Kind.MTimeNs != 2 {
		t.Errorf("file facts = %+v, want size 2 mtime 2", items[1].Kind)
	}
	if len(items[1].Kind.Etag) == 0 {
		t.Error("an s3-kind source derives an etag")
	}
	if items[1].Kind.Etag != nil && !bytes.HasPrefix(items[1].Kind.Etag, []byte(`"`)) {
		t.Errorf("etag %q is not the quoted-digest shape", items[1].Kind.Etag)
	}

	// Overwrite deliberately leaves the etag as it was -- the test, not
	// the source, decides whether the storage vouches for the new bytes
	// -- and SetModified moves only the time.
	old := slices.Clone(items[1].Kind.Etag)
	m.Overwrite("z.txt", []byte("different"), 3)
	items, err = m.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(items[1].Kind.Etag, old) {
		t.Error("overwrite moved the etag; only the test may")
	}
	if items[1].Kind.MTimeNs != 3 {
		t.Errorf("mtime = %d, want 3", items[1].Kind.MTimeNs)
	}
	// A fresh AddFile recomputes the etag from the new contents.
	m.AddFile("z.txt", []byte("rewritten"), 4)
	items, err = m.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Equal(items[1].Kind.Etag, old) {
		t.Error("rewriting the file kept its derived etag")
	}
	m.SetModified("z.txt", 9)
	m.SetEtag("z.txt", []byte(`"pinned"`))
	items, err = m.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if items[1].Kind.MTimeNs != 9 || string(items[1].Kind.Etag) != `"pinned"` {
		t.Errorf("after pinning: %+v", items[1].Kind)
	}
}

func TestMemorySourceSftpKindHasNoEtag(t *testing.T) {
	m := NewMemorySource("sftp://host/path", uint8(tree.MetaSFTP))
	m.AddFile("f", []byte("data"), 5)
	items, err := m.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 1 || string(items[0].Name) != "f" {
		t.Fatalf("listing = %v, want f", items)
	}
	if len(items[0].Kind.Etag) != 0 {
		t.Error("an sftp-kind source must not invent an etag")
	}
	if got := m.MetaKind(); got != uint8(tree.MetaSFTP) {
		t.Errorf("meta kind = %d, want sftp", got)
	}
}

func TestMemorySourceListsAFileItself(t *testing.T) {
	// The file-root contract: listing a path that names a file lists the
	// file itself, which is how the walker detects a single-file source.
	m := NewMemorySource("s3://bucket/data", uint8(tree.MetaS3))
	m.AddFile("data", []byte("one file"), 7)
	items, err := m.List(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 1 || string(items[0].Name) != "data" || items[0].Kind.Kind != KindFile {
		t.Fatalf("listing = %v, want the file data itself", items)
	}

	rc, err := m.Read(context.Background(), []byte("data"))
	if err != nil {
		t.Fatal(err)
	}
	defer rc.Close()
	content, err := io.ReadAll(rc)
	if err != nil {
		t.Fatal(err)
	}
	if string(content) != "one file" {
		t.Errorf("read = %q", content)
	}

	if _, err := m.Read(context.Background(), []byte("missing")); err == nil {
		t.Error("reading a missing file succeeded")
	}
}

func TestOpenSourceRejectsUnknownSchemes(t *testing.T) {
	_, err := OpenSource(context.Background(), "gopher://x")
	if err == nil {
		t.Fatal("an unknown scheme was accepted")
	}
	if !bytes.Contains([]byte(err.Error()), []byte("unsupported source scheme")) {
		t.Errorf("error = %v, want the unsupported-scheme message", err)
	}
}
