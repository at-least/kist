package repo

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

// A clock is shared by every repository in a scenario, so that the
// pruner and the clients agree on what time it is and a test can move
// all of them past the grace period at once. Every read advances it by
// a second, so two snapshots never collide.
type clock struct {
	mu sync.Mutex
	at time.Time
}

// newClock starts at the real wall clock: a v2 mark's age is the mark
// object's mtime as reported by the backend, so the scenario's time and
// the filesystem's have to agree at the start.
func newClock() *clock {
	return &clock{at: time.Now()}
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
		Replicas:    ptrUint8(0),
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
	summary, err := r.Backup(context.Background(), []string{source}, BackupOptions{SpoolDir: s.t.TempDir()})
	handle := summary.Handle
	if err != nil {
		s.t.Fatalf("backup as %s: %v", r.ClientID(), err)
	}
	s.sources[handle.Key] = source
	// The pre-delete revive check treats a same-second object as
	// rewritten (the safe side). Real mtimes here all fall in the same
	// second, so age the repo's data objects to keep the protocol
	// observable -- the data objects end up older than every mark and
	// touch, which is where the protocol writes them.
	ageAll(s.t, s.dir, 10)
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
		// The root tree's entries are named by the source's absolute
		// path, so the restore lands under the target by that full path.
		compareTrees(s.t, source, filepath.Join(target, source))
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
			s.t.Errorf("close backend: %v", err)
		}
	}()
	return countKeys(s.t, b, pack.Prefix)
}

func (s *scenario) marks() []crypto.ID {
	s.t.Helper()
	r := s.pruner()
	marks, stale, err := r.listMarks(context.Background())
	if err != nil {
		s.t.Fatalf("list marks: %v", err)
	}
	_ = stale
	return sortedMarkIDs(marks)
}

// sortedMarkIDs returns a mark set's IDs in a stable order.
func sortedMarkIDs(marks map[crypto.ID]time.Time) []crypto.ID {
	ids := make([]crypto.ID, 0, len(marks))
	for id := range marks {
		ids = append(ids, id)
	}
	slices.SortFunc(ids, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
	return ids
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
)

// ageAll sets every object's real mtime in the repo `secs` into the past,
// except under gc/ and touch/: a mark's mtime IS its age and a touch's
// mtime IS the revival signal, and both are written after the data
// objects they refer to. Aging them too would drop a mark into the same
// second as its object, where prune's safe-side comparisons (same second
// counts as rewritten / touched) can no longer tell the protocol state
// from a rewrite. The mark-then-sweep protocol compares REAL backend
// mtimes (the injected clock only drives snapshot timestamps), so a test
// that wants an object to be older than its mark must move the file times
// itself.
func ageAll(t *testing.T, dir string, secs int) {
	t.Helper()
	err := filepath.Walk(dir, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if info.IsDir() {
			return nil
		}
		rel, err := filepath.Rel(dir, path)
		if err != nil {
			return err
		}
		sep := string(filepath.Separator)
		if rel == "gc" || rel == "touch" || strings.HasPrefix(rel, "gc"+sep) || strings.HasPrefix(rel, "touch"+sep) {
			return nil
		}
		past := time.Now().Add(-time.Duration(secs) * time.Second)
		return os.Chtimes(path, past, past)
	})
	if err != nil {
		t.Fatalf("age objects: %v", err)
	}
}

// shortGrace makes the two phases observable in one test without a
// three-day sleep; the clock still has to be moved past it explicitly.
var shortGrace = PruneOptions{Grace: time.Hour, ClockSkew: DefaultClockSkew}

