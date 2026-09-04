package repo

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
)

// A clock is shared by every repository in a scenario, so that the
// pruner and the clients agree on what time it is and a test can move
// all of them past the grace period at once. Every read advances it by
// a second, so two snapshots never collide.
type clock struct {
	mu sync.Mutex
	at time.Time
}

func newClock() *clock {
	return &clock{at: time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)}
}

func (c *clock) now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.at = c.at.Add(time.Second)
	return c.at
}

func (c *clock) advance(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.at = c.at.Add(d)
}

// scenario is a repository with one pruner and any number of clients on
// a shared clock.
type scenario struct {
	t     *testing.T
	dir   string
	clock *clock
	seq   int

	// sources remembers which directory each snapshot was taken of.
	sources map[string]string
}

func newScenario(t *testing.T) *scenario {
	t.Helper()
	s := &scenario{t: t, dir: filepath.Join(t.TempDir(), "repo"), clock: newClock(), sources: map[string]string{}}

	b, err := backend.CreateLocal(s.dir)
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}
	opts := s.options("pruner-init", "ffffffffffffffffffffffffffffffff")
	r, err := Init(context.Background(), b, opts)
	if err != nil {
		t.Fatalf("init: %v", err)
	}
	if err := r.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
	return s
}

func (s *scenario) options(seed, clientID string) Options {
	return Options{
		Password:    []byte(testPassword),
		ClientID:    clientID,
		StateDir:    s.t.TempDir(),
		CacheDir:    s.t.TempDir(),
		KDF:         cheapKDF(),
		NonceSource: crypto.DeterministicReader(seed),
		Now:         s.clock.now,
	}
}

// open opens the repository as clientID. Each open gets its own nonce
// stream, so two clients never produce the same pack for the same data
// -- which is what makes duplicate packs reproducible here.
func (s *scenario) open(clientID string) *Repository {
	s.t.Helper()
	s.seq++
	b, err := backend.OpenLocal(s.dir)
	if err != nil {
		s.t.Fatalf("open backend: %v", err)
	}
	r, err := Open(context.Background(), b, s.options(clientID+"-"+string(rune('a'+s.seq)), clientID))
	if err != nil {
		s.t.Fatalf("open as %s: %v", clientID, err)
	}
	s.t.Cleanup(func() {
		if err := r.Close(); err != nil {
			s.t.Errorf("close: %v", err)
		}
	})
	return r
}

func (s *scenario) pruner() *Repository { return s.open("ffffffffffffffffffffffffffffffff") }

func (s *scenario) backup(r *Repository, source string) snapshot.Handle {
	s.t.Helper()
	_, handle, err := r.Backup(context.Background(), []string{source}, BackupOptions{SpoolDir: s.t.TempDir()})
	if err != nil {
		s.t.Fatalf("backup as %s: %v", r.ClientID(), err)
	}
	s.sources[handle.Key] = source
	return handle
}

func (s *scenario) forget(r *Repository, handle snapshot.Handle) {
	s.t.Helper()
	if _, err := r.Forget(context.Background(), ForgetOptions{Keys: []string{handle.Key}}); err != nil {
		s.t.Fatalf("forget %s: %v", handle.Key, err)
	}
}

func (s *scenario) prune(r *Repository, opts PruneOptions) PruneReport {
	s.t.Helper()
	report, err := r.Prune(context.Background(), opts)
	if err != nil {
		s.t.Fatalf("prune: %v", err)
	}
	return report
}

// healthy asserts what every scenario must end with: check --read-data
// finds nothing, and every remaining snapshot restores byte for byte.
func (s *scenario) healthy() {
	s.t.Helper()
	r := s.pruner()
	ctx := context.Background()
	report, err := r.Check(ctx, CheckOptions{ReadData: true})
	if err != nil {
		s.t.Fatalf("check: %v", err)
	}
	if !report.OK() {
		s.t.Fatalf("check found problems: %v", report.Problems)
	}
	handles, err := r.Snapshots(ctx, "")
	if err != nil {
		s.t.Fatalf("snapshots: %v", err)
	}
	for _, h := range handles {
		source, ok := s.sources[h.Key]
		if !ok {
			s.t.Fatalf("snapshot %s has no source to compare with", h.Key)
		}
		target := filepath.Join(s.t.TempDir(), "restored")
		if _, err := r.Restore(ctx, h.Key, target, RestoreOptions{}); err != nil {
			s.t.Fatalf("restore %s: %v", h.Key, err)
		}
		compareTrees(s.t, source, filepath.Join(target, filepath.Base(source)))
	}
}

