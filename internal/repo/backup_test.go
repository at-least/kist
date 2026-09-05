package repo

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"sort"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/chunker"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
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
		{path: "setuid.bin", data: []byte("pretend this is a binary"), mode: 0o755 | fs.ModeSetuid},
		{path: "setgid.bin", data: []byte("and this one too"), mode: 0o2755&^0o2000 | fs.ModeSetgid},
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
		// Compare the special bits too, not just Perm: a restore that
		// drops setuid is a restore that produced something different.
		const modeBits = fs.ModePerm | fs.ModeSetuid | fs.ModeSetgid | fs.ModeSticky
		if runtime.GOOS != "windows" && wantInfo.Mode()&modeBits != gotInfo.Mode()&modeBits {
			t.Errorf("%s: mode = %v, want %v", name, gotInfo.Mode()&modeBits, wantInfo.Mode()&modeBits)
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

	// The snapshot root holds one entry per source path, named by the
	// source's own bytes: the v2 rule is the full absolute path, so a
	// restore rebuilds it under the target component by component.
	compareTrees(t, source, filepath.Join(target, source))
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
		logTreeDifferences(t, second)
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

	base := filepath.Join(target, source)
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
	var pathStrings []string
	for _, p := range snap.Paths {
		pathStrings = append(pathStrings, string(p))
	}
	if !sort.StringsAreSorted(pathStrings) {
		t.Errorf("snapshot paths are not sorted: %v", pathStrings)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := r.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}
	for _, root := range []string{first, second} {
		// Each source is restored under the target by its full path.
		if _, err := os.Stat(filepath.Join(target, root)); err != nil {
			t.Errorf("restored tree is missing %s: %v", root, err)
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
	compareTrees(t, source, filepath.Join(target, source))
}

// PRODUCTION GAP (v2): `kist backup <single-file>` writes a root-tree
// FILE entry named by the file's absolute path, and restore never creates
// the intermediate directories for such an entry -- it only mkdirs for
// directory entries -- so the snapshot commits but cannot be restored
// (repro: kist backup /tmp/x/one.txt; kist restore <key> /tmp/out ->
// "open .../out/tmp/x/one.txt: no such file or directory"). The fix
// belongs in restore (create parents for absolute-path file entries) or
// in backup (another naming for file sources); this test pins the
// end-to-end OUTCOME -- the file comes back under the target by its full
// path, per docs/format.md §7 -- not the mechanism. Enable after fixing.
const singleFileRestoreGap = "PRODUCTION GAP: restore cannot recreate absolute-path file entries at the snapshot root (no parent directories are created)"

func TestBackupOfASingleFileRestores(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "single-file")

	src := filepath.Join(t.TempDir(), "one.txt")
	if err := os.WriteFile(src, []byte("single file"), 0o644); err != nil {
		t.Fatal(err)
	}
	_, handle, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := r.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}
	got, err := os.ReadFile(filepath.Join(target, src))
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "single file" {
		t.Errorf("restored %q, want %q", got, "single file")
	}
}

// v2 splits a directory larger than MaxNodesPerTree into a chain of tree
// segments linked by Prev; the parent records the LAST segment's ID and
// unchanged earlier segments keep their names.
func TestBackupSegmentsHugeDirectories(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "segments")

	source := t.TempDir()
	total := tree.MaxNodesPerTree + 5
	specs := make([]fileSpec, 0, total)
	for i := range total {
		if i%2500 == 0 {
			specs = append(specs, fileSpec{path: fmt.Sprintf("f%06d.bin", i), data: randomBytes(t, fmt.Sprintf("seg-%d", i), 1024)})
		} else {
			specs = append(specs, fileSpec{path: fmt.Sprintf("f%06d.bin", i)})
		}
	}
	writeTree(t, source, specs)

	snap, handle, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if snap.Stats.Files != uint64(total) {
		t.Fatalf("backed up %d files, want %d", snap.Stats.Files, total)
	}

	// The source directory's tree is the chain of both segments.
	entries, err := r.LoadTreeChain(ctx, snap.Root)
	if err != nil {
		t.Fatalf("root chain: %v", err)
	}
	if len(entries) != 1 || string(entries[0].Name) != source || entries[0].Subtree == nil {
		t.Fatalf("root holds %+v, want the one source %q", entries[0], source)
	}
	all, err := r.LoadTreeChain(ctx, *entries[0].Subtree)
	if err != nil {
		t.Fatalf("dir chain: %v", err)
	}
	if len(all) != total {
		t.Fatalf("chain reassembled %d entries, want %d", len(all), total)
	}
	for i, e := range all {
		if want := fmt.Sprintf("f%06d.bin", i); string(e.Name) != want {
			t.Fatalf("entry %d = %q, want %q (segments out of order)", i, e.Name, want)
		}
	}

	// A second, unchanged backup writes no new segments.
	treesAfterFirst := countKeys(t, r.Backend(), "trees/")
	second := reopen(t, dir, "segments-2")
	if _, _, err := second.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if got := countKeys(t, second.Backend(), "trees/"); got != treesAfterFirst {
		t.Errorf("%d trees after the second backup, want the original %d: earlier segments were not reused", got, treesAfterFirst)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := second.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}
	compareTrees(t, source, filepath.Join(target, source))
}