func TestPruneMarksThenSweepsAfterTheGrace(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	h := s.backup(a, src)
	s.forget(a, h)

	// Real mtimes must differ from the mark's: the pre-delete revive check
	// treats a same-second object as rewritten (the safe side).
	ageAll(s.t, s.dir, 10)
	p := s.pruner()
	first := s.prune(p, shortGrace)
	if len(first.Marked) != 1 || len(first.TreesMarked) != 1 || len(first.Deleted) != 0 || len(first.Held) != 0 {
		t.Fatalf("first run: marked %d/%d trees deleted %d held %d, want 1 1 0 0",
			len(first.Marked), len(first.TreesMarked), len(first.Deleted), len(first.Held))
	}

	// Too soon: held by age, not by any client. v3 holds the pack AND its
	// tree: both are garbage now that the only snapshot is forgotten.
	second := s.prune(p, shortGrace)
	if len(second.Held) != 2 || len(second.Deleted) != 0 || len(second.Marked) != 0 {
		t.Fatalf("second run: %+v", second)
	}

	// After the grace the pack and its tree go: the client's only snapshot
	// was forgotten, and there is no client registry -- a client with no
	// snapshots is not waited for. Its first backup is protected by the
	// backup-side commit gate instead, not by the sweep.
	s.clock.advance(2 * time.Hour)
	third := s.prune(p, shortGrace)
	if len(third.Deleted) != 1 || len(third.TreesDeleted) != 1 || third.BytesReclaimed == 0 {
		t.Fatalf("third run: %+v", third)
	}
	if s.packs() != 0 {
		t.Errorf("%d packs stored, want 0", s.packs())
	}
	// The marks of deleted objects outlive the deletion by one run: a
	// client listing the marks in the instant after the delete must still
	// see them (pack + tree).
	if len(s.marks()) != 2 {
		t.Fatal("mark removed with the object; it must outlive it")
	}
	if len(p.Index().Packs()) != 0 {
		t.Errorf("index names %d packs, want 0", len(p.Index().Packs()))
	}

	fourth := s.prune(p, shortGrace)
	if len(fourth.Unmarked) != 2 || len(s.marks()) != 0 {
		t.Fatalf("fourth run did not clear the orphaned marks (pack + tree): %+v", fourth)
	}
	s.healthy()
}

// A client whose newest snapshot predates the mark holds the pack: a
// backup it started before the mark could still be counting on the pack.
// That is the whole of the hold rule in v2 -- there is no registry.
func TestPruneHoldsForAClientWithNoSnapshotNewerThanTheMark(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))        // one's pack becomes dead
	s.backup(a, s.source("two", 10<<10)) // client active, but before the mark
	p := s.pruner()
	first := s.prune(p, shortGrace)
	if len(first.Marked) != 1 || len(first.TreesMarked) != 1 {
		t.Fatalf("first run: marked %v, trees %v, want the dead pack and its tree", first.Marked, first.TreesMarked)
	}

	s.clock.advance(2 * time.Hour)
	third := s.prune(p, shortGrace)
	if len(third.Held) != 2 || len(third.Deleted) != 0 {
		t.Fatalf("after the grace (pack + tree both held): %+v", third)
	}
	want := "client " + clientA + " has no snapshot newer than the mark plus clock skew (1h0m0s)"
	for _, held := range third.Held {
		if held.Reason != want {
			t.Errorf("hold reason = %q, want %q", held.Reason, want)
		}
	}

	// A snapshot newer than the mark releases the hold. Both objects go:
	// the tree has no touch signal, so nothing revived it.
	s.backup(a, s.source("three", 10<<10))
	fourth := s.prune(p, shortGrace)
	if len(fourth.Deleted) != 1 || len(fourth.TreesDeleted) != 1 {
		t.Fatalf("after the client became active: %+v", fourth)
	}
	s.healthy()
}

func TestPruneDryRunChangesNothing(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)
	h := s.backup(a, s.source("one", 100<<10))
	s.forget(a, h)

	report := s.prune(s.pruner(), PruneOptions{DryRun: true, ClockSkew: DefaultClockSkew})
	if len(report.Marked) != 1 || len(report.TreesMarked) != 1 {
		t.Fatalf("dry run reported %+v", report)
	}
	if len(s.marks()) != 0 {
		t.Error("dry run wrote a mark")
	}
}

