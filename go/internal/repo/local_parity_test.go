package repo

import (
	"bytes"
	"context"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"testing"
	"time"

	"github.com/at-least/kist/internal/tree"
)

// TestLocalWalkerEntryFields pins the exact field matrix the local
// (posix) walker records per entry, so a walker refactor cannot silently
// drift: which metadata pointers are present, their values where the
// platform lets the test pin them, and the chunk layout. It was run
// green against the pre-source walker and must stay green after it.
func TestLocalWalkerEntryFields(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "walker-parity")

	source := t.TempDir()
	writeTree(t, source, []fileSpec{
		{path: "readme.txt", data: []byte("hello kist\n")},
		{path: "empty.bin", data: nil},
		{path: "exec.sh", data: []byte("#!/bin/sh\n"), mode: 0o755},
		{path: "setuid.bin", data: []byte("s"), mode: 0o755 | fs.ModeSetuid},
		{path: "docs/notes.md", data: bytes.Repeat([]byte("compressible. "), 4096)},
		{path: "docs/deep/blob.bin", data: randomBytes(t, "parity-blob", 3<<20)},
		{path: "hard-a", data: []byte("one inode, two names")},
	})

	// Pin the modification times AFTER writeTree's chmod: chmod updates
	// ctime (which this test does not pin) but not mtime, and a pinned
	// second-precision mtime makes the assertion independent of when the
	// fixture was written.
	pinned := time.Date(2025, 6, 1, 12, 0, 0, 0, time.UTC)
	err := filepath.Walk(source, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		// Directories too: a walker that drops a directory's mtime
		// changes every parent tree's bytes.
		return os.Chtimes(path, pinned, pinned)
	})
	if err != nil {
		t.Fatalf("pin mtimes: %v", err)
	}

	// A hard-linked pair: both names must record dev/ino/nlink.
	if err := os.Link(filepath.Join(source, "hard-a"), filepath.Join(source, "hard-b")); err != nil {
		t.Fatalf("link: %v", err)
	}
	// A symlink with a relative target.
	if err := os.Symlink("readme.txt", filepath.Join(source, "link")); err != nil {
		t.Fatalf("symlink: %v", err)
	}

	summary, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if len(summary.Snapshot.Roots) != 1 {
		t.Fatalf("roots = %d, want 1", len(summary.Snapshot.Roots))
	}

	// want is the field matrix per slash-relative path. ctime, uid, gid,
	// dev and ino are real values the test cannot pin; presence (and the
	// pointer-not-zero rules) is what is asserted for them.
	type fields struct {
		typ     tree.NodeType
		size    uint64
		sizeSet bool
		mode    uint32 // 0 = not asserted (dirs, symlinks)
		mtimeNs int64  // 0 = not asserted (dirs)
		chunks  int    // -1 = do not care
		target  string
		links   uint64 // 0 = not asserted
	}
	pinnedNs := pinned.UnixNano()
	pin := func(typ tree.NodeType, size uint64, mode uint32, chunks int) fields {
		return fields{typ: typ, size: size, sizeSet: true, mode: mode, mtimeNs: pinnedNs, chunks: chunks}
	}
	wantEntries := map[string]fields{
		"readme.txt":         pin(tree.TypeFile, 11, uint32(0o644), 1),
		"empty.bin":          pin(tree.TypeFile, 0, uint32(0o644), 0),
		"exec.sh":            pin(tree.TypeFile, 10, uint32(0o755), 1),
		"setuid.bin":         pin(tree.TypeFile, 1, uint32(0o755|fs.ModeSetuid), 1),
		"docs/notes.md":      pin(tree.TypeFile, 57344, uint32(0o644), -1),
		"docs/deep/blob.bin": pin(tree.TypeFile, 3<<20, uint32(0o644), -1),
		"hard-a":             pin(tree.TypeFile, 20, uint32(0o644), 1),
		"hard-b":             pin(tree.TypeFile, 20, uint32(0o644), 1),
		"link":               {typ: tree.TypeSymlink, target: "readme.txt"},
		"docs":               {typ: tree.TypeDir, mtimeNs: pinnedNs},
		"docs/deep":          {typ: tree.TypeDir, mtimeNs: pinnedNs},
	}
	// The hard-linked pair needs the links field on top of the pinned file
	// fields; os.Chtimes cannot pin a symlink's own mtime, so the link
	// asserts none.
	wantEntries["hard-a"] = func(f fields) fields { f.links = 2; return f }(wantEntries["hard-a"])
	wantEntries["hard-b"] = func(f fields) fields { f.links = 2; return f }(wantEntries["hard-b"])

	seen := map[string]tree.Entry{}
	for _, root := range summary.Snapshot.Roots {
		entries, err := r.LoadTreeChain(ctx, root.Tree)
		if err != nil {
			t.Fatalf("load root tree: %v", err)
		}
		if err := collectEntries(ctx, r, entries, string(root.Path), seen); err != nil {
			t.Fatal(err)
		}
	}

	base := rootPathPrefix(source)
	got := make([]string, 0, len(seen))
	for path := range seen {
		got = append(got, path)
	}
	slices.Sort(got)
	wantPaths := make([]string, 0, len(wantEntries))
	for path := range wantEntries {
		wantPaths = append(wantPaths, base+"/"+path)
	}
	slices.Sort(wantPaths)
	if !slices.Equal(got, wantPaths) {
		t.Fatalf("backed-up paths = %v, want %v", got, wantPaths)
	}

	for rel, want := range wantEntries {
		e, ok := seen[base+"/"+rel]
		if !ok {
			t.Errorf("%s: not backed up", rel)
			continue
		}
		if tree.NodeType(e.Type) != want.typ {
			t.Errorf("%s: type = %v, want %v", rel, tree.NodeType(e.Type), want.typ)
		}
		if want.sizeSet && e.Size != want.size {
			t.Errorf("%s: size = %d, want %d", rel, e.Size, want.size)
		}
		if want.mode != 0 && (e.Mode == nil || *e.Mode != want.mode) {
			t.Errorf("%s: mode = %v, want %d", rel, derefPtr32(e.Mode), want.mode)
		}
		if want.mtimeNs != 0 && (e.MTimeNs == nil || *e.MTimeNs != want.mtimeNs) {
			t.Errorf("%s: mtime = %v, want %d", rel, derefPtr64i(e.MTimeNs), want.mtimeNs)
		}
		if want.typ == tree.TypeFile && want.chunks >= 0 && len(e.Chunks) != want.chunks {
			t.Errorf("%s: %d chunks, want %d", rel, len(e.Chunks), want.chunks)
		}
		if want.typ == tree.TypeSymlink {
			if string(e.Target) != want.target {
				t.Errorf("%s: target = %q, want %q", rel, e.Target, want.target)
			}
		}
		if want.typ != tree.TypeDir && e.CTimeNs == nil {
			t.Errorf("%s: ctime absent, want recorded (this platform has one)", rel)
		}
		// A symlink's own mtime cannot be pinned portably (utimensat
		// follows the link), but it must be present and real: the old
		// walker recorded it, and a zero would send every restore's
		// directory times to the epoch.
		if want.typ == tree.TypeSymlink && (e.MTimeNs == nil || *e.MTimeNs == 0) {
			t.Errorf("%s: mtime = %v, want the link's own time", rel, e.MTimeNs)
		}
		if e.UID == nil || e.GID == nil {
			t.Errorf("%s: uid/gid absent, want recorded", rel)
		}
		if want.links != 0 {
			if e.Links == nil || *e.Links != want.links {
				t.Errorf("%s: nlink = %v, want %d", rel, e.Links, want.links)
			}
			if e.Device == nil || e.Inode == nil {
				t.Errorf("%s: dev/ino absent, want recorded for a hard link", rel)
			}
		} else if want.typ == tree.TypeFile && (e.Device != nil || e.Inode != nil || e.Links != nil) {
			t.Errorf("%s: dev/ino/nlink present for a single-link file: %v %v %v", rel, e.Device, e.Inode, e.Links)
		}
		if tree.MetaKind(e.MetaKind) != tree.MetaPOSIX {
			t.Errorf("%s: meta kind = %d, want posix", rel, e.MetaKind)
		}
		if len(e.Etag) != 0 || len(e.Vern) != 0 {
			t.Errorf("%s: etag/vern present on a posix entry", rel)
		}
	}

	// The hard-linked pair shares one chunk list.
	if !slices.Equal(seen[base+"/hard-a"].Chunks, seen[base+"/hard-b"].Chunks) {
		t.Errorf("hard-linked pair does not share its chunk list")
	}
}

// rootPathPrefix returns the source path as the snapshot records it. The
// test runs the backup on a path that needs no normalisation beyond
// cleaning, so this is stable.
func rootPathPrefix(source string) string {
	if p, err := filepath.Abs(source); err == nil {
		return p
	}
	return source
}

// collectEntries flattens a directory subtree into seen, keyed by
// slash-separated paths under the root locator.
func collectEntries(ctx context.Context, r *Repository, entries []tree.Entry, prefix string, seen map[string]tree.Entry) error {
	for _, e := range entries {
		path := prefix + "/" + string(e.Name)
		seen[path] = e
		if tree.NodeType(e.Type) == tree.TypeDir {
			children, err := r.LoadTreeChain(ctx, *e.Subtree)
			if err != nil {
				return err
			}
			if err := collectEntries(ctx, r, children, path, seen); err != nil {
				return err
			}
		}
	}
	return nil
}

func derefPtr32(p *uint32) uint32 {
	if p == nil {
		return 0
	}
	return *p
}

func derefPtr64i(p *int64) int64 {
	if p == nil {
		return 0
	}
	return *p
}
