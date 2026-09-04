package repo

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
)

// fileSpec describes one file to create for a test.
type fileSpec struct {
	path string
	data []byte
	mode fs.FileMode
}

func writeTree(t *testing.T, root string, files []fileSpec) {
	t.Helper()

	for _, f := range files {
		full := filepath.Join(root, filepath.FromSlash(f.path))
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatalf("mkdir: %v", err)
		}
		mode := f.mode
		if mode == 0 {
			mode = 0o644
		}
		if err := os.WriteFile(full, f.data, mode); err != nil {
			t.Fatalf("write %s: %v", full, err)
		}
		// WriteFile respects umask; the test cares about the exact mode.
		if err := os.Chmod(full, mode); err != nil {
			t.Fatalf("chmod %s: %v", full, err)
		}
	}
}

func randomBytes(t *testing.T, seed string, n int) []byte {
	t.Helper()

	b := make([]byte, n)
	if _, err := io.ReadFull(crypto.DeterministicReader(seed), b); err != nil {
		t.Fatalf("generate data: %v", err)
	}
	return b
}

func sampleFiles(t *testing.T) []fileSpec {
	t.Helper()

	return []fileSpec{
		{path: "readme.txt", data: []byte("hello kist\n")},
		{path: "empty.bin", data: nil},
		{path: "exec.sh", data: []byte("#!/bin/sh\necho hi\n"), mode: 0o755},
		{path: "docs/notes.md", data: bytes.Repeat([]byte("compressible text. "), 20000)},
		{path: "docs/deep/nested/blob.bin", data: randomBytes(t, "blob", 6<<20)},
		{path: "docs/deep/small.bin", data: randomBytes(t, "small", 1024)},
	}
}

// compareTrees asserts that two directory trees are byte-for-byte and
// mode-for-mode identical.
func compareTrees(t *testing.T, want, got string) {
	t.Helper()

	collect := func(root string) map[string]fs.FileInfo {
		out := map[string]fs.FileInfo{}
		err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				return err
			}
			rel, err := filepath.Rel(root, path)
			if err != nil {
				return err
			}
			if rel == "." {
				return nil
			}
			info, err := d.Info()
			if err != nil {
				return err
			}
			out[filepath.ToSlash(rel)] = info
			return nil
		})
		if err != nil {
			t.Fatalf("walk %s: %v", root, err)
		}
		return out
	}

	wantEntries, gotEntries := collect(want), collect(got)

	var missing, extra []string
	for name := range wantEntries {
		if _, ok := gotEntries[name]; !ok {
			missing = append(missing, name)
		}
	}
	for name := range gotEntries {
		if _, ok := wantEntries[name]; !ok {
			extra = append(extra, name)
		}
	}
	sort.Strings(missing)
	sort.Strings(extra)
	if len(missing) > 0 {
		t.Errorf("restore is missing %d entries: %v", len(missing), missing)
	}
	if len(extra) > 0 {
		t.Errorf("restore has %d unexpected entries: %v", len(extra), extra)
	}

	for name, wantInfo := range wantEntries {
		gotInfo, ok := gotEntries[name]
		if !ok {
			continue
		}
		if wantInfo.IsDir() != gotInfo.IsDir() {
			t.Errorf("%s: IsDir = %v, want %v", name, gotInfo.IsDir(), wantInfo.IsDir())
			continue
		}
		if wantInfo.Mode()&fs.ModeSymlink != 0 {
			wantTarget, err := os.Readlink(filepath.Join(want, filepath.FromSlash(name)))
			if err != nil {
				t.Fatalf("readlink: %v", err)
			}
			gotTarget, err := os.Readlink(filepath.Join(got, filepath.FromSlash(name)))
			if err != nil {
				t.Fatalf("readlink: %v", err)
			}
			if wantTarget != gotTarget {
				t.Errorf("%s: symlink target = %q, want %q", name, gotTarget, wantTarget)
			}
			continue
		}
		if runtime.GOOS != "windows" && wantInfo.Mode().Perm() != gotInfo.Mode().Perm() {
			t.Errorf("%s: mode = %v, want %v", name, gotInfo.Mode().Perm(), wantInfo.Mode().Perm())
		}
		if wantInfo.IsDir() {
			continue
		}
		if wantInfo.Size() != gotInfo.Size() {
			t.Errorf("%s: size = %d, want %d", name, gotInfo.Size(), wantInfo.Size())
			continue
		}

		wantData, err := os.ReadFile(filepath.Join(want, filepath.FromSlash(name)))
		if err != nil {
			t.Fatalf("read: %v", err)
		}
		gotData, err := os.ReadFile(filepath.Join(got, filepath.FromSlash(name)))
		if err != nil {
			t.Fatalf("read: %v", err)
		}
		if !bytes.Equal(wantData, gotData) {
			t.Errorf("%s: contents differ", name)
		}
	}
}