// A mark's age is the backend's mtime for the mark object, and a mark is
// written with PutIfAbsent: refreshing it on every run would mean the
// grace period never ends.
func TestPruneDoesNotRefreshAnExistingMark(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)
	s.forget(a, s.backup(a, s.source("one", 100<<10)))
	p := s.pruner()
	s.prune(p, shortGrace)
	before := s.marks()

	markMtime := func() time.Time {
		fi, err := p.Backend().Stat(context.Background(), gcKey(before[0]))
		if err != nil {
			t.Fatal(err)
		}
		return fi.Modified
	}
	at := markMtime()

	// The pack is still held by age, so the run neither removed the mark
	// nor re-wrote it: mark() goes through PutIfAbsent and leaves an
	// existing mark's bytes -- and therefore its mtime -- alone. (The
	// backend reports mtime at whole seconds, so a rewrite inside the
	// same second would go unnoticed here; the PutIfAbsent rule itself is
	// pinned by the backend conformance suite.)
	s.clock.advance(30 * time.Minute)
	again := s.prune(p, shortGrace)
	if len(again.Held) != 2 || len(again.Marked) != 0 || len(again.Unmarked) != 0 {
		t.Fatalf("second run (pack + tree both held): %+v", again)
	}
	if got := markMtime(); !got.Equal(at) {
		t.Errorf("mark was rewritten: mtime %s -> %s", at, got)
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

// A client nobody has heard from within --forget-clients-after is not
// waited for, even though it has snapshots predating the mark.
func TestPruneForgetsAClientNotHeardFromInLong(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)
	s.forget(a, s.backup(a, s.source("one", 100<<10)))
	s.backup(a, s.source("two", 10<<10)) // the client's newest snapshot
	p := s.pruner()
	opts := PruneOptions{Grace: time.Hour, ForgetClientsAfter: 10 * time.Hour, ClockSkew: DefaultClockSkew}
	s.prune(p, opts)

	s.clock.advance(2 * time.Hour)
	if r := s.prune(p, opts); len(r.Held) != 2 {
		t.Fatalf("client still within its window (pack + tree both held): %+v", r)
	}
	s.clock.advance(10 * time.Hour) // past forget-clients-after since the client's last snapshot
	if r := s.prune(p, opts); len(r.Deleted) != 1 || len(r.TreesDeleted) != 1 {
		t.Fatalf("client forgotten: %+v", r)
	}
	s.healthy()
}

// A key under gc/ that parses as a content address is a mark whatever
// its bytes say (v2 marks are content-free); one whose pack is gone is
// an orphan and is removed, as is any key that is not a mark at all.
func TestPruneCleansJunkAndOrphanedMarks(t *testing.T) {
	s := newScenario(t)
	p := s.pruner()
	ctx := context.Background()
	if err := backend.PutBytesIfAbsent(ctx, p.Backend(), GCPrefix+"not-a-pack", []byte("junk")); err != nil {
		t.Fatal(err)
	}
	if err := p.mark(ctx, crypto.ID{1}); err != nil {
		t.Fatal(err)
	}
	if err := p.mark(ctx, crypto.ID{2}); err != nil {
		t.Fatal(err)
	}
	report := s.prune(p, shortGrace)
	if len(report.Unmarked) != 2 {
		t.Fatalf("unmarked = %v, want both orphans", report.Unmarked)
	}
	for _, id := range []crypto.ID{{1}, {2}} {
		if !slices.Contains(report.Unmarked, id) {
			t.Errorf("unmarked = %v, want %s among them", report.Unmarked, id)
		}
	}
	if n := countKeys(t, p.Backend(), GCPrefix); n != 0 {
		t.Errorf("%d keys left under gc/, want 0", n)
	}
}

