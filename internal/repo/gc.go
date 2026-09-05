package repo

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/parity"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

// GCPrefix is where prune records the objects it intends to delete.
// Packs, trees and index blobs share the namespace: their names are all
// 32-byte content addresses and cannot collide.
const GCPrefix = "gc/"

// GCMarkMagic is the entire content of a gc mark: 8 fixed bytes that
// carry no information. "When was this marked" is the backend's
// modification time for the mark object; "what was marked" is the key.
// Content-free marks are what keep the backup role Put-only -- there is
// nothing to encrypt and nothing to forge.
var GCMarkMagic = []byte("KISTGC2\n")

// gcKey returns the mark key for an object.
func gcKey(id crypto.ID) string { return GCPrefix + id.String() }

// parseGCKey is the inverse of gcKey.
func parseGCKey(key string) (crypto.ID, error) {
	rest, ok := strings.CutPrefix(key, GCPrefix)
	if !ok {
		return crypto.ID{}, fmt.Errorf("%q is not under %s", key, GCPrefix)
	}
	return crypto.ParseID(rest)
}

// listMarks returns every mark in the repository with the time its mark
// object was last written (per the backend's clock, truncated to whole
// seconds), and any keys under GCPrefix that are not marks.
//
// Second precision is deliberate and is part of the format: S3 itself
// only reports whole seconds, its list and head precisions differ, and a
// local filesystem's mtime has nanoseconds the backend does not carry.
// Within the same second, "the object was rewritten" and "the mark was
// written" are indistinguishable; every consumer takes the safe side
// (prune does not delete, backup does not commit).
func (r *Repository) listMarks(ctx context.Context) (map[crypto.ID]time.Time, []string, error) {
	marks := make(map[crypto.ID]time.Time)
	var junk []string
	err := r.backend.List(ctx, GCPrefix, func(fi backend.FileInfo) error {
		// A key under gc/ that is not a mark is junk to be removed, not an
		// error to stop on: it cannot be trusted and it cannot be anything
		// the format put there.
		id, err := parseGCKey(fi.Key)
		if err != nil {
			junk = append(junk, fi.Key)
			return nil //nolint:nilerr // classified as junk above
		}
		marks[id] = fi.Modified.Truncate(time.Second)
		return nil
	})
	if err != nil {
		return nil, nil, fmt.Errorf("list gc marks: %w", err)
	}
	return marks, junk, nil
}

// listMarkedPacks is the cheap form of listMarks for a backup client.
func (r *Repository) listMarkedPacks(ctx context.Context) (map[crypto.ID]time.Time, error) {
	marks, _, err := r.listMarks(ctx)
	return marks, err
}

// mark writes a mark for an object. An existing mark is left alone: the
// older a mark, the sooner its object may go, and refreshing it on every
// run would mean the grace period never ends.
func (r *Repository) mark(ctx context.Context, id crypto.ID) error {
	if err := backend.PutBytesIfAbsent(ctx, r.backend, gcKey(id), GCMarkMagic); err != nil && !errors.Is(err, backend.ErrExists) {
		return fmt.Errorf("mark %s: %w", id, err)
	}
	return nil
}

// PruneOptions configure Prune.
type PruneOptions struct {
	// Grace is how long an object must have been marked before it is
	// deleted. Zero means DefaultGrace.
	Grace time.Duration

	// ForgetClientsAfter is how long a client may go without a snapshot
	// before prune stops waiting for it. Zero means thirty days, matching
	// the unified format's default.
	ForgetClientsAfter time.Duration

	// ClockSkew is how far apart the clocks of the pruner and a client
	// are allowed to be. Zero means DefaultClockSkew.
	//
	// The sweep compares a client's activity, stamped by the client's
	// clock, with a mark, stamped by the pruner's backend. A client whose
	// clock runs fast could otherwise look active since a mark it never
	// saw.
	ClockSkew time.Duration

	// DryRun reports what would happen and changes nothing.
	DryRun bool

	// Progressf receives one line per phase, for a human watching.
	Progressf func(format string, args ...any)
}

