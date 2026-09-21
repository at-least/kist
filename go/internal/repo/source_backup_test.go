package repo

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"slices"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/source"
	"github.com/at-least/kist/internal/tree"
)

// The remote-SOURCE tests use MemorySource: an s3-shaped or sftp-shaped
// filesystem with controllable contents, times and etags. The walker
// under test is the same one local paths go through; only the source
// differs, which is the point of the source abstraction.

// initRealtimeRepo is initRepo with the real clock: the posix fast
// path's racy guard compares file timestamps against the parent
// snapshot's start, which the injected test clocks deliberately place in
// the past -- there, every recently written file is "racily clean" and
// correctly re-read. These tests need the ordering real filesystems and
// real clocks produce.
func initRealtimeRepo(t *testing.T, seed string) (*Repository, string) {
	t.Helper()

	dir := filepath.Join(t.TempDir(), "repo")
	b, err := backend.CreateLocal(dir)
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}
	r, err := Init(context.Background(), b, Options{
		Password:    []byte(testPassword),
		Replicas:    ptrUint8(0),
		ClientID:    "00112233445566778899aabbccddeeff",
		StateDir:    t.TempDir(),
		CacheDir:    t.TempDir(),
		KDF:         cheapKDF(),
		NonceSource: crypto.DeterministicReader(seed),
	})
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

// initRepoWithChunker is initRealtimeRepo with explicit chunk sizes, so
// a test-sized file can cross the indirect-storage threshold.
func initRepoWithChunker(t *testing.T, seed string, sizes ChunkerParams) (*Repository, string) {
	t.Helper()

	dir := filepath.Join(t.TempDir(), "repo")
	b, err := backend.CreateLocal(dir)
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}
	r, err := Init(context.Background(), b, Options{
		Password:    []byte(testPassword),
		Replicas:    ptrUint8(0),
		ClientID:    "00112233445566778899aabbccddeeff",
		StateDir:    t.TempDir(),
		CacheDir:    t.TempDir(),
		KDF:         cheapKDF(),
		NonceSource: crypto.DeterministicReader(seed),
		Chunker:     &sizes,
	})
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

func memoryTree(t *testing.T, kind uint8, locator string) *MemorySourceView {
	t.Helper()
	m := source.NewMemorySource(locator, kind)
	m.AddFile("readme.txt", []byte("hello kist\n"), 1_000_000_000)
	m.AddFile("dir/note.md", bytes.Repeat([]byte("compressible text. "), 20000), 2_000_000_000) //nolint:unconvert // clearer than a variable for one use
	m.AddFile("blob.bin", randomBytes(t, "remote-blob", 300<<10), 3_000_000_000)
	return &MemorySourceView{MemorySource: m}
}

// MemorySourceView names the files a test's MemorySource holds, so the
// assertions do not repeat path strings.
type MemorySourceView struct {
	*source.MemorySource
}

func (v *MemorySourceView) path(name string) string { return name }

func injectMemory(t *testing.T, m *source.MemorySource) BackupOptions {
	t.Helper()
	return BackupOptions{SpoolDir: t.TempDir(), Source: SourceSpec{Source: m, Locator: slices.Clone(m.Locator())}}
}