// Scenario A. A backup starts before the mark and its snapshot lands
// after it. The mark is young at commit time, so the backup commits; it
// is Put-only and leaves the mark, and the next prune -- finding the
// pack live again -- removes it.
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
	if len(marked.Marked) != 1 || len(marked.TreesMarked) != 1 {
		t.Fatalf("prune inside the backup marked %d packs and %d trees, want 1 1",
			len(marked.Marked), len(marked.TreesMarked))
	}
	if s.packs() != 1 {
		t.Fatalf("the client uploaded again although it saw no mark: %d packs", s.packs())
	}
	// The backup is Put-only: both marks -- the pack's and the tree's --
	// survive the commit.
	if len(s.marks()) != 2 {
		t.Fatal("the backup removed a gc mark; backups never remove marks")
	}

	s.clock.advance(2 * time.Hour)
	second := s.prune(p, shortGrace)
	// Both are live again: the committed snapshot reaches the tree and
	// resolves its chunks into the pack, so both marks are cancelled.
	if len(second.Deleted) != 0 || len(second.Marked) != 0 || len(second.Unmarked) != 2 || second.Live != 1 {
		t.Fatalf("second run: %+v", second)
	}
	if s.packs() != 1 {
		t.Errorf("packs %d, want 1", s.packs())
	}
	s.healthy()
}

// Scenario B, v2. A backup is in flight when the grace runs out, and the
// client has no snapshots, so nothing holds the pack: the sweep deletes
// it. The backup's commit gate re-resolves every referenced chunk, finds
// the pack missing, and refuses -- no snapshot is written pointing at
// data that is gone. A re-run re-uploads and commits.
func TestPruneRaceBackupInFlightAtSweepRefusesToCommit(t *testing.T) {
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
	_, err := client.Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err == nil {
		t.Fatal("the backup committed although the pack it referenced was swept")
	}
	if len(sweep.Deleted) != 1 {
		t.Fatalf("sweep during the backup: %+v", sweep)
	}
	if s.packs() != 0 {
		t.Fatalf("%d packs, want 0", s.packs())
	}
	if handles, err := p.Snapshots(context.Background(), ""); err != nil != (len(handles) != 0) || len(handles) != 0 {
		t.Fatalf("snapshots after the refused commit: %v %v", handles, err)
	}

	// Re-running re-uploads everything and commits.
	summary, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("re-run: %v", err)
	}
	h := summary.Handle
	s.sources[h.Key] = src
	if summary.Report.PacksNew == 0 {
		t.Error("the re-run uploaded nothing")
	}
	s.healthy()
}

// Scenario C. A backup that starts after the mark sees it, re-uploads
// the marked pack's chunks into a pack of its own, and leaves the mark
// in place (backups are Put-only). The next prune finds the duplicate
// and reclaims it.
func TestPruneRaceRevival(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	p := s.pruner()
	s.prune(p, shortGrace)

	summaryA, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	h := summaryA.Handle
	if err != nil {
		t.Fatal(err)
	}
	s.sources[h.Key] = src
	// The backup is Put-only: both marks -- the pack's and the tree's --
	// survive (the tree was reused, so its fresh touch earned its keep).
	if len(s.marks()) != 2 {
		t.Fatal("a mark was removed by a backup; backups never remove marks")
	}
	// Both of the source's chunks (data.bin and note.txt) live in the
	// marked pack, so both miss: PacksRevived counts chunk-level
	// re-uploads out of marked packs.
	if summaryA.Report.PacksRevived != 2 || summaryA.Report.PacksNew != 1 || s.packs() != 2 {
		t.Errorf("revived %d added %d stored %d, want 2 1 2", summaryA.Report.PacksRevived, summaryA.Report.PacksNew, s.packs())
	}
	s.healthy()

	// Two packs hold the same chunks now; the duplicate is reclaimed
	// once the mark has aged past the grace and the client is active.
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

	again, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	if again.Report.ChunksNew != 0 {
		t.Errorf("backup after prune uploaded %d chunks, want 0", again.Report.ChunksNew)
	}
}

// Scenario E. A backup starts after the mark and runs in the instant
// between the sweep deciding to delete the pack and doing it. The
// re-upload is what keeps the data: the mark survives (backups are
// Put-only) and must not matter.
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
	// The pack went; the tree stayed: the revival backup overwrote the
	// tree's touch after the mark, so the sweep cancelled the tree's mark
	// instead of deleting out from under the snapshot that just landed.
	if len(sweep.Deleted) != 1 || len(sweep.Unmarked) != 1 || len(sweep.TreesDeleted) != 0 {
		t.Fatalf("sweep: %+v", sweep)
	}
	s.healthy()
}