// DefaultClockSkew is the clock disagreement prune tolerates by default.
const DefaultClockSkew = time.Hour

// DefaultInactiveAfter is how long a client may go without a snapshot
// before prune stops holding deletions for it.
const DefaultInactiveAfter = 30 * 24 * time.Hour

func (o PruneOptions) progress(format string, args ...any) {
	if o.Progressf != nil {
		o.Progressf(format, args...)
	}
}

func (o PruneOptions) grace() time.Duration {
	if o.Grace > 0 {
		return o.Grace
	}
	return DefaultGrace
}

func (o PruneOptions) clockSkew() time.Duration {
	if o.ClockSkew > 0 {
		return o.ClockSkew
	}
	return DefaultClockSkew
}

func (o PruneOptions) forgetClientsAfter() time.Duration {
	if o.ForgetClientsAfter > 0 {
		return o.ForgetClientsAfter
	}
	return DefaultInactiveAfter
}

// HeldPack is a marked pack prune did not delete, and why.
type HeldPack struct {
	Pack   crypto.ID
	Reason string
}

// PruneReport is what one prune run did, or would do under DryRun.
type PruneReport struct {
	Stored int // packs in the repository
	Live   int // packs some snapshot resolves a chunk to

	Marked   []crypto.ID // packs marked by this run
	Unmarked []crypto.ID // marks removed: the pack is live again, or gone
	Deleted  []crypto.ID // packs deleted by this run
	Held     []HeldPack  // marked packs left for a later run

	// Locked packs were deleted as far as this repository is concerned
	// -- they no longer read -- but the storage retains their bytes, so
	// nothing was reclaimed. See backend.ErrLocked.
	Locked []crypto.ID

	BytesReclaimed uint64
}

// ErrUnhealthy means prune refused to run because something a snapshot
// needs is missing or unreadable. Prune on a broken repository is how
// broken becomes gone; check says what is wrong.
var ErrUnhealthy = errors.New("prune: repository is not healthy; run check")

// pruneHooks are set by tests to interleave a prune with other work at
// the two points where the interleaving matters. Nil in production.
var pruneHooks struct {
	afterLiveness func()
	beforeDelete  func()
}

