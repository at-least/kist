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

	summary, err := r.Backup(ctx, []string{source}, BackupOptions{Host: "testhost", SpoolDir: t.TempDir()})
	snap, handle := summary.Snapshot, summary.Handle
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if snap.Stats.Files == 0 || summary.Report.PacksNew == 0 {
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

	first, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("first backup: %v", err)
	}
	if first.Report.PacksNew == 0 {
		t.Fatal("the first backup wrote no packs")
	}
	packsAfterFirst := countKeys(t, r.Backend(), pack.Prefix)
	treesAfterFirst := countKeys(t, r.Backend(), "trees/")

	second := reopen(t, dir, "incremental-2")
	stats, err := second.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}

	if stats.Report.ChunksNew != 0 {
		t.Errorf("second backup stored %d new chunks, want 0", stats.Report.ChunksNew)
	}
	if stats.Report.PacksNew != 0 {
		t.Errorf("second backup wrote %d packs, want 0", stats.Report.PacksNew)
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
	if _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatalf("first backup: %v", err)
	}
	treesAfterFirst := countKeys(t, r.Backend(), "trees/")

	writeTree(t, source, []fileSpec{{path: "docs/deep/small.bin", data: randomBytes(t, "changed", 1024)}})

	second := reopen(t, dir, "changed-2")
	stats, err := second.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if stats.Report.ChunksNew != 1 {
		t.Errorf("stored %d new chunks for one changed 1 KiB file, want 1", stats.Report.ChunksNew)
	}

	// v3 has no synthetic root: source/, docs/ and deep/ are on the path
	// from the changed file up; docs/deep/nested/ is not.
	newTrees := countKeys(t, second.Backend(), "trees/") - treesAfterFirst
	if newTrees != 3 {
		t.Errorf("wrote %d new trees, want 3 (source, docs, deep)", newTrees)
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
	summary, err := r.Backup(ctx, []string{source}, BackupOptions{
		SpoolDir: t.TempDir(),
		Warnf:    func(format string, args ...any) { warnings = append(warnings, fmt.Sprintf(format, args...)) },
	})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if summary.Snapshot.Stats.Files != 1 {
		t.Errorf("backed up %d files, want 1", summary.Snapshot.Stats.Files)
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

	summary, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	handle := summary.Handle
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	// One chunk of data, stored once, even though two names point at it.
	if summary.Report.ChunksNew != 1 {
		t.Errorf("stored %d chunks for one file under two names, want 1", summary.Report.ChunksNew)
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
	summary, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	handle := summary.Handle
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

	summary, err := r.Backup(ctx, []string{second, first}, BackupOptions{SpoolDir: t.TempDir()})
	snap, handle := summary.Snapshot, summary.Handle
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if len(snap.Roots) != 2 {
		t.Errorf("snapshot records %d roots, want 2", len(snap.Roots))
	}
	var pathStrings []string
	for _, p := range snap.Roots {
		pathStrings = append(pathStrings, string(p.Path))
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

	if _, err := r.Backup(context.Background(), nil, BackupOptions{}); err == nil {
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

	summary, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	handle := summary.Handle
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if summary.Report.ChunksNew != 1 {
		t.Errorf("stored %d chunks for three copies of one payload, want 1", summary.Report.ChunksNew)
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

// A single-file source names its root-tree FILE entry by the file's
// absolute path (docs/format.md §7), and restore creates the parents for
// it, so the file comes back under the target by its full path.
func TestBackupOfASingleFileRestores(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "single-file")

	src := filepath.Join(t.TempDir(), "one.txt")
	if err := os.WriteFile(src, []byte("single file"), 0o644); err != nil {
		t.Fatal(err)
	}
	summary, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	handle := summary.Handle
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

	summary, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	handle := summary.Handle
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if summary.Snapshot.Stats.Files != uint64(total) {
		t.Fatalf("backed up %d files, want %d", summary.Snapshot.Stats.Files, total)
	}
	if len(summary.Snapshot.Roots) != 1 {
		t.Fatalf("snapshot records %d roots, want 1", len(summary.Snapshot.Roots))
	}

	// v3: the root tree IS the source directory's contents — the chain of
	// both segments holds every entry directly (no synthetic root).
	all, err := r.LoadTreeChain(ctx, summary.Snapshot.Roots[0].Tree)
	if err != nil {
		t.Fatalf("root chain: %v", err)
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
	if _, err := second.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
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

	// v3: the root tree holds the source directory's children directly
	// (single-component names), with the indirect file inside it.
	dirTree := tree.New([]tree.Entry{{
		Name: []byte("big.bin"), Type: uint8(tree.TypeFile), MetaKind: 2,
		MTimeNs: func() *int64 { v := int64(1767225845000000000); return &v }(),
		Size:    uint64(len(payload)), Chunks: listChunks, ContentType: uint8(tree.ContentIndirect),
	}})
	dirID, encoded, err := dirTree.Encode(&r.keys.Hash)
	if err != nil {
		t.Fatal(err)
	}
	sealed, err := crypto.Seal(&r.keys.Meta, dirID[:], encoded, crypto.DeterministicReader("indirect-restore-dir"))
	if err != nil {
		t.Fatal(err)
	}
	if err := r.Backend().Put(ctx, tree.Key(dirID), bytes.NewReader(sealed), int64(len(sealed))); err != nil {
		t.Fatal(err)
	}
	snap := &snapshot.Snapshot{
		Version: snapshot.Version,
		Roots:   []snapshot.Root{{Path: []byte("/virtual"), Tree: dirID}},
		TimeNs:  1767225845000000001,
		Host:    "t", ClientID: r.clientID,
	}
	handle, err := snap.Save(ctx, r.Backend(), r.keys, crypto.DeterministicReader("snap"), 0)
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
	var rootLists [2][]snapshot.Root
	for i, h := range handles[len(handles)-2:] {
		snap, err := r.LoadSnapshot(ctx, h.Key)
		if err != nil {
			t.Logf("load %s: %v", h.Key, err)
			return
		}
		rootLists[i] = snap.Roots
	}
	// Pair up roots by locator; unpaired roots are logged whole.
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
	byPath := map[string][2]crypto.ID{}
	for i, roots := range rootLists {
		for _, r := range roots {
			pair := byPath[string(r.Path)]
			pair[i] = r.Tree
			byPath[string(r.Path)] = pair
		}
	}
	for path, pair := range byPath {
		walk(path, pair[0], pair[1])
	}
}

// docs/format.md §9 (v3) pins how stats count: dirs counts directory
// ENTRIES (roots are paths, not entries); files counts every name, hard
// links included; bytes counts hard-linked content once. kist-rs asserts
// the same numbers (kist-core/tests/backup_restore.rs).
func TestSnapshotStatsFollowTheSpecCounting(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("hard links are not tracked on Windows")
	}
	ctx := context.Background()
	r, _ := initRepo(t, "stats")

	source := t.TempDir()
	payload := randomBytes(t, "stats-data", 300<<10)
	writeTree(t, source, []fileSpec{{path: "a.bin", data: payload}, {path: "sub/c.txt", data: []byte("c")}})
	if err := os.Link(filepath.Join(source, "a.bin"), filepath.Join(source, "b.bin")); err != nil {
		t.Fatalf("link: %v", err)
	}
	if err := os.Mkdir(filepath.Join(source, "empty"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("a.bin", filepath.Join(source, "link")); err != nil {
		t.Fatal(err)
	}

	snap, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	// v3 口徑（§9.1）：dirs 只數目錄 entry（sub、empty；root 是 path 不是
	// entry），bytes 的 hard link 內容只算一次（a.bin + 1 byte 的 c.txt）。
	want := snapshot.Stats{Files: 3, Dirs: 2, Symlinks: 1, Bytes: uint64(len(payload)) + 1}
	got := snapshot.Stats{Files: snap.Snapshot.Stats.Files, Dirs: snap.Snapshot.Stats.Dirs,
		Symlinks: snap.Snapshot.Stats.Symlinks, Bytes: snap.Snapshot.Stats.Bytes}
	if got != want {
		t.Errorf("stats = %+v, want %+v", got, want)
	}
}
