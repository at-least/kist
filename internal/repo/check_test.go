package repo

import (
	"bytes"
	"context"

	"path/filepath"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
)

// backedUpRepo returns a repository with one snapshot in it, and the
// directory it lives in.
func backedUpRepo(t *testing.T, seed string) (*Repository, string, snapshot.Handle) {
	t.Helper()

	r, dir := initRepo(t, seed)
	source := t.TempDir()
	writeTree(t, source, sampleFiles(t))

	_, handle, err := r.Backup(context.Background(), []string{source}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	return r, dir, handle
}

func TestCheckPassesOnAHealthyRepository(t *testing.T) {
	ctx := context.Background()
	r, _, _ := backedUpRepo(t, "check-ok")

	for _, readData := range []bool{false, true} {
		report, err := r.Check(ctx, CheckOptions{ReadData: readData})
		if err != nil {
			t.Fatalf("check(read-data=%v): %v", readData, err)
		}
		if !report.OK() {
			t.Errorf("check(read-data=%v) found problems: %v", readData, report.Problems)
		}
		if report.Snapshots != 1 || report.Packs == 0 || report.Chunks == 0 || report.Trees == 0 {
			t.Errorf("check(read-data=%v) report = %+v, want non-zero counts", readData, report)
		}
	}
}

// The two check levels catch different damage and neither subsumes the
// other. This is the test that says which is which.
func TestCheckDetectsDamage(t *testing.T) {
	ctx := context.Background()

	cases := []struct {
		name string
		// damage mutates the repository. It returns a substring the
		// report must mention.
		damage func(t *testing.T, r *Repository) string
		// structural says whether a plain check catches it, or whether it
		// takes --read-data.
		structural bool
	}{
		{
			name:       "a pack is deleted",
			structural: true,
			damage: func(t *testing.T, r *Repository) string {
				id := anyPack(t, r)
				if err := r.Backend().Delete(ctx, pack.Key(id)); err != nil {
					t.Fatalf("delete: %v", err)
				}
				return "which no pack holds"
			},
		},
		{
			name:       "a pack is truncated",
			structural: true,
			damage: func(t *testing.T, r *Repository) string {
				id := anyPack(t, r)
				raw := mustGet(t, r, pack.Key(id))
				replace(t, r, pack.Key(id), raw[:len(raw)-4])
				return "pack " + id.String()
			},
		},
		{
			name:       "a tree is deleted",
			structural: true,
			damage: func(t *testing.T, r *Repository) string {
				key := anyKey(t, r, "trees/")
				if err := r.Backend().Delete(ctx, key); err != nil {
					t.Fatalf("delete: %v", err)
				}
				return "tree"
			},
		},
		{
			name:       "a byte inside chunk data is flipped",
			structural: false,
			damage: func(t *testing.T, r *Repository) string {
				id := anyPack(t, r)
				raw := mustGet(t, r, pack.Key(id))
				// Well inside the first chunk's ciphertext, past the
				// nonce and far from the trailer.
				flipped := bytes.Clone(raw)
				flipped[crypto.NonceSize+7] ^= 0x01
				replace(t, r, pack.Key(id), flipped)
				return "pack " + id.String()
			},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			r, dir, _ := backedUpRepo(t, "damage-"+tc.name)
			want := tc.damage(t, r)

			// Reopen: a check must work from what is stored, not from
			// what this process happens to remember.
			fresh := reopen(t, dir, "damage-check-"+tc.name)

			structural, err := fresh.Check(ctx, CheckOptions{})
			if err != nil {
				t.Fatalf("check: %v", err)
			}
			if tc.structural {
				assertReports(t, structural, want)
				return
			}
			if !structural.OK() {
				t.Logf("structural check also reported: %v", structural.Problems)
			}

			deep, err := fresh.Check(ctx, CheckOptions{ReadData: true})
			if err != nil {
				t.Fatalf("check --read-data: %v", err)
			}
			assertReports(t, deep, want)

			if structural.OK() && deep.OK() {
				t.Error("neither check level noticed the damage")
			}
		})
	}
}