// TestBackupFromMemorySourceRestoresUnderTheLocator covers the remote
// mapping end to end: a source with an s3:// locator restores under
// target/bucket/prefix, byte for byte.
func TestBackupFromMemorySourceRestoresUnderTheLocator(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "remote-source")

	src := memoryTree(t, uint8(tree.MetaS3), "s3://bucket/prefix")
	summary, err := r.Backup(ctx, nil, injectMemory(t, src.MemorySource))
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if len(summary.Snapshot.Roots) != 1 {
		t.Fatalf("roots = %d, want 1", len(summary.Snapshot.Roots))
	}
	if got := string(summary.Snapshot.Roots[0].Path); got != "s3://bucket/prefix" {
		t.Errorf("root path = %q, want the locator", got)
	}
	// One real subdirectory entry; no symlinks anywhere in this source.
	if summary.Snapshot.Stats.Files != 3 || summary.Snapshot.Stats.Dirs != 1 || summary.Snapshot.Stats.Symlinks != 0 {
		t.Errorf("stats = %+v, want 3 files, 1 dir, no symlinks", summary.Snapshot.Stats)
	}

	restored := reopen(t, dir, "restore")
	target := filepath.Join(t.TempDir(), "out")
	if _, err := restored.Restore(ctx, summary.Handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}

	// The locator maps to target/bucket/prefix (docs/format.md §9).
	base := filepath.Join(target, "bucket", "prefix")
	note := bytes.Repeat([]byte("compressible text. "), 20000)
	for name, want := range map[string][]byte{
		"readme.txt":  []byte("hello kist\n"),
		"dir/note.md": note,
		"blob.bin":    randomBytes(t, "remote-blob", 300<<10),
	} {
		got, err := os.ReadFile(filepath.Join(base, filepath.FromSlash(name)))
		if err != nil {
			t.Fatalf("read restored %s: %v", name, err)
		}
		if !bytes.Equal(got, want) {
			t.Errorf("%s: %d bytes, want %d identical ones", name, len(got), len(want))
		}
	}
}

// TestS3EtagFastPathReusesWithoutReading is the s3 half of the fast-path
// contract (§8.2): the same etag and size prove the contents unchanged,
// however the mtime moves, so the second backup reuses the file without
// reading it and stores no new chunks.
func TestS3EtagFastPathReusesWithoutReading(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "s3-fastpath")

	src := memoryTree(t, uint8(tree.MetaS3), "s3://bucket/prefix")
	first, err := r.Backup(ctx, nil, injectMemory(t, src.MemorySource))
	if err != nil {
		t.Fatalf("first backup: %v", err)
	}
	if first.Report.FilesReused != 0 {
		t.Errorf("first backup reused %d files, want 0", first.Report.FilesReused)
	}

	// Bump every mtime; etags and sizes stay as the source vouches for.
	for _, name := range []string{"readme.txt", "dir/note.md", "blob.bin"} {
		src.SetModified(src.path(name), 9_000_000_000)
	}

	again, err := r.Backup(ctx, nil, injectMemory(t, src.MemorySource))
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if again.Report.FilesReused != 3 {
		t.Errorf("reused %d files, want 3", again.Report.FilesReused)
	}
	if again.Report.ChunksNew != 0 || again.Report.PacksNew != 0 {
		t.Errorf("second backup stored %d new chunks in %d packs, want 0/0",
			again.Report.ChunksNew, again.Report.PacksNew)
	}
	if again.Snapshot.Parent == nil || *again.Snapshot.Parent != first.Handle.Key {
		t.Errorf("parent = %v, want %s", again.Snapshot.Parent, first.Handle.Key)
	}

	// Changing the contents moves the derived etag, and the proof is
	// gone: the file is re-read and its new chunks stored -- even at the
	// same size.
	src.AddFile(src.path("readme.txt"), []byte("hello kist!\n"), 9_000_000_000)
	third, err := r.Backup(ctx, nil, injectMemory(t, src.MemorySource))
	if err != nil {
		t.Fatalf("third backup: %v", err)
	}
	if third.Report.FilesReused != 2 {
		t.Errorf("reused %d files after a real change, want 2", third.Report.FilesReused)
	}
	if third.Report.ChunksNew == 0 {
		t.Error("changed contents stored no new chunks; the etag proof did not hold")
	}
}