// The M1 acceptance criterion, in miniature: back up, restore, and get
// back exactly what went in.
func TestBackupRestoreIsByteForByte(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "roundtrip")

	source := t.TempDir()
	writeTree(t, source, sampleFiles(t))
	if runtime.GOOS != "windows" {
		if err := os.Symlink("readme.txt", filepath.Join(source, "link.txt")); err != nil {
			t.Fatalf("symlink: %v", err)
		}
	}

	snap, handle, err := r.Backup(ctx, []string{source}, BackupOptions{Host: "testhost", SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if snap.Stats.Files == 0 || snap.Stats.PacksAdded == 0 {
		t.Errorf("stats look empty: %+v", snap.Stats)
	}
	if snap.Host != "testhost" {
		t.Errorf("host = %q, want testhost", snap.Host)
	}

	restored := reopen(t, dir, "restore")
	target := filepath.Join(t.TempDir(), "out")
	stats, err := restored.Restore(ctx, handle.Key, target, RestoreOptions{})
	if err != nil {
		t.Fatalf("restore: %v", err)
	}
	if stats.Files != snap.Stats.Files {
		t.Errorf("restored %d files, backed up %d", stats.Files, snap.Stats.Files)
	}

	// The snapshot root holds one entry per source path, named by its
	// base name.
	compareTrees(t, source, filepath.Join(target, filepath.Base(source)))
}

// The second backup of unchanged data must write no packs: that is the
// entire claim of content-defined chunking plus subtree reuse.
func TestSecondBackupOfUnchangedDataWritesNoPacks(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "incremental")

	source := t.TempDir()
	writeTree(t, source, sampleFiles(t))

	first, _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("first backup: %v", err)
	}
	if first.Stats.PacksAdded == 0 {
		t.Fatal("the first backup wrote no packs")
	}
	packsAfterFirst := countKeys(t, r.Backend(), pack.Prefix)
	treesAfterFirst := countKeys(t, r.Backend(), "trees/")

	second := reopen(t, dir, "incremental-2")
	stats, _, err := second.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}

	if stats.Stats.ChunksNew != 0 {
		t.Errorf("second backup stored %d new chunks, want 0", stats.Stats.ChunksNew)
	}
	if stats.Stats.PacksAdded != 0 {
		t.Errorf("second backup wrote %d packs, want 0", stats.Stats.PacksAdded)
	}
	if got := countKeys(t, second.Backend(), pack.Prefix); got != packsAfterFirst {
		t.Errorf("repository now holds %d packs, want the original %d", got, packsAfterFirst)
	}
	if got := countKeys(t, second.Backend(), "trees/"); got != treesAfterFirst {
		t.Errorf("repository now holds %d trees, want the original %d; unchanged subtrees were not reused", got, treesAfterFirst)
	}
	if got := countKeys(t, second.Backend(), snapshot.Prefix); got != 2 {
		t.Errorf("repository holds %d snapshots, want 2", got)
	}
}

// One changed file must rewrite the path from that file to the root, and
// nothing else.
func TestIncrementalBackupOnlyRewritesTheChangedPath(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "changed")

	source := t.TempDir()
	writeTree(t, source, sampleFiles(t))
	if _, _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatalf("first backup: %v", err)
	}
	treesAfterFirst := countKeys(t, r.Backend(), "trees/")

	writeTree(t, source, []fileSpec{{path: "docs/deep/small.bin", data: randomBytes(t, "changed", 1024)}})

	second := reopen(t, dir, "changed-2")
	stats, _, err := second.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if stats.Stats.ChunksNew != 1 {
		t.Errorf("stored %d new chunks for one changed 1 KiB file, want 1", stats.Stats.ChunksNew)
	}

	// source/, docs/, deep/ and the synthetic root are on the path from
	// the changed file up; docs/deep/nested/ is not.
	newTrees := countKeys(t, second.Backend(), "trees/") - treesAfterFirst
	if newTrees != 4 {
		t.Errorf("wrote %d new trees, want 4 (root, source, docs, deep)", newTrees)
	}
}

func TestBackupSkipsUnsupportedFileTypes(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("no FIFOs on Windows")
	}
	ctx := context.Background()
	r, _ := initRepo(t, "fifo")

	source := t.TempDir()
	writeTree(t, source, []fileSpec{{path: "real.txt", data: []byte("kept")}})
	if err := makeFIFO(filepath.Join(source, "pipe")); err != nil {
		t.Skipf("cannot create a FIFO here: %v", err)
	}

	var warnings []string
	snap, _, err := r.Backup(ctx, []string{source}, BackupOptions{
		SpoolDir: t.TempDir(),
		Warnf:    func(format string, args ...any) { warnings = append(warnings, fmt.Sprintf(format, args...)) },
	})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if snap.Stats.Files != 1 {
		t.Errorf("backed up %d files, want 1", snap.Stats.Files)
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "pipe") {
		t.Errorf("warnings = %v, want one naming the FIFO", warnings)
	}
}