// Prune reclaims packs no snapshot needs, in two phases separated by a
// grace period, with no lock.
//
// Phase 1 marks. A pack is dead when no chunk of any snapshot resolves
// to it -- resolves, not "is held by": two packs holding the same chunk
// are one live pack and one dead one, which is how the duplicates from
// concurrent backups get reclaimed. Dead packs get a mark; packs that
// are live again, or gone, lose theirs.
//
// Phase 2 sweeps. A pack is deleted only when all of these hold: it is
// marked; the mark is older than the grace period; this run found it
// dead; and every still-active client has a snapshot that started after
// the mark (plus clock skew). The last condition is the one that closes
// the race. A backup that started before the mark holds an index saying
// some chunk is stored, skips uploading it, and only later writes the
// snapshot that would have kept the pack alive. The mark cannot see that
// backup until its snapshot lands, but a client with no snapshot newer
// than the mark might have that backup in flight, and that is what holds
// the pack. A backup that starts after a mark sees it and treats the
// marked pack as absent (see backupRun.has); v2 backups never remove
// marks, so that stays true across the deletion itself.
//
// The index is rewritten, naming only what survived, BEFORE any deletion
// is visible, and the replacement blob supersedes every existing blob:
// a reader that still sees old and new together takes the new one's word
// for what exists.
//
// Both phases run in one call. The pack marked in phase 1 is held by
// its age in phase 2, so one run marks and the next run, after the
// grace, deletes.
func (r *Repository) Prune(ctx context.Context, opts PruneOptions) (PruneReport, error) {
	var report PruneReport
	now := r.now().UTC()

	// The order of the first listings is load-bearing. Snapshots are
	// listed before trailers are read and before liveness is walked, so
	// that everything decided below is decided against one consistent
	// set of snapshots: a snapshot that lands after the listing is either
	// from a backup that started after every mark considered here, or
	// from a client whose most recent listed snapshot predates the mark,
	// and either way its pack survives.
	opts.progress("listing gc marks")
	marks, junk, err := r.listMarks(ctx)
	if err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}
	staleBlobs, unusableBlobs, err := index.List(ctx, r.backend)
	if err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}
	handles, err := snapshot.List(ctx, r.backend, "")
	if err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}

	// The loaded index is what clients see; it is compared with the
	// stored packs below, and rewritten if it names one that is gone.
	if err := r.refreshIndex(ctx, func(string, ...any) {}); err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}

	opts.progress("reading pack trailers")
	ix, packs, err := index.Rebuild(ctx, r.backend, r.keys)
	if err != nil {
		return report, fmt.Errorf("%w: %w", ErrUnhealthy, err)
	}
	report.Stored = len(packs)
	stored := make(map[crypto.ID]struct{}, len(packs))
	for id := range packs {
		stored[id] = struct{}{}
	}

	opts.progress("walking %d snapshots", len(handles))
	live := make(map[crypto.ID]struct{})
	seenTrees := make(map[crypto.ID]struct{})
	pruneChunks := r.NewChunkSource()
	var problems []string
	problem := func(format string, args ...any) {
		problems = append(problems, fmt.Sprintf(format, args...))
	}
	for _, h := range handles {
		snap, err := snapshot.Load(ctx, r.backend, r.keys, h.Key)
		if err != nil {
			problem("snapshot %s: %v", h.Key, err)
			continue
		}
		r.walkTree(ctx, snap.Root, h.Key, ix, seenTrees, live, pruneChunks, problem)
	}
	if len(problems) > 0 {
		return report, fmt.Errorf("%w: %s", ErrUnhealthy, strings.Join(problems, "; "))
	}
	report.Live = len(live)
	if pruneHooks.afterLiveness != nil {
		pruneHooks.afterLiveness()
	}

	// Phase 1: mark the dead, unmark the living. Marks whose packs are
	// gone are dealt with last, after the index has stopped naming them.
	opts.progress("marking")
	for _, id := range sortedPackInfos(packs) {
		_, isLive := live[id]
		_, isMarked := marks[id]
		switch {
		case !isLive && !isMarked:
			report.Marked = append(report.Marked, id)
			if !opts.DryRun {
				if err := r.mark(ctx, id); err != nil {
					return report, fmt.Errorf("prune: %w", err)
				}
			}
		case isLive && isMarked:
			report.Unmarked = append(report.Unmarked, id)
			delete(marks, id)
			if !opts.DryRun {
				if err := r.remove(ctx, gcKey(id)); err != nil {
					return report, fmt.Errorf("prune: unmark %s: %w", id, err)
				}
			}
		}
	}

	// Phase 2: sweep what has been dead long enough, if nobody might
	// still be counting on it.
	opts.progress("sweeping")
	activity := lastActivity(handles)
	deleted := make(map[crypto.ID]struct{})
	for _, id := range sortedMarkAges(marks) {
		info, wasStored := packs[id]
		if !wasStored {
			continue
		}
		markedAt := marks[id]
		if hold := holdReason(markedAt, now, activity, opts); hold != "" {
			report.Held = append(report.Held, HeldPack{Pack: id, Reason: hold})
			continue
		}
		if opts.DryRun {
			report.Deleted = append(report.Deleted, id)
			deleted[id] = struct{}{}
			continue
		}
		if pruneHooks.beforeDelete != nil {
			pruneHooks.beforeDelete()
		}
		size := info.Size
		switch err := r.backend.Delete(ctx, pack.Key(id)); {
		case errors.Is(err, backend.ErrLocked):
			report.Locked = append(report.Locked, id)
		case err != nil:
			return report, fmt.Errorf("prune: delete pack %s: %w", id, err)
		default:
			report.Deleted = append(report.Deleted, id)
			report.BytesReclaimed += size
		}
		deleted[id] = struct{}{}
		if err := r.remove(ctx, parity.Key(id)); err != nil {
			return report, fmt.Errorf("prune: remove parity of %s: %w", id, err)
		}
	}

	// The index must stop naming what is gone: what this run deleted,
	// and anything a previous run deleted before it could get here. Only
	// once that is done may a mark whose pack is gone be removed. In the
	// other order a client could load a blob naming the pack, find no
	// mark, and skip an upload it needed to make.
	for id := range deleted {
		delete(packs, id)
	}
	for _, id := range r.index.Packs() {
		if _, wasStored := packs[id]; !wasStored {
			deleted[id] = struct{}{}
		}
	}
	if len(deleted) > 0 && !opts.DryRun {
		opts.progress("rewriting the index")
		if err := r.replaceIndex(ctx, staleBlobs, unusableBlobs, packs); err != nil {
			return report, fmt.Errorf("prune: %w", err)
		}
	}

	// A pack this run deleted keeps its mark until the next run: a client
	// that lists the marks in this very instant must still see it. Only
	// a mark whose pack was already gone when this run started goes now.
	opts.progress("removing marks of objects that are gone")
	for _, id := range sortedMarkAges(marks) {
		if _, wasStored := stored[id]; wasStored {
			continue
		}
		// The mark namespace is shared by packs, trees and index blobs
		// (docs/format.md §1): a mark whose pack is gone may still point
		// at a live tree or blob, and only a mark whose object is gone
		// everywhere may be removed.
		if r.anyObjectExists(ctx, id) {
			continue
		}
		report.Unmarked = append(report.Unmarked, id)
		if !opts.DryRun {
			if err := r.remove(ctx, gcKey(id)); err != nil {
				return report, fmt.Errorf("prune: unmark %s: %w", id, err)
			}
		}
	}
	for _, key := range junk {
		if !opts.DryRun {
			if err := r.remove(ctx, key); err != nil {
				return report, fmt.Errorf("prune: remove %s: %w", key, err)
			}
		}
	}

	// Parity whose pack is gone -- deleted by a run that died before
	// this point, or by hand -- goes the same way as an orphaned mark.
	var orphanedParity []string
	err = r.backend.List(ctx, parity.Prefix, func(fi backend.FileInfo) error {
		id, err := crypto.ParseID(fi.Key[len(parity.Prefix):])
		if err != nil {
			orphanedParity = append(orphanedParity, fi.Key)
			return nil //nolint:nilerr // not a parity key: junk to remove
		}
		if _, ok := packs[id]; !ok {
			orphanedParity = append(orphanedParity, fi.Key)
		}
		return nil
	})
	if err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}
	for _, key := range orphanedParity {
		if !opts.DryRun {
			if err := r.remove(ctx, key); err != nil {
				return report, fmt.Errorf("prune: remove %s: %w", key, err)
			}
		}
	}
	return report, nil
}