// TestSftpSourceHasNoFastPath is the sftp half of the contract: an
// unprovable mtime is never trusted, so the second backup re-reads the
// files -- and the chunk dedup absorbs the re-read, storing nothing new.
func TestSftpSourceHasNoFastPath(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "sftp-nofastpath")

	src := memoryTree(t, uint8(tree.MetaSFTP), "sftp://host/data")
	if _, err := r.Backup(ctx, nil, injectMemory(t, src.MemorySource)); err != nil {
		t.Fatalf("first backup: %v", err)
	}

	for _, name := range []string{"readme.txt", "dir/note.md", "blob.bin"} {
		src.SetModified(src.path(name), 9_000_000_000)
	}
	again, err := r.Backup(ctx, nil, injectMemory(t, src.MemorySource))
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if again.Report.FilesReused != 0 {
		t.Errorf("reused %d files on an sftp source, want 0: the mtime is a claim, not a proof", again.Report.FilesReused)
	}
	if again.Report.ChunksRead == 0 {
		t.Error("second backup read nothing; the files were not re-read")
	}
	if again.Report.ChunksNew != 0 {
		t.Errorf("re-reading identical contents stored %d new chunks, want 0: dedup must absorb it",
			again.Report.ChunksNew)
	}

	// sftp directories carry no time, so they are recorded as the
	// conservative generic kind (§8.1: the sftp kind requires an mtime).
	entries, err := r.LoadTreeChain(ctx, again.Snapshot.Roots[0].Tree)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if tree.NodeType(entry.Type) == tree.TypeDir && tree.MetaKind(entry.MetaKind) != tree.MetaGeneric {
			t.Errorf("sftp directory %q recorded as kind %d, want generic", entry.Name, entry.MetaKind)
		}
		if tree.NodeType(entry.Type) == tree.TypeFile && (len(entry.Etag) > 0 || entry.Mode != nil) {
			t.Errorf("sftp file %q carries s3/posix fields %+v", entry.Name, entry)
		}
	}
}

// TestSingleFileSourceRestoresAtItsOwnPath: a remote root that names one
// file produces a root tree with exactly that entry, and restore puts it
// at target/<locator minus last component>/<name>.
func TestSingleFileSourceRestoresAtItsOwnPath(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "single-file")

	m := source.NewMemorySource("s3://bucket/data", uint8(tree.MetaS3))
	m.AddFile("data", []byte("one file's worth"), 7)
	summary, err := r.Backup(ctx, nil, injectMemory(t, m))
	if err != nil {
		t.Fatalf("backup: %v", err)
	}

	// The file-root rule, seen from the snapshot: one non-dir entry
	// named like the locator's last component.
	entries, err := r.LoadTreeChain(ctx, summary.Snapshot.Roots[0].Tree)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || tree.NodeType(entries[0].Type) != tree.TypeFile || string(entries[0].Name) != "data" {
		t.Fatalf("root tree = %+v, want the single file entry data", entries)
	}

	restored := reopen(t, dir, "restore")
	target := filepath.Join(t.TempDir(), "out")
	if _, err := restored.Restore(ctx, summary.Handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}
	got, err := os.ReadFile(filepath.Join(target, "bucket", "data"))
	if err != nil {
		t.Fatalf("read restored file: %v", err)
	}
	if string(got) != "one file's worth" {
		t.Errorf("restored %q", got)
	}
}

// TestRemoteURLRequiresExactlyOneRoot: a remote URL is an opaque
// locator; the paths argument must be exactly it.
func TestRemoteURLRequiresExactlyOneRoot(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "url-args")

	err := func() error {
		_, err := r.Backup(ctx, []string{"/somewhere/else"}, BackupOptions{
			SpoolDir: t.TempDir(),
			Source:   SourceSpec{URL: "s3://bucket/prefix"},
		})
		return err
	}()
	if err == nil {
		t.Fatal("a URL plus a different path was accepted")
	}
}

// TestLocalSecondBackupReusesFiles pins the posix half of the fast path:
// a second backup of untouched local files reuses them from the parent's
// metadata without reading a byte (the kernel's own bookkeeping is the
// proof), while anything modified after the parent's start is re-read --
// the racy guard.
func TestLocalSecondBackupReusesFiles(t *testing.T) {
	ctx := context.Background()
	r, _ := initRealtimeRepo(t, "local-fastpath")

	source := t.TempDir()
	writeTree(t, source, []fileSpec{
		{path: "a.txt", data: []byte("stable")},
		{path: "b.txt", data: randomBytes(t, "stable-blob", 100<<10)},
	})
	if _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatalf("first backup: %v", err)
	}

	again, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if again.Report.FilesReused != 2 {
		t.Errorf("reused %d files, want 2", again.Report.FilesReused)
	}
	if again.Report.ChunksNew != 0 {
		t.Errorf("stored %d new chunks, want 0", again.Report.ChunksNew)
	}

	// Touch one file after the parent's start, then roll its mtime back
	// to look unchanged: the change time still says otherwise, and the
	// racy guard exists for exactly this lie. The file must be re-read.
	if err := os.WriteFile(filepath.Join(source, "a.txt"), []byte("stable"), 0o644); err != nil {
		t.Fatal(err)
	}
	past := time.Unix(0, again.Snapshot.TimeNs-int64(time.Minute))
	if err := os.Chtimes(filepath.Join(source, "a.txt"), past, past); err != nil {
		t.Fatal(err)
	}
	third, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("third backup: %v", err)
	}
	if third.Report.FilesReused != 1 {
		t.Errorf("reused %d files after a rolled-back touch, want 1 (the untouched one)",
			third.Report.FilesReused)
	}
}