func TestHardLinksAreStoredOnceAndRestoredAsLinks(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("hard links are not tracked on Windows")
	}
	ctx := context.Background()
	r, dir := initRepo(t, "hardlink")

	source := t.TempDir()
	payload := randomBytes(t, "hardlink-data", 1<<20)
	writeTree(t, source, []fileSpec{{path: "original.bin", data: payload}})
	if err := os.Link(filepath.Join(source, "original.bin"), filepath.Join(source, "alias.bin")); err != nil {
		t.Fatalf("link: %v", err)
	}

	snap, handle, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	// One chunk of data, stored once, even though two names point at it.
	if snap.Stats.ChunksNew != 1 {
		t.Errorf("stored %d chunks for one file under two names, want 1", snap.Stats.ChunksNew)
	}

	target := filepath.Join(t.TempDir(), "out")
	stats, err := reopen(t, dir, "hardlink-2").Restore(ctx, handle.Key, target, RestoreOptions{})
	if err != nil {
		t.Fatalf("restore: %v", err)
	}
	if stats.Links != 1 {
		t.Errorf("restored %d hard links, want 1", stats.Links)
	}

	base := filepath.Join(target, filepath.Base(source))
	if !sameInode(t, filepath.Join(base, "original.bin"), filepath.Join(base, "alias.bin")) {
		t.Error("the restored names are separate files, not one file under two names")
	}
}

func TestRestoreRefusesANonEmptyTarget(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "nonempty")

	source := t.TempDir()
	writeTree(t, source, []fileSpec{{path: "a.txt", data: []byte("a")}})
	_, handle, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}

	target := t.TempDir()
	if err := os.WriteFile(filepath.Join(target, "existing"), []byte("x"), 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}
	if _, err := r.Restore(ctx, handle.Key, target, RestoreOptions{}); err == nil || !strings.Contains(err.Error(), "not empty") {
		t.Fatalf("restore into a non-empty directory: err = %v, want a 'not empty' error", err)
	}
}

func TestBackupOfSeveralPaths(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "multipath")

	first := t.TempDir()
	second := t.TempDir()
	writeTree(t, first, []fileSpec{{path: "one.txt", data: []byte("one")}})
	writeTree(t, second, []fileSpec{{path: "two.txt", data: []byte("two")}})

	snap, handle, err := r.Backup(ctx, []string{second, first}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if len(snap.Paths) != 2 {
		t.Errorf("snapshot records %d paths, want 2", len(snap.Paths))
	}
	if !sort.StringsAreSorted(snap.Paths) {
		t.Errorf("snapshot paths are not sorted: %v", snap.Paths)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := r.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}
	for _, root := range []string{first, second} {
		if _, err := os.Stat(filepath.Join(target, filepath.Base(root))); err != nil {
			t.Errorf("restored tree is missing %s: %v", filepath.Base(root), err)
		}
	}
}

func TestBackupRejectsNoPaths(t *testing.T) {
	r, _ := initRepo(t, "nopaths")

	if _, _, err := r.Backup(context.Background(), nil, BackupOptions{}); err == nil {
		t.Fatal("backup with no paths: got nil error")
	}
}

func countKeys(t *testing.T, b backend.Backend, prefix string) int {
	t.Helper()

	n := 0
	if err := b.List(context.Background(), prefix, func(backend.FileInfo) error {
		n++
		return nil
	}); err != nil {
		t.Fatalf("list %q: %v", prefix, err)
	}
	return n
}

func sameInode(t *testing.T, a, b string) bool {
	t.Helper()

	infoA, err := os.Stat(a)
	if err != nil {
		t.Fatalf("stat %s: %v", a, err)
	}
	infoB, err := os.Stat(b)
	if err != nil {
		t.Fatalf("stat %s: %v", b, err)
	}
	return os.SameFile(infoA, infoB)
}

// Two identical files in one backup produce the same chunk twice. The
// index only learns about a chunk when its pack is finished, so a
// duplicate inside the pack still being built is invisible to the
// deduplicator -- and a pack listing one chunk twice fails its own
// trailer consistency check, which makes it unreadable.
//
// Found by the full-scale acceptance test, not by the small ones: it
// needs the same content to repeat inside a single pack.
func TestDuplicateContentWithinOnePackIsStoredOnce(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "duplicate")

	// Larger than the chunker minimum, so each copy is a real chunk, and
	// small enough that all of them land in one pack.
	payload := randomBytes(t, "duplicate-payload", 1<<20)
	source := t.TempDir()
	writeTree(t, source, []fileSpec{
		{path: "a/first.bin", data: payload},
		{path: "b/second.bin", data: payload},
		{path: "c/third.bin", data: payload},
	})

	snap, handle, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if snap.Stats.ChunksNew != 1 {
		t.Errorf("stored %d chunks for three copies of one payload, want 1", snap.Stats.ChunksNew)
	}

	// The pack must be readable, and the restore must produce all three.
	fresh := reopen(t, dir, "duplicate-2")
	report, err := fresh.Check(ctx, CheckOptions{ReadData: true})
	if err != nil {
		t.Fatalf("check: %v", err)
	}
	if !report.OK() {
		t.Fatalf("check found problems: %v", report.Problems)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := fresh.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}
	compareTrees(t, source, filepath.Join(target, filepath.Base(source)))
}