func (s *scenario) packs() int {
	s.t.Helper()
	b, err := backend.OpenLocal(s.dir)
	if err != nil {
		s.t.Fatalf("open backend: %v", err)
	}
	defer func() {
		if err := b.Close(); err != nil {
			s.t.Errorf("close: %v", err)
		}
	}()
	return countKeys(s.t, b, pack.Prefix)
}

func (s *scenario) marks() []crypto.ID {
	s.t.Helper()
	r := s.pruner()
	marks, _, err := r.listMarks(context.Background())
	if err != nil {
		s.t.Fatalf("list marks: %v", err)
	}
	return sortedIDs(marks)
}

func (s *scenario) source(name string, size int) string {
	s.t.Helper()
	dir := filepath.Join(s.t.TempDir(), name)
	writeTree(s.t, dir, []fileSpec{
		{path: "data.bin", data: randomBytes(s.t, name, size)},
		{path: "note.txt", data: []byte("source " + name + "\n")},
	})
	return dir
}

const (
	clientA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	clientB = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	grace   = 72 * time.Hour
)

// shortGrace makes the two phases observable in one test without a
// three-day sleep; the clock still has to be moved past it explicitly.
var shortGrace = PruneOptions{Grace: time.Hour}

func TestPruneMarksThenSweepsAfterTheGrace(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	h := s.backup(a, src)
	s.forget(a, h)

	p := s.pruner()
	first := s.prune(p, shortGrace)
	if len(first.Marked) != 1 || len(first.Deleted) != 0 || len(first.Held) != 0 {
		t.Fatalf("first run: marked %d deleted %d held %d, want 1 0 0", len(first.Marked), len(first.Deleted), len(first.Held))
	}

	// Too soon: held by age, not by any client.
	second := s.prune(p, shortGrace)
	if len(second.Held) != 1 || len(second.Deleted) != 0 || len(second.Marked) != 0 {
		t.Fatalf("second run: %+v", second)
	}

	// After the grace: the only client's last activity is its
	// registration, which predates the mark, so it still holds.
	s.clock.advance(2 * time.Hour)
	third := s.prune(p, shortGrace)
	if len(third.Held) != 1 || len(third.Deleted) != 0 {
		t.Fatalf("third run: %+v", third)
	}
	if third.Held[0].Reason != "client "+clientA+" has not been active since the mark" {
		t.Errorf("hold reason = %q", third.Held[0].Reason)
	}

	// The client backs up something else, so it has been active since
	// the mark. Now the pack goes, the mark stays for the next run, and
	// the index no longer names the pack.
	other := s.source("two", 100<<10)
	s.backup(a, other)
	fourth := s.prune(p, shortGrace)
	if len(fourth.Deleted) != 1 || fourth.BytesReclaimed == 0 {
		t.Fatalf("fourth run: %+v", fourth)
	}
	if s.packs() != 1 {
		t.Errorf("%d packs stored, want 1", s.packs())
	}
	if len(s.marks()) != 1 {
		t.Errorf("mark removed with the pack; it must outlive it")
	}
	if len(p.Index().Packs()) != 1 {
		t.Errorf("index names %d packs, want 1", len(p.Index().Packs()))
	}

	fifth := s.prune(p, shortGrace)
	if len(fifth.Unmarked) != 1 || len(s.marks()) != 0 {
		t.Fatalf("fifth run did not clear the orphaned mark: %+v", fifth)
	}
	s.healthy()
}

func TestPruneDryRunChangesNothing(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)
	h := s.backup(a, s.source("one", 100<<10))
	s.forget(a, h)

	report := s.prune(s.pruner(), PruneOptions{DryRun: true})
	if len(report.Marked) != 1 {
		t.Fatalf("dry run reported %+v", report)
	}
	if len(s.marks()) != 0 {
		t.Error("dry run wrote a mark")
	}
}

