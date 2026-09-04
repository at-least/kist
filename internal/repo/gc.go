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
	"github.com/at-least/kist/internal/snapshot"
)

// GCPrefix is where prune records the packs it intends to delete.
//
// A mark is the only piece of state the two prune phases share, and the
// only signal a backup client gets that a pack it deduplicates against is
// on its way out. It is an object like any other -- sealed, with the key
// as AAD -- so that a mark cannot be forged or moved by anyone without
// the repository password.
const GCPrefix = "gc/"

// gcVersion is the mark object schema version.
const gcVersion = 1

// DefaultGrace is how long a pack stays marked before it can be deleted.
//
// It bounds how stale a running backup's view of the repository may be:
// a backup that opened before the mark and is still running when the
// grace runs out is held off by the client condition in prune, not by
// this number alone. Three days is long enough that a weekend outage of
// the maintenance job does not turn into a race.
const DefaultGrace = 72 * time.Hour

// ClientsPrefix is where clients announce themselves.
//
// A client writes its record once, before its first backup, and prune
// uses the record's time as the earliest moment that client could have
// had a backup in flight. Without it a client's first backup would be
// invisible to prune until its first snapshot landed, which is exactly
// the backup most likely to run for longer than the grace period.
const ClientsPrefix = "clients/"

// clientRecord is what a client's record holds.
type clientRecord struct {
	Version     uint64 `cbor:"v"`
	FirstSeenNs int64  `cbor:"first_seen"`
}

func clientKey(clientID string) string { return ClientsPrefix + clientID }

// register writes this client's record if it has none. The record is
// never updated: its time is a lower bound, and the snapshots supply the
// rest.
func (r *Repository) register(ctx context.Context, now time.Time) error {
	key := clientKey(r.clientID)
	encoded, err := crypto.Marshal(clientRecord{Version: gcVersion, FirstSeenNs: now.UnixNano()})
	if err != nil {
		return fmt.Errorf("register client: %w", err)
	}
	sealed, err := crypto.Seal(&r.keys.Meta, []byte(key), encoded, r.nonceSource)
	if err != nil {
		return fmt.Errorf("register client: %w", err)
	}
	if err := backend.PutBytesIfAbsent(ctx, r.backend, key, sealed); err != nil && !errors.Is(err, backend.ErrExists) {
		return fmt.Errorf("register client: %w", err)
	}
	return nil
}

// listClients returns when each registered client was first seen, and
// the keys under ClientsPrefix that are not readable records.
//
// An unreadable record is reported and skipped, never deleted and never
// fatal. Backup credentials can write under clients/, so a record that
// will not open may be an honest client's damaged record -- its only
// protection while its first backup runs -- or junk written to keep
// prune from running. Neither is a reason to stop, and the first is a
// reason not to delete.
func (r *Repository) listClients(ctx context.Context) (map[string]time.Time, []string, error) {
	seen := make(map[string]time.Time)
	var junk []string
	err := r.backend.List(ctx, ClientsPrefix, func(fi backend.FileInfo) error {
		clientID := strings.TrimPrefix(fi.Key, ClientsPrefix)
		sealed, err := backend.GetAll(ctx, r.backend, fi.Key)
		if err != nil {
			return err
		}
		encoded, err := crypto.Open(&r.keys.Meta, []byte(fi.Key), sealed)
		if err != nil {
			junk = append(junk, fi.Key)
			return nil //nolint:nilerr // reported as junk above
		}
		var rec clientRecord
		if err := crypto.Unmarshal(encoded, &rec); err != nil || rec.Version != gcVersion {
			junk = append(junk, fi.Key)
			return nil //nolint:nilerr // reported as junk above
		}
		seen[clientID] = time.Unix(0, rec.FirstSeenNs).UTC()
		return nil
	})
	if err != nil {
		return nil, nil, fmt.Errorf("list clients: %w", err)
	}
	return seen, junk, nil
}

// gcMark is what a mark object holds.
type gcMark struct {
	Version  uint64 `cbor:"v"`
	MarkedNs int64  `cbor:"marked"`
	By       string `cbor:"by"`
}

// gcKey returns the mark key for a pack.
func gcKey(id crypto.ID) string { return GCPrefix + id.String() }

// parseGCKey is the inverse of gcKey.
func parseGCKey(key string) (crypto.ID, error) {
	rest, ok := strings.CutPrefix(key, GCPrefix)
	if !ok {
		return crypto.ID{}, fmt.Errorf("%q is not under %s", key, GCPrefix)
	}
	return crypto.ParseID(rest)
}