// TestFastPathDoesNotReuseMarkedPacks: the metadata proof is necessary
// but not sufficient -- a chunk sitting in a pack prune has marked is
// treated as absent, and the file is read and re-uploaded (the revive
// path), never carried over from a pack that may be deleted.
func TestFastPathDoesNotReuseMarkedPacks(t *testing.T) {
	ctx := context.Background()
	r, _ := initRealtimeRepo(t, "marked-reuse")

	source := t.TempDir()
	writeTree(t, source, []fileSpec{{path: "a.txt", data: randomBytes(t, "marked-blob", 100<<10)}})
	if _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatalf("first backup: %v", err)
	}

	// Mark every pack the way prune would: an 8-byte gc/<pack> object,
	// fresh enough to be inside any grace period.
	for _, pack := range r.Index().Packs() {
		if err := r.Backend().Put(ctx, gcKey(pack), bytes.NewReader(GCMarkMagic), int64(len(GCMarkMagic))); err != nil {
			t.Fatalf("mark %s: %v", pack, err)
		}
	}

	again, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if again.Report.FilesReused != 0 {
		t.Errorf("reused %d files from marked packs, want 0", again.Report.FilesReused)
	}
	if again.Report.PacksRevived == 0 {
		t.Error("re-uploaded nothing out of the marked pack; the revive path did not run")
	}
}

// TestIndirectEntryIsReusedByTheFastPath: a file whose chunk list went
// indirect (more than tree.MaxInlineChunks chunks, via a repository with
// tiny chunk sizes) is reused from the parent like any other -- and the
// reuse re-verifies the list's own chunks, so a list the index can no
// longer resolve forces a re-read.
func TestIndirectEntryIsReusedByTheFastPath(t *testing.T) {
	ctx := context.Background()

	tiny := ChunkerParams{MinSize: 64, AvgSize: 256, MaxSize: 1024}
	r, _ := initRepoWithChunker(t, "indirect-reuse", tiny)

	source := t.TempDir()
	writeTree(t, source, []fileSpec{{path: "big.bin", data: randomBytes(t, "indirect-blob", 512<<10)}})
	first, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("first backup: %v", err)
	}
	entries, err := r.LoadTreeChain(ctx, first.Snapshot.Roots[0].Tree)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || tree.ContentType(entries[0].ContentType) != tree.ContentIndirect {
		t.Fatalf("fixture did not produce an indirect entry: %+v", entries)
	}

	again, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if again.Report.FilesReused != 1 {
		t.Errorf("reused %d indirect entries, want 1", again.Report.FilesReused)
	}
	if again.Report.ChunksNew != 0 {
		t.Errorf("reusing an indirect entry stored %d new chunks, want 0", again.Report.ChunksNew)
	}

	// Changed contents end the reuse; the new chunk list is indirect
	// again and its chunks are stored.
	if err := os.WriteFile(filepath.Join(source, "big.bin"), randomBytes(t, "indirect-blob-2", 512<<10), 0o644); err != nil {
		t.Fatal(err)
	}
	third, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("third backup: %v", err)
	}
	if third.Report.FilesReused != 0 {
		t.Errorf("reused an entry whose contents changed, want 0")
	}
	if third.Report.ChunksNew == 0 {
		t.Error("changed contents stored no new chunks")
	}
}