// The bit-flip case is the one that matters most: a structural check must
// not claim a repository is healthy when its data is not.
func TestOnlyReadDataCatchesAFlippedBitInAChunk(t *testing.T) {
	ctx := context.Background()
	r, dir, _ := backedUpRepo(t, "flip")

	id := anyPack(t, r)
	raw := mustGet(t, r, pack.Key(id))
	flipped := bytes.Clone(raw)
	flipped[crypto.NonceSize+7] ^= 0x01
	replace(t, r, pack.Key(id), flipped)

	fresh := reopen(t, dir, "flip-check")

	structural, err := fresh.Check(ctx, CheckOptions{})
	if err != nil {
		t.Fatalf("check: %v", err)
	}
	if !structural.OK() {
		t.Fatalf("the structural check saw a data-only bit flip, so this test no longer proves anything: %v", structural.Problems)
	}

	deep, err := fresh.Check(ctx, CheckOptions{ReadData: true})
	if err != nil {
		t.Fatalf("check --read-data: %v", err)
	}
	if deep.OK() {
		t.Fatal("check --read-data did not notice a flipped bit inside chunk data")
	}
}

// Restoring from a damaged pack must fail loudly, not produce a file that
// is quietly wrong.
func TestRestoreFromADamagedPackFails(t *testing.T) {
	ctx := context.Background()
	r, dir, handle := backedUpRepo(t, "restore-damage")

	id := anyPack(t, r)
	raw := mustGet(t, r, pack.Key(id))
	flipped := bytes.Clone(raw)
	flipped[crypto.NonceSize+7] ^= 0x01
	replace(t, r, pack.Key(id), flipped)

	fresh := reopen(t, dir, "restore-damage-2")
	target := filepath.Join(t.TempDir(), "out")
	if _, err := fresh.Restore(ctx, handle.Key, target, RestoreOptions{}); err == nil {
		t.Fatal("restore from a damaged pack: got nil error")
	}
}

// An index is a cache: delete every blob and a rebuild must recover the
// same answers, with restore working again afterwards.
func TestRebuildIndexRecoversFromDeletedBlobs(t *testing.T) {
	ctx := context.Background()
	r, dir, handle := backedUpRepo(t, "rebuild")

	before := r.Index().Len()
	if err := r.Backend().List(ctx, index.Prefix, func(fi backend.FileInfo) error {
		return r.Backend().Delete(ctx, fi.Key)
	}); err != nil {
		t.Fatalf("delete index blobs: %v", err)
	}

	fresh := reopen(t, dir, "rebuild-2")
	if fresh.Index().Len() != 0 {
		t.Fatalf("index still holds %d chunks after its blobs were deleted", fresh.Index().Len())
	}

	n, err := fresh.RebuildIndex(ctx)
	if err != nil {
		t.Fatalf("rebuild index: %v", err)
	}
	if n != before {
		t.Errorf("rebuilt index holds %d chunks, the original held %d", n, before)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := fresh.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore after rebuild: %v", err)
	}
}

func assertReports(t *testing.T, report CheckReport, want string) {
	t.Helper()

	if report.OK() {
		t.Fatalf("check found no problems, expected one mentioning %q", want)
	}
	for _, p := range report.Problems {
		if strings.Contains(p, want) {
			return
		}
	}
	t.Errorf("problems = %v, want one mentioning %q", report.Problems, want)
}

func anyPack(t *testing.T, r *Repository) crypto.ID {
	t.Helper()

	key := anyKey(t, r, pack.Prefix)
	id, err := crypto.ParseID(key[len(pack.Prefix):])
	if err != nil {
		t.Fatalf("parse pack id: %v", err)
	}
	return id
}

// anyKey returns the lexically first key under a prefix, so that a test
// that damages "some object" damages the same one on every run.
func anyKey(t *testing.T, r *Repository, prefix string) string {
	t.Helper()

	var first string
	if err := r.Backend().List(context.Background(), prefix, func(fi backend.FileInfo) error {
		if first == "" || fi.Key < first {
			first = fi.Key
		}
		return nil
	}); err != nil {
		t.Fatalf("list %q: %v", prefix, err)
	}
	if first == "" {
		t.Fatalf("no objects under %q", prefix)
	}
	return first
}

func mustGet(t *testing.T, r *Repository, key string) []byte {
	t.Helper()

	data, err := backend.GetAll(context.Background(), r.Backend(), key)
	if err != nil {
		t.Fatalf("get %s: %v", key, err)
	}
	return data
}

// replace overwrites an object, which is exactly what a repository's
// format forbids -- which is the point: this is how damage arrives.
func replace(t *testing.T, r *Repository, key string, data []byte) {
	t.Helper()

	ctx := context.Background()
	if err := r.Backend().Delete(ctx, key); err != nil {
		t.Fatalf("delete %s: %v", key, err)
	}
	if err := backend.PutBytesIfAbsent(ctx, r.Backend(), key, data); err != nil {
		t.Fatalf("store %s: %v", key, err)
	}
}