func TestPruneDoesNotRefreshAnExistingMark(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)
	s.forget(a, s.backup(a, s.source("one", 100<<10)))
	p := s.pruner()
	s.prune(p, shortGrace)
	before := s.marks()
	s.clock.advance(30 * time.Minute)
	s.prune(p, shortGrace)

	marks, _, err := p.listMarks(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if got := marks[before[0]]; time.Unix(0, got.MarkedNs).After(s.clock.at.Add(-30 * time.Minute)) {
		t.Errorf("mark was refreshed to %s", time.Unix(0, got.MarkedNs))
	}
}

func TestPruneRefusesAnUnhealthyRepository(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)
	s.backup(a, s.source("one", 100<<10))

	// Delete a tree out from under the snapshot.
	key := anyKey(t, a, "trees/")
	if err := a.Backend().Delete(context.Background(), key); err != nil {
		t.Fatal(err)
	}
	_, err := s.pruner().Prune(context.Background(), shortGrace)
	if !errors.Is(err, ErrUnhealthy) {
		t.Fatalf("prune on a repository missing a tree: err = %v, want ErrUnhealthy", err)
	}
	if n := s.packs(); n != 1 {
		t.Errorf("prune touched packs: %d left", n)
	}
}

func TestPruneForgetsAClientNotHeardFromInLong(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)
	s.forget(a, s.backup(a, s.source("one", 100<<10)))
	p := s.pruner()
	s.prune(p, shortGrace)

	s.clock.advance(2 * time.Hour)
	if r := s.prune(p, shortGrace); len(r.Held) != 1 {
		t.Fatalf("client still within its window: %+v", r)
	}
	s.clock.advance(10 * time.Hour) // past 10x grace since the client's last activity
	if r := s.prune(p, shortGrace); len(r.Deleted) != 1 {
		t.Fatalf("client forgotten: %+v", r)
	}
}

func TestPruneCleansJunkAndOrphanedMarks(t *testing.T) {
	s := newScenario(t)
	p := s.pruner()
	ctx := context.Background()
	for _, key := range []string{GCPrefix + "not-a-pack", gcKey(crypto.ID{1})} {
		if err := backend.PutBytesIfAbsent(ctx, p.Backend(), key, []byte("junk")); err != nil {
			t.Fatal(err)
		}
	}
	if err := p.mark(ctx, crypto.ID{2}, s.clock.now()); err != nil {
		t.Fatal(err)
	}
	report := s.prune(p, shortGrace)
	if len(report.Unmarked) != 1 || report.Unmarked[0] != (crypto.ID{2}) {
		t.Errorf("unmarked = %v, want the orphan", report.Unmarked)
	}
	if n := countKeys(t, p.Backend(), GCPrefix); n != 0 {
		t.Errorf("%d keys left under gc/, want 0", n)
	}
}

// Scenario A. A backup starts before the mark and its snapshot lands
// after it. The next run must find the pack live and keep it.
func TestPruneRaceSnapshotLandsAfterMark(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src)) // the pack exists, nothing refers to it

	client := s.open(clientA)
	p := s.pruner()
	var marked PruneReport
	backupHooks.afterMarks = func() {
		backupHooks.afterMarks = nil
		marked = s.prune(p, shortGrace)
	}
	defer func() { backupHooks.afterMarks = nil }()
	s.backup(client, src)
	if len(marked.Marked) != 1 {
		t.Fatalf("prune inside the backup marked %d packs, want 1", len(marked.Marked))
	}
	if s.packs() != 1 {
		t.Fatalf("the client uploaded again although it saw no mark: %d packs", s.packs())
	}
	// The backup noticed the mark at commit and removed it itself.
	if len(s.marks()) != 0 {
		t.Errorf("mark survived the commit of a backup that references the pack")
	}

	s.clock.advance(2 * time.Hour)
	second := s.prune(p, shortGrace)
	if len(second.Deleted) != 0 || len(second.Marked) != 0 || second.Live != 1 {
		t.Fatalf("second run: %+v", second)
	}
	if s.packs() != 1 {
		t.Errorf("packs %d, want 1", s.packs())
	}
	s.healthy()
}