// The grace period is also the longest a backup may run: anything past
// it may have had its objects marked, deleted and unmarked already,
// beyond any after-the-fact check. (A backup with no snapshots has no
// sweep-side protection; this gate is what keeps its first commit safe.)
func TestBackupRefusesToCommitAfterTheGracePeriod(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 100<<10)
	client := s.open(clientA)
	backupHooks.afterMarks = func() {
		backupHooks.afterMarks = nil
		s.clock.advance(DefaultGrace) // the backup is now older than the grace
	}
	defer func() { backupHooks.afterMarks = nil }()

	_, err := client.Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if !errors.Is(err, ErrBackupTooLong) {
		t.Fatalf("backup across the grace: err = %v, want ErrBackupTooLong", err)
	}
	if handles, err := client.Snapshots(context.Background(), ""); err != nil || len(handles) != 0 {
		t.Fatalf("a too-long backup committed: %v %v", handles, err)
	}
}

// A backup role holds no Delete permission in v2, so a backup cannot
// remove a mark even in principle. Make the whole gc/ prefix read-only
// -- stronger than the real permission boundary -- and the backup must
// not care: it re-uploads out of marked packs and never writes to gc/.
func TestBackupSurvivesBeingUnableToUnmark(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("a read-only directory does not prevent deletion on Windows; the mechanism under test is POSIX")
	}
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))
	s.prune(s.pruner(), shortGrace)

	gcDir := filepath.Join(s.dir, "gc")
	if err := os.Chmod(gcDir, 0o555); err != nil {
		t.Fatal(err)
	}
	restore := func() {
		if err := os.Chmod(gcDir, 0o755); err != nil {
			t.Errorf("restore permissions: %v", err)
		}
	}
	t.Cleanup(restore)

	summaryA, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	h := summaryA.Handle
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	s.sources[h.Key] = src
	if summaryA.Report.PacksNew != 1 {
		t.Errorf("chunks were not re-uploaded: %+v", summaryA.Snapshot.Stats)
	}
	if len(s.marks()) != 2 {
		t.Fatal("a mark did not survive; marks must, backups are Put-only")
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
	first := s.prune(p, shortGrace)
	s.clock.advance(2 * time.Hour)

	// The crash state: pack gone, mark present, blobs untouched. v3 also
	// marked the snapshot's tree; the pack's ID comes from the report so
	// the mark set's ordering cannot hand the tree's ID over here.
	packID := first.Marked[0]
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
	summaryA, err := s.open(clientA).Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	h := summaryA.Handle
	if err != nil {
		t.Fatal(err)
	}
	s.sources[h.Key] = src
	if summaryA.Report.PacksNew == 0 {
		t.Error("the client trusted the stale index and uploaded nothing")
	}
	s.healthy()
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
	// v3 holds the tree on the same rule as the pack: both marks predate
	// the client's newest snapshot plus the skew.
	if len(held.Deleted) != 0 || len(held.Held) != 2 {
		t.Fatalf("with a one-hour skew: %+v", held)
	}
	swept := s.prune(p, PruneOptions{Grace: time.Hour, ClockSkew: time.Minute})
	if len(swept.Deleted) != 1 || len(swept.TreesDeleted) != 1 {
		t.Fatalf("with a one-minute skew: %+v", swept)
	}
	s.healthy()
}