// anyObjectExists reports whether any object with this name exists, in
// any of the namespaces a gc mark can point at.
func (r *Repository) anyObjectExists(ctx context.Context, id crypto.ID) bool {
	for _, key := range []string{pack.Key(id), tree.Key(id), index.Key(id)} {
		if _, err := r.backend.Stat(ctx, key); err == nil {
			return true
		}
	}
	return false
}

// holdReason says why a marked pack may not be deleted now, or "" if it
// may.
func holdReason(markedAt, now time.Time, activity map[string]time.Time, opts PruneOptions) string {
	if age := now.Sub(markedAt); age < opts.grace() {
		return fmt.Sprintf("marked %s ago, grace is %s", age.Round(time.Second), opts.grace())
	}
	var waiting []string
	for client, at := range activity {
		if now.Sub(at) > opts.forgetClientsAfter() {
			continue // not heard from in so long that it is not waited for
		}
		if !at.After(markedAt.Add(opts.clockSkew())) {
			waiting = append(waiting, client)
		}
	}
	if len(waiting) > 0 {
		slices.Sort(waiting)
		return fmt.Sprintf("client %s has no snapshot newer than the mark", strings.Join(waiting, ", "))
	}
	return ""
}

// lastActivity returns, per client, when its most recent snapshot
// started. Every backup a client has in flight started after that
// moment. There is no client registry in v2: a client whose snapshots
// were all forgotten is inactive by definition, and its first backup is
// protected by the backup-side commit checks instead (docs/format.md
// §13.3).
func lastActivity(handles []snapshot.Handle) map[string]time.Time {
	activity := make(map[string]time.Time, len(handles))
	for _, h := range handles {
		if h.Time.After(activity[h.ClientID]) {
			activity[h.ClientID] = h.Time
		}
	}
	return activity
}