// listMarks returns every mark in the repository, oldest first, with any
// keys under GCPrefix that are not marks.
func (r *Repository) listMarks(ctx context.Context) (map[crypto.ID]gcMark, []string, error) {
	marks := make(map[crypto.ID]gcMark)
	var junk []string
	err := r.backend.List(ctx, GCPrefix, func(fi backend.FileInfo) error {
		// A key under gc/ that is not a readable mark is junk to be
		// removed, not an error to stop on: it cannot be trusted and
		// it cannot be anything the format put there.
		id, err := parseGCKey(fi.Key)
		if err != nil {
			junk = append(junk, fi.Key)
			return nil //nolint:nilerr // classified as junk above
		}
		sealed, err := backend.GetAll(ctx, r.backend, fi.Key)
		if err != nil {
			return err
		}
		encoded, err := crypto.Open(&r.keys.Meta, []byte(fi.Key), sealed)
		if err != nil {
			junk = append(junk, fi.Key)
			return nil //nolint:nilerr // classified as junk above
		}
		var m gcMark
		if err := crypto.Unmarshal(encoded, &m); err != nil || m.Version != gcVersion {
			junk = append(junk, fi.Key)
			return nil //nolint:nilerr // classified as junk above
		}
		marks[id] = m
		return nil
	})
	if err != nil {
		return nil, nil, fmt.Errorf("list gc marks: %w", err)
	}
	return marks, junk, nil
}