// Scenario B. A backup is still in flight when the grace runs out. The
// sweep must hold the pack because the client has not been active since
// the mark.
func TestPruneRaceBackupInFlightAtSweep(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))

	client := s.open(clientA)
	p := s.pruner()
	var sweep PruneReport
	backupHooks.afterMarks = func() {
		backupHooks.afterMarks = nil
		s.prune(p, shortGrace)
		s.clock.advance(2 * time.Hour)
		sweep = s.prune(p, shortGrace)
	}
	defer func() { backupHooks.afterMarks = nil }()
	s.backup(client, src)

	if len(sweep.Deleted) != 0 || len(sweep.Held) != 1 {
		t.Fatalf("sweep during the backup: %+v", sweep)
	}
	if s.packs() != 1 {
		t.Fatalf("%d packs, want the one the in-flight backup relies on", s.packs())
	}
	s.healthy()

	// And now that the snapshot has landed, the pack is live and, the
	// commit having removed the mark, there is nothing left to do.
	if r := s.prune(p, shortGrace); r.Live != 1 || len(r.Deleted) != 0 || len(r.Marked) != 0 {
		t.Fatalf("after the backup: %+v", r)
	}
}

// Scenario C. A backup that starts after the mark sees it, uploads the
// pack's chunks again and removes the mark.
func TestPruneRaceRevival(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	p := s.pruner()
	s.prune(p, shortGrace)

	snap, h, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	s.sources[h.Key] = src
	if len(s.marks()) != 0 {
		t.Error("the mark survived a backup that referenced the pack")
	}
	if snap.Stats.PacksRevived != 1 || snap.Stats.PacksAdded != 1 || s.packs() != 2 {
		t.Errorf("revived %d added %d stored %d, want 1 1 2", snap.Stats.PacksRevived, snap.Stats.PacksAdded, s.packs())
	}
	s.healthy()

	// Two packs hold the same chunks now; the duplicate is reclaimed.
	s.prune(p, shortGrace)
	s.clock.advance(2 * time.Hour)
	s.backup(s.open(clientA), s.source("two", 10<<10))
	if r := s.prune(p, shortGrace); len(r.Deleted) != 1 {
		t.Fatalf("duplicate not reclaimed: %+v", r)
	}
}

// Scenario D. Two clients back up the same data at once and produce two
// packs of it. One snapshot is forgotten; the duplicate and nothing
// else is reclaimed; the survivor restores; a further backup uploads
// nothing.
func TestPruneRaceDuplicatePacksFromConcurrentBackups(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	// B's whole backup runs inside A's, after A has read the index and
	// before A uploads: the interleaving the S3 test gets by luck.
	a, b := s.open(clientA), s.open(clientB)
	var hb snapshot.Handle
	backupHooks.afterMarks = func() {
		backupHooks.afterMarks = nil
		hb = s.backup(b, src)
	}
	defer func() { backupHooks.afterMarks = nil }()
	ha := s.backup(a, src)
	if s.packs() != 2 {
		t.Fatalf("%d packs, want the duplicate pair", s.packs())
	}

	p := s.pruner()
	first := s.prune(p, shortGrace)
	if len(first.Marked) != 1 {
		t.Fatalf("with both snapshots present, marked %d, want the non-canonical duplicate", len(first.Marked))
	}
	s.forget(a, ha)
	if r := s.prune(p, shortGrace); len(r.Marked) != 0 || len(r.Unmarked) != 0 {
		t.Fatalf("forgetting one of two snapshots of the same data changed marks: %+v", r)
	}

	s.clock.advance(2 * time.Hour)
	s.backup(s.open(clientA), s.source("two", 10<<10))
	s.backup(s.open(clientB), s.source("three", 10<<10))
	sweep := s.prune(p, shortGrace)
	if len(sweep.Deleted) != 1 {
		t.Fatalf("sweep: %+v", sweep)
	}
	if s.packs() != 3 { // one of the pair, plus the two small ones
		t.Errorf("%d packs, want 3", s.packs())
	}

	if _, ok := s.sources[hb.Key]; !ok {
		t.Fatal("B's snapshot was not recorded")
	}
	s.healthy()

	again, _, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	if again.Stats.ChunksNew != 0 {
		t.Errorf("backup after prune uploaded %d chunks, want 0", again.Stats.ChunksNew)
	}
}