// v2 stores a file with more than MaxInlineChunks chunks indirectly: the
// chunk list itself is encoded as a ChunkList, chunked like data, and the
// entry points at those chunks with ContentType indirect. A file big
// enough to trip the limit organically is half a gigabyte, so this test
// builds the objects directly and exercises the restore side end to end.
func TestRestoreFollowsIndirectChunkLists(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "indirect")

	payload := randomBytes(t, "indirect-payload", 3<<20)
	var ids []crypto.ID
	var put [][]byte
	c, err := chunker.New(bytes.NewReader(payload))
	if err != nil {
		t.Fatal(err)
	}
	for {
		chunk, err := c.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		id := crypto.ContentID(&r.keys.Hash, chunk.Data)
		ids = append(ids, id)
		put = append(put, append([]byte(nil), chunk.Data...))
	}
	if len(ids) < 2 {
		t.Fatalf("payload produced %d chunks, want several", len(ids))
	}

	// Store the content chunks, then the encoded chunk list, in packs.
	store := func(chunks map[crypto.ID][]byte) []crypto.ID {
		w, err := pack.NewWriter(r.keys, t.TempDir(), crypto.DeterministicReader("indirect"))
		if err != nil {
			t.Fatal(err)
		}
		var order []crypto.ID
		for id, data := range chunks {
			if err := w.Add(id, data); err != nil {
				t.Fatal(err)
			}
			order = append(order, id)
		}
		packID, entries, _, err := w.Finish(ctx, r.Backend())
		if err != nil {
			t.Fatal(err)
		}
		r.index.AddPack(packID, entries)
		slices.SortFunc(order, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
		return order
	}
	contentChunks := make(map[crypto.ID][]byte, len(ids))
	for i, id := range ids {
		contentChunks[id] = put[i]
	}
	if _, err := r.RebuildIndex(ctx); err != nil {
		t.Fatalf("rebuild: %v", err)
	}
	store(contentChunks)

	encoded, err := crypto.Marshal(tree.NewChunkList(ids))
	if err != nil {
		t.Fatal(err)
	}
	listID := crypto.ContentID(&r.keys.Hash, encoded)
	listChunks := store(map[crypto.ID][]byte{listID: encoded})

	// The snapshot root holds one absolute-path directory entry -- the
	// shape a directory-source backup produces -- with the indirect file
	// inside it.
	dirTree := tree.New([]tree.Entry{{
		Name: []byte("big.bin"), Type: uint8(tree.TypeFile), Mode: 0o644,
		Size: uint64(len(payload)), MTimeNs: 1767225845000000000,
		Chunks: listChunks, ContentType: uint8(tree.ContentIndirect),
	}})
	dirID, err := dirTree.Save(ctx, r.Backend(), r.keys, crypto.DeterministicReader("dir"))
	if err != nil {
		t.Fatal(err)
	}
	root := tree.New([]tree.Entry{{
		Name: []byte("/virtual"), Type: uint8(tree.TypeDir), Mode: 0o755 | uint32(os.ModeDir),
		MTimeNs: 1767225845000000000, Subtree: &dirID,
	}})
	rootID, err := root.Save(ctx, r.Backend(), r.keys, crypto.DeterministicReader("root"))
	if err != nil {
		t.Fatal(err)
	}

	snap := &snapshot.Snapshot{
		Version: snapshot.Version, Root: rootID, TimeNs: 1767225845000000001,
		Host: "t", Paths: [][]byte{[]byte("/virtual")}, ClientID: r.clientID,
	}
	handle, err := snap.Save(ctx, r.Backend(), r.keys, crypto.DeterministicReader("snap"))
	if err != nil {
		t.Fatal(err)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := r.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore: %v", err)
	}
	got, err := os.ReadFile(filepath.Join(target, "virtual", "big.bin"))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, payload) {
		t.Errorf("indirect restore produced %d bytes that differ from the %d-byte payload", len(got), len(payload))
	}
}

// logTreeDifferences prints, for the two most recent snapshots, every
// entry whose tree differs between them: what a platform did differently
// between two walks of the same directory.
func logTreeDifferences(t *testing.T, r *Repository) {
	t.Helper()
	ctx := context.Background()
	handles, err := r.Snapshots(ctx, "")
	if err != nil || len(handles) < 2 {
		t.Logf("cannot diff snapshots: %v", err)
		return
	}
	var roots [2]crypto.ID
	for i, h := range handles[len(handles)-2:] {
		snap, err := r.LoadSnapshot(ctx, h.Key)
		if err != nil {
			t.Logf("load %s: %v", h.Key, err)
			return
		}
		roots[i] = snap.Root
	}
	var walk func(path string, a, b crypto.ID)
	walk = func(path string, a, b crypto.ID) {
		if a == b {
			return
		}
		ta, errA := r.LoadTree(ctx, a)
		tb, errB := r.LoadTree(ctx, b)
		if errA != nil || errB != nil {
			t.Logf("%s: load trees: %v %v", path, errA, errB)
			return
		}
		byName := map[string]tree.Entry{}
		for _, e := range tb.Entries {
			byName[string(e.Name)] = e
		}
		for _, ea := range ta.Entries {
			eb, ok := byName[string(ea.Name)]
			if !ok {
				t.Logf("%s/%s: only in the first snapshot", path, ea.Name)
				continue
			}
			if fmt.Sprintf("%+v", ea) != fmt.Sprintf("%+v", eb) {
				t.Logf("%s/%s differs:\n  first:  %+v\n  second: %+v", path, ea.Name, ea, eb)
			}
			if tree.NodeType(ea.Type) == tree.TypeDir && ea.Subtree != nil && eb.Subtree != nil {
				walk(path+"/"+string(ea.Name), *ea.Subtree, *eb.Subtree)
			}
		}
	}
	walk("", roots[0], roots[1])
}