func sortedPackInfos(m map[crypto.ID]index.PackInfo) []crypto.ID {
	ids := make([]crypto.ID, 0, len(m))
	for id := range m {
		ids = append(ids, id)
	}
	slices.SortFunc(ids, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
	return ids
}

func sortedMarkAges(m map[crypto.ID]time.Time) []crypto.ID {
	ids := make([]crypto.ID, 0, len(m))
	for id := range m {
		ids = append(ids, id)
	}
	slices.SortFunc(ids, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
	return ids
}

// remove deletes a housekeeping object: an old index blob, a mark, junk.
//
// Storage that retains versions reports ErrLocked for every delete. For
// these objects the outcome asked for -- the key no longer reads -- holds
// all the same, and the retained bytes are nobody's concern here. Packs
// and snapshots are deleted directly, because for them the retention is
// worth reporting.
func (r *Repository) remove(ctx context.Context, key string) error {
	if err := r.backend.Delete(ctx, key); err != nil && !errors.Is(err, backend.ErrLocked) {
		return err
	}
	return nil
}

// replaceIndex writes one blob describing packs, then removes the blobs
// listed in stale and the keys in unusable, and installs the result as
// this repository's index.
//
// The new blob supersedes every existing blob, so a reader that sees the
// replacement land mid-listing takes its word for what exists, and a
// crash between writing it and removing the old ones leaves two blobs --
// the survivor wins by supersession rather than by merge order.
func (r *Repository) replaceIndex(ctx context.Context, stale []crypto.ID, unusable []string, packs map[crypto.ID]index.PackInfo) error {
	var fresh crypto.ID
	if len(packs) > 0 {
		// The replacement supersedes every blob that exists now, not only
		// the ones this run is about to remove: an overlap with a blob
		// another prune wrote concurrently is resolved in favour of the
		// newer view either way, and naming all of them is what makes the
		// phantom entries go away on the next rewrite.
		supersedes := slices.Clone(stale)
		if len(supersedes) > 0 {
			slices.SortFunc(supersedes, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
		}
		var err error
		if fresh, err = index.Save(ctx, r.backend, r.keys, packs, supersedes, r.nonceSource); err != nil {
			return err
		}
	}
	for _, id := range stale {
		if id == fresh {
			continue
		}
		if err := r.remove(ctx, index.Key(id)); err != nil {
			return fmt.Errorf("remove the old index blob %s: %w", id, err)
		}
	}
	for _, key := range unusable {
		if err := r.remove(ctx, key); err != nil {
			return fmt.Errorf("remove %s: %w", key, err)
		}
	}

	ix := index.New()
	for id, info := range packs {
		ix.AddPack(id, info.Entries)
	}
	r.index = ix
	return nil
}