// Scenario E. A backup starts after the mark and runs in the instant
// between the sweep deciding to delete the pack and doing it. The
// re-upload is what keeps the data: the mark removal loses this race
// and must not matter.
func TestPruneRaceRevivalDuringSweep(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	p := s.pruner()
	s.prune(p, shortGrace)
	s.clock.advance(2 * time.Hour)
	s.backup(s.open(clientA), s.source("two", 10<<10)) // active since the mark

	pruneHooks.beforeDelete = func() {
		pruneHooks.beforeDelete = nil
		s.backup(s.open(clientA), src)
	}
	defer func() { pruneHooks.beforeDelete = nil }()
	sweep := s.prune(p, shortGrace)
	if len(sweep.Deleted) != 1 {
		t.Fatalf("sweep: %+v", sweep)
	}
	s.healthy()
}

// A backup role that cannot remove marks still backs up safely, because
// the re-upload does not depend on it.
func TestBackupSurvivesBeingUnableToUnmark(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("a read-only directory does not prevent deletion on Windows; the mechanism under test is POSIX")
	}
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	s.prune(s.pruner(), shortGrace)

	// Make the mark undeletable on the local backend.
	markPath := filepath.Join(s.dir, filepath.FromSlash(gcKey(s.marks()[0])))
	if err := os.Chmod(filepath.Dir(markPath), 0o555); err != nil {
		t.Fatal(err)
	}
	restore := func() {
		if err := os.Chmod(filepath.Dir(markPath), 0o755); err != nil {
			t.Errorf("restore permissions: %v", err)
		}
	}
	t.Cleanup(restore)

	var warnings []string
	snap, h, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{
		SpoolDir: t.TempDir(),
		Warnf:    func(f string, a ...any) { warnings = append(warnings, f) },
	})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	s.sources[h.Key] = src
	if len(warnings) == 0 {
		t.Error("no warning about the mark that could not be removed")
	}
	if snap.Stats.PacksAdded != 1 {
		t.Errorf("chunks were not re-uploaded: %+v", snap.Stats)
	}
	restore()
	s.healthy()
}

// A sweep that deleted the pack and crashed before rewriting the index
// leaves blobs naming a pack that is gone. The next run must rewrite the
// index before it removes the mark; removing the mark first would leave
// a window where a client trusts the stale blob and skips the upload.
func TestPruneRewritesTheIndexAfterACrashedSweep(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	p := s.pruner()
	s.prune(p, shortGrace)
	s.clock.advance(2 * time.Hour)

	// The crash state: pack gone, mark present, blobs untouched.
	packID := s.marks()[0]
	if err := p.Backend().Delete(context.Background(), pack.Key(packID)); err != nil {
		t.Fatal(err)
	}

	report := s.prune(p, shortGrace)
	if len(report.Unmarked) != 1 {
		t.Fatalf("orphaned mark not removed: %+v", report)
	}
	fresh := s.pruner()
	for _, id := range fresh.Index().Packs() {
		if id == packID {
			t.Fatalf("a fresh open still sees pack %s in the index after the mark was removed", packID)
		}
	}
	snap, h, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	s.sources[h.Key] = src
	if snap.Stats.PacksAdded == 0 {
		t.Error("the client trusted the stale index and uploaded nothing")
	}
	s.healthy()
}

// A mark placed during a backup, on a client that cannot remove marks.
// The commit-time revival fails; the snapshot lands referencing the
// pack; prune's own recompute is the last line and must find the pack
// live.
func TestPruneRaceMarkDuringBackupWithoutUnmarkPermission(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("a read-only directory does not prevent deletion on Windows; the mechanism under test is POSIX")
	}
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	p := s.pruner()
	gcDir := filepath.Join(s.dir, "gc")

	restore := func() {
		if err := os.Chmod(gcDir, 0o755); err != nil {
			t.Errorf("restore permissions: %v", err)
		}
	}
	backupHooks.afterMarks = func() {
		backupHooks.afterMarks = nil
		s.prune(p, shortGrace)
		if err := os.Chmod(gcDir, 0o555); err != nil {
			t.Fatal(err)
		}
	}
	defer func() { backupHooks.afterMarks = nil }()
	t.Cleanup(restore)

	var warnings []string
	snap, h, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{
		SpoolDir: t.TempDir(),
		Warnf:    func(f string, a ...any) { warnings = append(warnings, f) },
	})
	restore()
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	s.sources[h.Key] = src
	if len(warnings) == 0 || snap.Stats.PacksAdded != 0 {
		t.Fatalf("expected a warning and no upload; warnings %v, packs added %d", warnings, snap.Stats.PacksAdded)
	}
	if len(s.marks()) != 1 {
		t.Fatal("the mark should have survived: the client could not remove it")
	}

	s.clock.advance(2 * time.Hour)
	report := s.prune(p, shortGrace)
	if len(report.Deleted) != 0 || len(report.Unmarked) != 1 {
		t.Fatalf("prune after the backup: %+v", report)
	}
	if s.packs() != 1 {
		t.Errorf("%d packs, want 1", s.packs())
	}
	s.healthy()
}