// A restore is the one moment when whoever controls the repository gets
// to choose filenames on the machine doing the restoring. Names that are
// not a single component inside the parent must be refused, whatever a
// tree object claims.
func TestSafeJoinRefusesEscapes(t *testing.T) {
	dir := filepath.Join(string(filepath.Separator), "restore", "target")

	for _, name := range []string{
		"", ".", "..",
		"../escape",
		"a/b",
		"a" + string(filepath.Separator) + "b",
		"/absolute",
		"nul\x00byte",
		"..",
	} {
		t.Run(strings.ReplaceAll(name, "\x00", "NUL"), func(t *testing.T) {
			if got, err := safeJoin(dir, name); err == nil {
				t.Fatalf("safeJoin(%q) = %q, want an error", name, got)
			}
		})
	}

	got, err := safeJoin(dir, "ordinary.txt")
	if err != nil {
		t.Fatalf("safeJoin of an ordinary name: %v", err)
	}
	if want := filepath.Join(dir, "ordinary.txt"); got != want {
		t.Errorf("safeJoin = %q, want %q", got, want)
	}
}

// rebuild-index has to leave the repair on disk. A rebuild that only
// updates the running process repairs nothing: the next Open reloads the
// same missing or damaged blobs.
func TestRebuildIndexPersists(t *testing.T) {
	ctx := context.Background()
	r, dir, handle := backedUpRepo(t, "rebuild-persist")
	before := r.Index().Len()

	if err := r.Backend().List(ctx, index.Prefix, func(fi backend.FileInfo) error {
		return r.Backend().Delete(ctx, fi.Key)
	}); err != nil {
		t.Fatalf("delete index blobs: %v", err)
	}

	repairing := reopen(t, dir, "rebuild-persist-2")
	if _, err := repairing.RebuildIndex(ctx); err != nil {
		t.Fatalf("rebuild index: %v", err)
	}

	// A fresh process, with no rebuild call of its own.
	after := reopen(t, dir, "rebuild-persist-3")
	if after.Index().Len() != before {
		t.Errorf("after reopening, the index holds %d chunks, want %d", after.Index().Len(), before)
	}
	if n := countKeys(t, after.Backend(), index.Prefix); n != 1 {
		t.Errorf("repository holds %d index blobs, want exactly 1", n)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := after.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore after a persisted rebuild: %v", err)
	}
}

// A damaged index blob must not make a repository unopenable. It is a
// cache, and opening the repository is how you reach the command that
// repairs it -- making it fatal would be a catch-22.
func TestADamagedIndexBlobIsRepairable(t *testing.T) {
	ctx := context.Background()
	r, dir, handle := backedUpRepo(t, "bad-blob")
	before := r.Index().Len()

	blobKey := anyKey(t, r, index.Prefix)
	damaged := bytes.Clone(mustGet(t, r, blobKey))
	damaged[len(damaged)/2] ^= 0x01
	replace(t, r, blobKey, damaged)

	// Also drop in an object that is not named like a blob at all.
	if err := backend.PutBytesIfAbsent(ctx, r.Backend(), index.Prefix+"not-a-hash", []byte("junk")); err != nil {
		t.Fatalf("store: %v", err)
	}

	var warnings []string
	opened := reopenWarning(t, dir, "bad-blob-2", &warnings)
	if len(warnings) != 2 {
		t.Errorf("opening warned %d times, want 2 (the damaged blob and the misnamed object): %v", len(warnings), warnings)
	}

	report, err := opened.Check(ctx, CheckOptions{})
	if err != nil {
		t.Fatalf("check: %v", err)
	}
	if report.OK() {
		t.Error("check did not report the damaged index blob")
	}

	if _, err := opened.RebuildIndex(ctx); err != nil {
		t.Fatalf("rebuild index: %v", err)
	}

	var afterWarnings []string
	repaired := reopenWarning(t, dir, "bad-blob-3", &afterWarnings)
	if len(afterWarnings) != 0 {
		t.Errorf("after a rebuild, opening still warned: %v", afterWarnings)
	}
	if repaired.Index().Len() != before {
		t.Errorf("repaired index holds %d chunks, want %d", repaired.Index().Len(), before)
	}

	clean, err := repaired.Check(ctx, CheckOptions{ReadData: true})
	if err != nil {
		t.Fatalf("check: %v", err)
	}
	if !clean.OK() {
		t.Errorf("check after the repair found problems: %v", clean.Problems)
	}

	target := filepath.Join(t.TempDir(), "out")
	if _, err := repaired.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore after the repair: %v", err)
	}
}