// listMarkedPacks is the cheap form of listMarks for a backup client: it
// needs to know which packs are marked, not when or by whom, so it does
// not read the objects.
func (r *Repository) listMarkedPacks(ctx context.Context) (map[crypto.ID]struct{}, error) {
	marked := make(map[crypto.ID]struct{})
	err := r.backend.List(ctx, GCPrefix, func(fi backend.FileInfo) error {
		if id, err := parseGCKey(fi.Key); err == nil {
			marked[id] = struct{}{}
		}
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("list gc marks: %w", err)
	}
	return marked, nil
}

// mark writes a mark for a pack. An existing mark is left alone: the
// older a mark, the sooner its pack may go, and refreshing it on every
// run would mean the grace period never ends.
func (r *Repository) mark(ctx context.Context, id crypto.ID, now time.Time) error {
	key := gcKey(id)
	encoded, err := crypto.Marshal(gcMark{Version: gcVersion, MarkedNs: now.UnixNano(), By: r.clientID})
	if err != nil {
		return fmt.Errorf("mark %s: %w", id, err)
	}
	sealed, err := crypto.Seal(&r.keys.Meta, []byte(key), encoded, r.nonceSource)
	if err != nil {
		return fmt.Errorf("mark %s: %w", id, err)
	}
	if err := backend.PutBytesIfAbsent(ctx, r.backend, key, sealed); err != nil && !errors.Is(err, backend.ErrExists) {
		return fmt.Errorf("mark %s: %w", id, err)
	}
	return nil
}

// PruneOptions configure Prune.
type PruneOptions struct {
	// Grace is how long a pack must have been marked before it is
	// deleted. Zero means DefaultGrace.
	Grace time.Duration

	// ForgetClientsAfter is how long a client may go without a snapshot
	// before prune stops waiting for it. Zero means ten times Grace.
	//
	// A client that has not been heard from in that long is assumed to
	// have no backup in flight. If it does, that backup opened its view
	// of the repository more than ForgetClientsAfter ago.
	ForgetClientsAfter time.Duration

	// ClockSkew is how far apart the clocks of the pruner and a client
	// are allowed to be. Zero means DefaultClockSkew.
	//
	// The sweep compares a client's activity, stamped by the client's
	// clock, with a mark, stamped by the pruner's. A client whose clock
	// runs fast could otherwise look active since a mark it never saw.
	ClockSkew time.Duration

	// DryRun reports what would happen and changes nothing.
	DryRun bool

	// Progressf receives one line per phase, for a human watching.
	Progressf func(format string, args ...any)
}

// DefaultClockSkew is the clock disagreement prune tolerates by default.
const DefaultClockSkew = time.Hour

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
	return 10 * o.grace()
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

	// UnreadableClients are keys under clients/ that are not records.
	UnreadableClients []string

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
// dead; and every client prune still waits for has been active since
// the mark. The last condition is the one that closes the race. A
// backup that started before the mark holds an index saying the chunk
// is stored, skips uploading it, and only later writes the snapshot
// that would have kept the pack alive. The mark cannot see that backup
// until its snapshot lands, but its client's last activity -- its most
// recent snapshot, or its registration if it has none -- is then older
// than the mark, and that is what holds the pack. A client active
// since the mark started every backup it has in flight after the mark,
// and a backup that starts after a mark sees it and treats the pack as
// absent (see backupRun.has).
//
// The mark outlives the pack: phase 2 deletes the pack and leaves the
// mark, and the next run's phase 1 removes marks whose packs are gone.
// That is what makes "a backup that starts after the mark sees it"
// true across the deletion itself, when a client could otherwise list
// the marks in the instant between the pack going and its mark going,
// and go on trusting an index that still names the pack.
//
// Both phases run in one call. The pack marked in phase 1 is held by
// its age in phase 2, so one run marks and the next run, after the
// grace, deletes.
func (r *Repository) Prune(ctx context.Context, opts PruneOptions) (PruneReport, error) {
	var report PruneReport
	now := r.now().UTC()

	// The order of the first three listings is load-bearing. Snapshots
	// are listed before trailers are read and before liveness is
	// walked, so that everything decided below is decided against one
	// consistent set of snapshots: a snapshot that lands after the
	// listing is either from a backup that started after every mark
	// considered here, or from a client whose most recent listed
	// snapshot predates the mark, and either way its pack survives.
	opts.progress("listing gc marks")
	marks, junk, err := r.listMarks(ctx)
	if err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}
	staleBlobs, unusableBlobs, err := index.List(ctx, r.backend)
	if err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}
	clients, unreadableClients, err := r.listClients(ctx)
	if err != nil {
		return report, fmt.Errorf("prune: %w", err)
	}
	report.UnreadableClients = unreadableClients
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
		r.walkTree(ctx, snap.Root, h.Key, ix, seenTrees, live, problem)
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
	for _, id := range sortedIDs(packs) {
		_, isLive := live[id]
		_, isMarked := marks[id]
		switch {
		case !isLive && !isMarked:
			report.Marked = append(report.Marked, id)
			if !opts.DryRun {
				if err := r.mark(ctx, id, now); err != nil {
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
	activity := lastActivity(clients, handles)
	deleted := make(map[crypto.ID]struct{})
	for _, id := range sortedIDs(marks) {
		if _, stored := packs[id]; !stored {
			continue
		}
		m := marks[id]
		markedAt := time.Unix(0, m.MarkedNs).UTC()
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
		size := packSize(packs[id])
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
		if _, stored := packs[id]; !stored {
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
	opts.progress("removing marks of packs that are gone")
	for _, id := range sortedIDs(marks) {
		if _, wasStored := stored[id]; wasStored {
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
	return report, nil
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
		return fmt.Sprintf("client %s has not been active since the mark", strings.Join(waiting, ", "))
	}
	return ""
}

// lastActivity returns, per client, the later of its registration and
// its most recent snapshot. Every backup a client has in flight started
// after that moment.
func lastActivity(clients map[string]time.Time, handles []snapshot.Handle) map[string]time.Time {
	activity := make(map[string]time.Time, len(clients))
	for id, at := range clients {
		activity[id] = at
	}
	for _, h := range handles {
		if h.Time.After(activity[h.ClientID]) {
			activity[h.ClientID] = h.Time
		}
	}
	return activity
}

func packSize(entries []pack.Entry) uint64 {
	var n uint64
	for _, e := range entries {
		n += uint64(e.Length)
	}
	return n
}

func sortedIDs[V any](m map[crypto.ID]V) []crypto.ID {
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
// The new blob is written before the old ones are deleted. A crash in
// between leaves two blobs describing overlapping sets -- harmless,
// because loading merges them -- rather than a window with no index.
func (r *Repository) replaceIndex(ctx context.Context, stale []crypto.ID, unusable []string, packs map[crypto.ID][]pack.Entry) error {
	fresh := crypto.ID{}
	if len(packs) > 0 {
		var err error
		if fresh, err = index.Save(ctx, r.backend, r.keys, packs, r.nonceSource); err != nil {
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
	for id, entries := range packs {
		ix.AddPack(id, entries)
	}
	r.index = ix
	return nil
}