// Backup credentials can write under clients/. Junk there must not stop
// prune, and must not be deleted either: an honest client's damaged
// record is its only protection while its first backup runs.
func TestPruneToleratesJunkClientRecords(t *testing.T) {
	s := newScenario(t)
	p := s.pruner()
	ctx := context.Background()
	if err := backend.PutBytesIfAbsent(ctx, p.Backend(), clientKey("cccccccccccccccccccccccccccccccc"), []byte("junk")); err != nil {
		t.Fatal(err)
	}
	report := s.prune(p, shortGrace)
	if len(report.UnreadableClients) != 1 {
		t.Errorf("unreadable clients = %v, want the junk record", report.UnreadableClients)
	}
	if n := countKeys(t, p.Backend(), ClientsPrefix); n != 1 {
		t.Errorf("%d keys under clients/, want the junk left in place", n)
	}
}

// A client whose clock runs ahead can look active since a mark it never
// saw. The skew margin holds the pack for it.
func TestPruneHoldsWithinTheClockSkew(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	p := s.pruner()
	s.prune(p, shortGrace)

	// Active 30 minutes after the mark, well after the grace by the
	// time of the sweep, but inside a one-hour skew.
	s.clock.advance(30 * time.Minute)
	s.backup(a, s.source("two", 10<<10))
	s.clock.advance(2 * time.Hour)

	held := s.prune(p, PruneOptions{Grace: time.Hour, ClockSkew: time.Hour})
	if len(held.Deleted) != 0 || len(held.Held) != 1 {
		t.Fatalf("with a one-hour skew: %+v", held)
	}
	swept := s.prune(p, PruneOptions{Grace: time.Hour, ClockSkew: time.Minute})
	if len(swept.Deleted) != 1 {
		t.Fatalf("with a one-minute skew: %+v", swept)
	}
	s.healthy()
}

// Assumption 4 of format.md §12, violated: a backup that runs longer
// than --forget-clients-after. The client deduplicated against a pack
// that was unmarked when it looked; the pack is marked, the client is
// forgotten, the pack is swept, and the snapshot commits pointing at
// chunks that are gone. The argument does not hold and the data is
// lost -- the documented limit. What is promised instead: the loss is
// visible to check, and the next prune refuses to touch anything.
func TestPruneRaceBackupLongerThanForgetClientsAfter(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src)) // the pack is dead but not yet marked

	client := s.open(clientA)
	p := s.pruner()
	var sweep PruneReport
	backupHooks.afterMarks = func() {
		backupHooks.afterMarks = nil
		s.prune(p, shortGrace) // marks the pack the backup is about to rely on
		s.clock.advance(11 * time.Hour)
		sweep = s.prune(p, shortGrace) // grace and 10x grace both passed: the client is forgotten
	}
	defer func() { backupHooks.afterMarks = nil }()
	s.backup(client, src)

	if len(sweep.Deleted) != 1 || len(sweep.Held) != 0 {
		t.Fatalf("sweep with the client forgotten: %+v", sweep)
	}

	report, err := p.Check(context.Background(), CheckOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if report.OK() {
		t.Fatal("check passed a snapshot whose pack was swept from under it")
	}
	t.Logf("check: %v", report.Problems)
	if _, err := p.Prune(context.Background(), shortGrace); !errors.Is(err, ErrUnhealthy) {
		t.Fatalf("prune after the loss: %v, want ErrUnhealthy", err)
	}
}