// V3-GC-5. A tree the pruner marked is revived when a later backup
// reuses it: the backup overwrites touch/<tree> after the mark, the
// sweep reads the fresh signal and cancels the mark instead of deleting,
// and the backup -- whose chunk re-uploads already sit in a pack of its
// own -- commits. The prune runs inside beforeIndex: the tree is stored
// and touched by then, but the snapshot is not, so the sweep must decide
// from the touch alone.
func TestPruneRevivesAMarkedTreeTouchedByALaterBackup(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	h := s.backup(a, src)
	s.forget(a, h)

	p := s.pruner()
	marked := s.prune(p, shortGrace)
	if len(marked.Marked) != 1 || len(marked.TreesMarked) != 1 {
		t.Fatalf("mark run: %+v", marked)
	}
	packID, treeID := marked.Marked[0], marked.TreesMarked[0]

	// The sweep needs marks older than the grace, but the backup may not
	// run longer than it: move the REAL mtimes instead of the shared
	// clock. Objects first, marks on top of them -- the order the
	// protocol writes them in. The touch is left alone: backup2's
	// overwriting Put below is what must out-rank the mark.
	ageObject := func(key string, secs int) {
		t.Helper()
		past := time.Now().Add(-time.Duration(secs) * time.Second)
		if err := os.Chtimes(filepath.Join(s.dir, key), past, past); err != nil {
			t.Fatal(err)
		}
	}
	ageObject(pack.Key(packID), 3*3600)
	ageObject(tree.Key(treeID), 3*3600)
	ageObject(gcKey(packID), 2*3600)
	ageObject(gcKey(treeID), 2*3600)

	// Opened after the marks: the client sees them and re-uploads the
	// marked pack's chunks into a pack of its own.
	client := s.open(clientA)
	var revival PruneReport
	backupHooks.beforeIndex = func() {
		backupHooks.beforeIndex = nil
		revival = s.prune(p, shortGrace)
	}
	defer func() { backupHooks.beforeIndex = nil }()
	summary, err := client.Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup2: %v", err)
	}
	s.sources[summary.Handle.Key] = src

	// The sweep saw no snapshot (this one was not committed yet) and an
	// expired mark on the tree -- and still kept the tree, on the
	// strength of its fresh touch. The pack had no such signal and went.
	// Held is empty: the pack this run just marked (backup2's own) is
	// recorded in Marked only, and first comes up for the age gate on
	// the next run.
	if len(revival.Unmarked) != 1 || len(revival.TreesDeleted) != 0 || len(revival.Deleted) != 1 || len(revival.Held) != 0 {
		t.Fatalf("revival run: %+v", revival)
	}
	if revival.Unmarked[0] != treeID || revival.Deleted[0] != packID {
		t.Fatalf("revival run unmarked %v deleted %v, want the tree revived and the pack swept", revival.Unmarked, revival.Deleted)
	}
	if summary.Report.PacksRevived != 2 || summary.Report.PacksNew != 1 {
		t.Errorf("backup2 re-uploaded out of the marked pack: %+v", summary.Report)
	}

	// The outlived marks clear on the next run, and what is left is the
	// new snapshot's data -- the revived tree included.
	if r := s.prune(p, shortGrace); len(r.Unmarked) != 2 {
		t.Fatalf("final run: %+v", r)
	}
	if n := countKeys(t, p.Backend(), tree.Prefix); n != 1 {
		t.Errorf("%d trees stored, want the revived one", n)
	}
	s.healthy()
}

// docs/format.md §10 (v3) makes compaction MANDATORY: when the effective
// (non-superseded) index blob count passes maxEffectiveIndexBlobs, prune
// must merge them into one. 65 one-file backups produce 65 blobs; the
// next prune must collapse them.
func TestPruneCompactsTheIndexPastTheThreshold(t *testing.T) {
	s := newScenario(t)
	a := s.open(clientA)

	for i := range maxEffectiveIndexBlobs + 1 {
		src := s.source("blob-"+strconv.Itoa(i), 1<<10)
		s.backup(a, src)
	}
	pre, _, err := index.List(context.Background(), a.Backend())
	if err != nil {
		t.Fatal(err)
	}
	if len(pre) <= maxEffectiveIndexBlobs {
		t.Fatalf("precondition: only %d effective blobs, want more than %d", len(pre), maxEffectiveIndexBlobs)
	}

	// Nothing was forgotten: the only trigger is the blob count itself.
	s.clock.advance(2 * time.Hour)
	if r := s.prune(a, shortGrace); len(r.Deleted) != 0 {
		t.Fatalf("nothing was forgotten; deleted %d objects", len(r.Deleted))
	}
	post, _, err := index.List(context.Background(), a.Backend())
	if err != nil {
		t.Fatal(err)
	}
	if len(post) != 1 {
		t.Fatalf("prune left %d effective index blobs, want 1 (compaction is mandatory)", len(post))
	}
}
