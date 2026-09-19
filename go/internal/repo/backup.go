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
	"slices"
	"sort"
	"strings"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/chunker"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/source"
	"github.com/at-least/kist/internal/tree"
)

// A SourceSpec chooses what a backup walks. The zero value walks local
// paths, which is what Backup's path arguments mean.
type SourceSpec struct {
	// URL names a single remote source: sftp://[user@]host[:port]/path
	// or s3://bucket/prefix. The backup then has exactly one root, and
	// the paths argument must be exactly this URL: a remote locator is
	// opaque, so there is nothing to normalise or de-nest.
	URL string

	// Source injects an opened source directly, for tests and in-process
	// use, recording Locator as the snapshot root. URL is ignored when
	// Source is set, and so are the path arguments.
	Source  source.Source
	Locator []byte
}

// remote reports whether the spec names a source of its own rather than
// local paths.
func (s SourceSpec) remote() bool { return s.URL != "" || s.Source != nil }

// BackupOptions configure one backup run.
type BackupOptions struct {
	// Host is recorded in the snapshot as a human label. Empty means the
	// machine's hostname.
	Host string

	// User is recorded in the snapshot as a human label. Empty means the
	// name of the user running the backup.
	User string

	// SpoolDir is where partly built packs are staged. Empty means the
	// system temporary directory.
	SpoolDir string

	// Parity is how many Reed-Solomon parity shards to store beside each
	// pack, out of 16 data shards; 0 stores none. It is a property of
	// this client's backups, not of the repository: check reports how
	// many packs have parity and repairs the ones that do.
	Parity int

	// Warnf receives non-fatal problems: an unreadable file, a socket
	// that cannot be represented. A backup that skips something must say
	// so; silence would be a lie about what was saved.
	Warnf func(format string, args ...any)

	// GCGrace is the grace period this backup assumes for its commit
	// gate. It must match what prune uses: the gate's whole argument is
	// "nothing marked before grace may already be gone". Zero means
	// DefaultGrace, and the CLI flag forwards prune's setting.
	GCGrace time.Duration

	// Source says what to walk: local paths (the zero value), or one
	// remote sftp:// or s3:// URL.
	Source SourceSpec
}

func (o BackupOptions) warn(format string, args ...any) {
	if o.Warnf != nil {
		o.Warnf(format, args...)
	}
}

func (o BackupOptions) gcGrace() time.Duration {
	if o.GCGrace > 0 {
		return o.GCGrace
	}
	return DefaultGrace
}

// DefaultGrace is how long a gc mark must age before the marked object
// may be deleted, and how long a backup may run before it refuses to
// commit. It must exceed the longest backup anyone will run.
const DefaultGrace = 72 * time.Hour

// graceSafetyMargin is subtracted from the grace when deciding whether a
// backup has run too long to commit: the backup and prune hosts' clocks
// may differ by minutes, and the wrong side of that line is the one where
// a backup commits over a deletion.
const graceSafetyMargin = time.Hour

// ErrBackupTooLong means a backup ran for longer than the gc grace
// period and did not commit: anything it wrote may already have been
// marked, deleted, and unmarked, and no after-the-fact check could tell.
// Re-running the backup reuses everything it uploaded.
var ErrBackupTooLong = errors.New("backup ran longer than the gc grace period; no snapshot was written")

// ErrTreeMarked means a tree this backup reached carries a gc mark that
// was already expired, and the tree or its touch signal did not survive
// the window prune's single HEAD-to-DELETE round trip leaves. Committing
// would point a snapshot at data that may be gone; failing is safe, and
// a re-run reuses whatever was uploaded.
var ErrTreeMarked = errors.New("a tree reached by this backup is marked for deletion and may be gone; no snapshot was written")

// A BackupReport is what one backup run DID: chunk, pack and error
// counters that depend on GC state and dedup order. They are the run's
// report -- CLI output, --json, metrics -- and deliberately NOT in the
// snapshot: two clients can legitimately count differently, and an
// immutable object must not (format-v3-draft.md §9.1).
type BackupReport struct {
	// ChunksNew counts chunks this run put into packs.
	ChunksNew uint64
	// ChunksRead counts chunks whose data this run read back, plus the
	// ones it confirmed without reading.
	ChunksRead uint64
	// PacksNew counts packs this run finished and uploaded.
	PacksNew uint64
	// PacksRevived counts chunk-level re-uploads out of marked packs.
	PacksRevived uint64
	// BytesStored is how many bytes of chunks this run sealed.
	BytesStored uint64
	// Errors counts source items that could not be read and were skipped.
	Errors uint64
	// FilesReused counts files restored from the parent's metadata
	// without being read: the fast paths that can prove a file unchanged
	// (posix by the kernel's own bookkeeping, s3 by the source's etag)
	// carry its chunk list over and count one here. Files whose proof
	// does not exist -- sftp and generic sources -- are always re-read
	// and never counted (format-v3-draft.md §8.2).
	FilesReused uint64
}

// A BackupSummary is what a committed backup produced: the snapshot
// object, where it landed, and the run's report.
type BackupSummary struct {
	Snapshot *snapshot.Snapshot
	Handle   snapshot.Handle
	Report   BackupReport
}

// Backup walks paths and commits a snapshot.
//
// The order is forced by the format: chunks go into packs, packs are
// uploaded, trees are written bottom-up (new trees PutIfAbsent, reused
// trees touched, format-v3-draft.md §13.1), the index blob is written,
// and only then is the snapshot committed -- its .r1 replica first when
// the repository has replicas, so the primary is still the one commit
// point. Dying anywhere before that last step leaves objects nothing
// refers to -- wasted space that prune reclaims, never a repository that
// cannot be read.
//
// GC obligations (§13): this backup is strictly Put-only. Packs that
// carry a gc mark are not deduplicated against -- their chunks are
// uploaded again -- and no mark is ever removed. Before committing, the
// index is reloaded and every referenced chunk must resolve to a pack
// that exists and whose mark, if any, is young; and every reachable tree
// that carried an expired mark at some point must still exist with a
// touch signal newer than that mark.
func (r *Repository) Backup(ctx context.Context, paths []string, opts BackupOptions) (BackupSummary, error) {
	if len(paths) == 0 && !opts.Source.remote() {
		return BackupSummary{}, errors.New("backup: no paths given")
	}

	host := opts.Host
	if host == "" {
		name, err := os.Hostname()
		if err != nil {
			// A label, not a key: a backup must not fail because the
			// machine cannot say its own name.
			opts.warn("could not determine the hostname: %v", err)
			name = "unknown"
		}
		host = name
	}
	user := opts.User
	if user == "" {
		if u, err := currentUser(); err == nil {
			user = u
		}
	}

	started := r.now().UTC()
	// Marks before the index: a backup that lists gc/ after this point
	// sees every mark a prune had written, and does not deduplicate
	// against a marked pack.
	marksAtStart, err := r.listMarkedPacks(ctx)
	if err != nil {
		return BackupSummary{}, fmt.Errorf("backup: %w", err)
	}
	// The dedup view is ranked by the same marks (format.md §10): a
	// chunk held by both a marked and an unmarked pack resolves to the
	// unmarked one, so the backup reuses it instead of re-uploading a
	// copy prune is about to keep anyway.
	if err := r.refreshIndexRanked(ctx, opts.warn, func(id crypto.ID) bool {
		_, marked := marksAtStart[id]
		return marked
	}); err != nil {
		return BackupSummary{}, fmt.Errorf("backup: %w", err)
	}
	if backupHooks.afterMarks != nil {
		backupHooks.afterMarks()
	}

	b := &backupRun{
		repo:             r,
		opts:             opts,
		marked:           marksAtStart,
		uploaded:         make(map[crypto.ID]struct{}),
		referenced:       make(map[crypto.ID]struct{}),
		written:          make(map[crypto.ID]index.PackInfo),
		ownPacks:         make(map[crypto.ID]struct{}),
		hard:             make(map[hardLinkKey]fileContents),
		seenTrees:        make(map[crypto.ID]struct{}),
		countedHardlinks: make(map[hardLinkKey]struct{}),
		started:          started,
	}
	defer b.abort()

	// Roots before the walk: what to read, and from where. A remote URL
	// opens its connection here, and owns closing it.
	plans, err := b.planRoots(ctx, paths)
	if err != nil {
		return BackupSummary{}, err
	}
	defer b.closeSources()

	// The newest snapshot of this client's with the same roots is the
	// parent: the fast path's reference point. Its start time is the
	// racy guard's line -- files modified at or after it are re-read no
	// matter how unchanged they look.
	locators := make([][]byte, len(plans))
	for i, plan := range plans {
		locators[i] = plan.locator
	}
	parentKey, parent, err := r.findParent(ctx, locators)
	if err != nil {
		return BackupSummary{}, err
	}
	if parent != nil {
		b.parentStartNs = parent.TimeNs
		b.parentRoots = make(map[string]crypto.ID, len(parent.Roots))
		for _, root := range parent.Roots {
			b.parentRoots[string(root.Path)] = root.Tree
		}
	}

	snapRoots, err := b.backupRoots(ctx, plans)
	if err != nil {
		return BackupSummary{}, err
	}

	if err := b.flush(ctx); err != nil {
		return BackupSummary{}, err
	}

	// The index blob is a cache, so it is written before the commit but a
	// failure to write it is still fatal: a backup that cannot record
	// where it put things has not finished.
	if len(b.written) > 0 {
		if _, err := index.Save(ctx, r.backend, r.keys, b.written, nil, r.nonceSource); err != nil {
			return BackupSummary{}, fmt.Errorf("backup: %w", err)
		}
	}
	if backupHooks.beforeIndex != nil {
		hook := backupHooks.beforeIndex
		backupHooks.beforeIndex = nil
		hook()
	}

	if err := b.verifyBeforeCommit(ctx); err != nil {
		return BackupSummary{}, fmt.Errorf("backup: %w", err)
	}

	var parentPtr *string
	if parentKey != "" {
		parentPtr = &parentKey
	}
	snap := &snapshot.Snapshot{
		Version:  snapshot.Version,
		Roots:    snapRoots,
		TimeNs:   started.UnixNano(),
		Host:     host,
		User:     user,
		ClientID: r.clientID,
		Parent:   parentPtr,
		Stats:    b.stats,
	}
	handle, err := snap.Save(ctx, r.backend, r.keys, r.nonceSource, int(r.config.Replicas))
	if err != nil {
		return BackupSummary{}, fmt.Errorf("backup: %w", err)
	}
	return BackupSummary{Snapshot: snap, Handle: handle, Report: b.report}, nil
}

// backupHooks are set by tests to interleave a backup with a prune at
// the points where the interleaving matters. Nil in production.
var backupHooks struct {
	afterMarks func()
	// beforeIndex runs after the walk and the pack flush, before the
	// index blob is written: every tree is stored and touched by then,
	// but the snapshot -- the commit -- is not.
	beforeIndex func()
}

// hardLinkKey identifies one inode, so that a file reachable by several
// names is stored once.
type hardLinkKey struct {
	device uint64
	inode  uint64
}

// fileContents is what a hard link's later names reuse.
type fileContents struct {
	size   uint64
	chunks []crypto.ID
	ctype  tree.ContentType
}

type backupRun struct {
	repo *Repository
	opts BackupOptions

	writer *pack.Writer

	// chunker is reused across files: its 16 MiB buffer is the single
	// largest per-file cost when files are small.
	chunker *chunker.Chunker

	// marked is the set of packs prune has marked for deletion, as of
	// the start of this backup, with the age of each mark. See has.
	marked map[crypto.ID]time.Time

	// referenced is the set of packs this run deduplicated against: it
	// refers to their chunks without holding a copy.
	referenced map[crypto.ID]struct{}

	// uploaded holds every chunk this run has put into a pack, finished
	// or not.
	//
	// The index only learns about a chunk when its pack is finished, so
	// without this a payload repeated inside one pack's worth of work --
	// two identical files, a duplicated directory -- would be added
	// twice. That produces a trailer listing one chunk twice, which fails
	// the trailer's own consistency check and makes the pack unreadable.
	// It is never cleared, because a chunk this run re-uploaded out of a
	// marked pack still resolves to the marked pack in the index and
	// would otherwise be re-uploaded on every repeat.
	uploaded map[crypto.ID]struct{}

	written  map[crypto.ID]index.PackInfo
	ownPacks map[crypto.ID]struct{}
	hard     map[hardLinkKey]fileContents

	// seenTrees is every tree this run reached, whether it wrote it or
	// found it already there. The commit gate checks the ones with marks.
	seenTrees map[crypto.ID]struct{}

	// countedHardlinks is the set of (dev,ino) groups whose bytes are
	// already in the stats: content is counted once per group, across
	// roots (format-v3-draft.md §9.1).
	countedHardlinks map[hardLinkKey]struct{}

	// parentStartNs is when the parent snapshot's backup started, in UTC
	// nanoseconds: the racy guard's line. Zero means no parent.
	parentStartNs int64

	// parentRoots maps the parent snapshot's root locators to their
	// trees. Only roots whose locator matches a root of this backup are
	// consulted.
	parentRoots map[string]crypto.ID

	// sources are the ones this run opened itself (from a URL) and must
	// close when the run ends. Injected sources belong to their caller.
	sources []source.Source

	stats   snapshot.Stats
	report  BackupReport
	started time.Time
}

// normalisePaths turns the caller's paths into absolute, deduplicated,
// sorted roots, as byte strings. A path nested inside another root is
// dropped with a warning: backed up twice it would also be restored
// twice, into the same place.
func normalisePaths(paths []string) ([][]byte, error) {
	seen := make(map[string]struct{}, len(paths))
	var strs []string

	for _, p := range paths {
		abs, err := filepath.Abs(p)
		if err != nil {
			return nil, fmt.Errorf("backup: resolve %s: %w", p, err)
		}
		abs = filepath.Clean(abs)
		if _, dup := seen[abs]; dup {
			continue
		}
		seen[abs] = struct{}{}
		strs = append(strs, abs)
	}
	sort.Strings(strs)

	var kept []string
	for _, s := range strs {
		if nested := slices.IndexFunc(kept, func(outer string) bool {
			return s == outer || strings.HasPrefix(s, outer+string(filepath.Separator))
		}); nested >= 0 {
			continue
		}
		kept = append(kept, s)
	}
	out := make([][]byte, len(kept))
	for i, s := range kept {
		out[i] = []byte(s)
	}
	return out, nil
}

// A rootPlan is one backup source, resolved before the walk: the
// locator the snapshot will record, and the Source to read it from.
type rootPlan struct {
	locator []byte
	src     source.Source

	// fileRoot marks a local root that IS a file or symlink, not a
	// directory: its root tree carries the one entry. The file's own
	// path is localPath; the source is rooted at its parent directory.
	fileRoot  bool
	localPath string
}

// planRoots resolves the backup's sources. Local paths keep the
// normalisation they always had -- absolute, deduplicated, sorted, with
// nested paths dropped -- while a remote URL or an injected source is a
// single opaque root.
func (b *backupRun) planRoots(ctx context.Context, paths []string) ([]rootPlan, error) {
	switch {
	case b.opts.Source.Source != nil:
		// An injected source: the locator is given, the paths ignored.
		return []rootPlan{{
			locator: slices.Clone(b.opts.Source.Locator),
			src:     b.opts.Source.Source,
		}}, nil

	case b.opts.Source.URL != "":
		url := b.opts.Source.URL
		if len(paths) != 1 || paths[0] != url {
			return nil, fmt.Errorf("backup: a remote source URL backs up exactly one root: pass only %s as the path", url)
		}
		src, err := source.OpenSource(ctx, url)
		if err != nil {
			return nil, fmt.Errorf("backup: %w", err)
		}
		b.sources = append(b.sources, src)
		return []rootPlan{{locator: []byte(url), src: src}}, nil

	default:
		roots, err := normalisePaths(paths)
		if err != nil {
			return nil, err
		}
		out := make([]rootPlan, 0, len(roots))
		for _, root := range roots {
			path := string(root)
			info, err := os.Lstat(path)
			if err != nil {
				return nil, fmt.Errorf("backup: %w", err)
			}
			if info.IsDir() {
				out = append(out, rootPlan{locator: root, src: source.NewLocalSourceAt(path, path)})
				continue
			}
			// A file or symlink source: the root tree carries the one
			// entry, and the source is rooted at the file's parent so the
			// entry's rel path is its name.
			out = append(out, rootPlan{
				locator:   root,
				src:       source.NewLocalSourceAt(filepath.Dir(path), filepath.Dir(path)),
				fileRoot:  true,
				localPath: path,
			})
		}
		return out, nil
	}
}

// closeSources releases the sources this run opened. Errors are
// warnings: the backup's outcome is already decided by its writes.
func (b *backupRun) closeSources() {
	for _, src := range b.sources {
		if err := src.Close(); err != nil {
			b.opts.warn("closing source: %v", err)
		}
	}
	b.sources = nil
}

// backupRoots produces one snapshot root per source plan.
//
// There is no synthetic root in v3: each source's directory CONTENTS
// become that root's tree, and entry names are always single path
// components. A local file or symlink yields a root tree with exactly
// one entry, named by the source path's last component -- and so does a
// remote source whose root names a single file, which the root listing
// detects: exactly one entry, a file, named like the locator's last
// component. Any other listing is walked as a directory. The probe
// failing at all means the source cannot be entered, which is not one
// item's problem: that fails the backup.
func (b *backupRun) backupRoots(ctx context.Context, plans []rootPlan) ([]snapshot.Root, error) {
	out := make([]snapshot.Root, 0, len(plans))

	for _, plan := range plans {
		id, err := b.backupRoot(ctx, plan)
		if err != nil {
			return nil, err
		}
		out = append(out, snapshot.Root{Path: slices.Clone(plan.locator), Tree: id})
	}
	return out, nil
}

// backupRoot walks one source and returns its root tree ID.
func (b *backupRun) backupRoot(ctx context.Context, plan rootPlan) (crypto.ID, error) {
	if plan.fileRoot {
		// lstat the file directly: listing its whole parent would make a
		// huge directory's other entries this root's problem.
		name := lastComponent(plan.locator)
		if len(name) == 0 {
			return crypto.ID{}, fmt.Errorf("backup: cannot derive a name for source path %s", plan.localPath)
		}
		item, ok, err := b.localFileItem(plan.localPath, name)
		if err != nil {
			return crypto.ID{}, err
		}
		if !ok {
			return crypto.ID{}, fmt.Errorf("backup: %s: source could not be read", plan.localPath)
		}
		entry, ok, err := b.processEntry(ctx, plan.src, name, item, b.parentFileEntry(ctx, plan.locator, name))
		if err != nil {
			return crypto.ID{}, err
		}
		if !ok {
			return crypto.ID{}, fmt.Errorf("backup: %s: source could not be read", plan.localPath)
		}
		return b.writeSegment(ctx, []tree.Entry{entry}, nil)
	}

	if plan.src == nil {
		return crypto.ID{}, errors.New("backup: source plan has no source")
	}
	probe, err := plan.src.List(ctx, nil)
	if err != nil {
		return crypto.ID{}, fmt.Errorf("backup: %w", err)
	}
	// File-source rule: exactly one file entry, named like the locator's
	// own last component. The same shape restore and the mount detect,
	// so the three mappings can never disagree about where a root's
	// contents land.
	last := string(lastComponent(plan.locator))
	if len(last) > 0 && len(probe) == 1 &&
		probe[0].Kind.Kind == source.KindFile && string(probe[0].Name) == last {
		entry, ok, err := b.processEntry(ctx, plan.src, probe[0].Name, probe[0], b.parentFileEntry(ctx, plan.locator, probe[0].Name))
		if err != nil {
			return crypto.ID{}, err
		}
		if !ok {
			return crypto.ID{}, fmt.Errorf("backup: %s: source could not be read", plan.locator)
		}
		return b.writeSegment(ctx, []tree.Entry{entry}, nil)
	}
	return b.walkDir(ctx, plan.src, nil, b.parentSubtree(plan.locator))
}

// parentSubtree returns the parent snapshot's tree for the root with
// this locator, or nil when there is no parent for it.
func (b *backupRun) parentSubtree(locator []byte) *crypto.ID {
	if id, ok := b.parentRoots[string(locator)]; ok {
		return &id
	}
	return nil
}

// parentFileEntry finds the parent snapshot's entry for a file root:
// the file of that name in the same locator's root tree. Reading it is
// the fast path's business; failing costs a re-read, never the backup.
func (b *backupRun) parentFileEntry(ctx context.Context, locator, name []byte) *tree.Entry {
	subtree := b.parentSubtree(locator)
	if subtree == nil {
		return nil
	}
	stream, err := openParentStream(ctx, b.repo, *subtree)
	if err != nil {
		b.opts.warn("cannot read parent tree %s: %v; re-reading", *subtree, err)
		return nil
	}
	return stream.takeName(ctx, name)
}

// localFileItem builds the SourceItem for a local file or symlink root
// by stat'ing the path itself. The bool reports whether the item could
// be built at all; false with a nil error means it was skipped with a
// warning.
func (b *backupRun) localFileItem(path string, name []byte) (source.SourceItem, bool, error) {
	info, err := os.Lstat(path)
	if err != nil {
		return source.SourceItem{}, false, fmt.Errorf("backup: %w", err)
	}

	item := source.SourceItem{Name: slices.Clone(name)}
	switch {
	case info.Mode()&fs.ModeSymlink != 0:
		target, err := os.Readlink(path)
		if err != nil {
			b.skip("skipping %s: read link: %v", path, err)
			return source.SourceItem{}, false, nil
		}
		item.Kind = source.SourceItemKind{Kind: source.KindSymlink, Target: []byte(target)}
	default:
		item.Kind = source.SourceItemKind{
			Kind:    source.KindFile,
			Size:    uint64(max(info.Size(), 0)), //nolint:gosec // a length is never negative
			MTimeNs: info.ModTime().UnixNano(),
		}
	}
	meta := source.CapturePosix(info)
	item.Posix = &meta
	return item, true, nil
}

// lastComponent returns the bytes after the final path separator, as the
// entry name of a file or symlink source.
func lastComponent(path []byte) []byte {
	if i := bytes.LastIndexByte(path, '/'); i >= 0 {
		return path[i+1:]
	}
	return path
}

// fillPosixMeta records the posix fields (format-v3-draft.md §8.1):
// mode/uid/gid/mtime are required -- uid 0 is root, a real value -- the
// change time is recorded when the platform has one, and the hard-link
// identity only for a file with more than one name.
func fillPosixMeta(entry *tree.Entry, posix *source.PosixMeta, isFile bool) {
	entry.Mode = tree.Ptr(posix.Mode)
	entry.UID = tree.Ptr(posix.UID)
	entry.GID = tree.Ptr(posix.GID)
	entry.MTimeNs = tree.Ptr(posix.MTimeNs)
	if posix.CTimeNs != 0 {
		entry.CTimeNs = tree.Ptr(posix.CTimeNs)
	}
	if isFile && posix.NLink > 1 {
		entry.Device = tree.Ptr(posix.Dev)
		entry.Inode = tree.Ptr(posix.Inode)
		entry.Links = tree.Ptr(posix.NLink)
	}
}

// displayPath renders a relative source path for warnings and errors:
// the full local path when the source is local, the relative bytes
// otherwise.
func displayPath(src source.Source, rel []byte) string {
	if path, ok := source.LocalPathOf(src, rel); ok {
		return path
	}
	return string(rel)
}

// walkDir backs up one directory's CONTENTS and returns the tree ID of
// its last segment. v3 has no synthetic root, so a source root's
// contents are walked through here with an empty relative path.
//
// The source's listing is already sorted by name, which is what lets the
// parent snapshot's entries be consulted with a merge-join instead of a
// map. Entries a directory gains, loses or renames between listings are
// just names the parent does not have: they are read, and the parent's
// leftovers are never asked for.
func (b *backupRun) walkDir(ctx context.Context, src source.Source, dir []byte, parentSubtree *crypto.ID) (crypto.ID, error) {
	var stream *parentStream
	if parentSubtree != nil && !parentSubtree.IsZero() {
		s, err := openParentStream(ctx, b.repo, *parentSubtree)
		if err != nil {
			b.opts.warn("cannot read parent tree %s: %v; re-reading this directory", *parentSubtree, err)
		} else {
			stream = s
		}
	}

	display := displayPath(src, dir)
	items, err := src.List(ctx, dir)
	if err != nil {
		// A directory that cannot be listed is stored empty: one
		// directory's problem must not abandon the rest of the source.
		b.skip("reading %s: %v; storing it empty", display, err)
		return b.saveTreeSegments(ctx, nil)
	}

	var (
		children []tree.Entry
		prev     *crypto.ID
	)
	for _, item := range items {
		if err := ctx.Err(); err != nil {
			return crypto.ID{}, err
		}
		childRel := source.JoinRel(dir, item.Name)
		entry, ok, err := b.processEntry(ctx, src, childRel, item, stream.takeName(ctx, item.Name))
		if err != nil {
			return crypto.ID{}, err
		}
		if !ok {
			continue
		}
		children = append(children, entry)
		if len(children) >= tree.MaxNodesPerTree {
			// Huge directories are written in bounded pieces: the entries
			// of one segment, not the whole directory, are what stays in
			// memory (the large-repository memory gate).
			part := children
			children = nil
			id, err := b.writeSegment(ctx, part, prev)
			if err != nil {
				return crypto.ID{}, err
			}
			prev = &id
		}
	}
	return b.saveTreeSegmentsFrom(ctx, children, prev)
}

// processEntry produces the tree entry for one listed item, recursing
// into directories. The bool reports whether the entry should be
// included; false with a nil error means it was skipped with a warning.
//
// The entry records only what the source's metadata kind can carry
// (format-v3-draft.md §8.1): a posix item records the full set, an sftp
// item its mtime (and mode/ownership when the source has them), an s3
// item its mtime and the etag/vern it computed. An absent field means
// "the source did not say" -- never zero.
func (b *backupRun) processEntry(ctx context.Context, src source.Source, rel []byte, item source.SourceItem, parent *tree.Entry) (tree.Entry, bool, error) {
	if err := ctx.Err(); err != nil {
		return tree.Entry{}, false, err
	}
	mk := tree.MetaKind(src.MetaKind())
	display := displayPath(src, rel)

	entry := tree.Entry{
		Name:     item.Name,
		MetaKind: uint8(mk),
	}

	switch item.Kind.Kind {
	case source.KindSymlink:
		// Only the local source reports symlinks.
		b.stats.Symlinks++
		entry.Type = uint8(tree.TypeSymlink)
		entry.Target = item.Kind.Target
		if mk == tree.MetaPOSIX {
			if item.Posix == nil {
				b.skip("skipping %s: posix source did not provide metadata", display)
				return tree.Entry{}, false, nil
			}
			fillPosixMeta(&entry, item.Posix, false)
		}

	case source.KindDir:
		var parentDir *crypto.ID
		if parent != nil && tree.NodeType(parent.Type) == tree.TypeDir && parent.Subtree != nil {
			parentDir = parent.Subtree
		}
		subtree, err := b.walkDir(ctx, src, rel, parentDir)
		if err != nil {
			return tree.Entry{}, false, err
		}
		b.stats.Dirs++
		entry.Type = uint8(tree.TypeDir)
		entry.Subtree = &subtree
		switch mk {
		case tree.MetaPOSIX:
			if item.Posix == nil {
				// Unreachable for the sources this build ships: the local
				// source always captures what it lists. Refusing beats an
				// entry that could not pass its own validation.
				b.skip("skipping %s: posix source did not provide metadata", display)
				return tree.Entry{}, false, nil
			}
			fillPosixMeta(&entry, item.Posix, false)
		case tree.MetaSFTP:
			// The sftp kind requires an mtime, but a directory listing's
			// common prefixes have no time to record: the conservative
			// generic kind (no fields at all) is what a reader can trust.
			// Restore accordingly leaves the directory's time alone
			// instead of setting it to the epoch.
			entry.MetaKind = uint8(tree.MetaGeneric)
		case tree.MetaS3, tree.MetaGeneric:
			// s3 directories record no fields: everything the kind
			// carries is optional, and a listed prefix has nothing to
			// record. generic is already the empty kind.
		}

	case source.KindFile:
		// A posix source may only back up regular files: sockets, FIFOs
		// and device nodes are skipped with a warning, because restoring
		// them faithfully needs privileges a restore should not assume.
		// Remote sources have no such type bits to check.
		if mk == tree.MetaPOSIX {
			switch {
			case item.Posix == nil:
				b.skip("skipping %s: posix source did not provide metadata", display)
				return tree.Entry{}, false, nil
			case !fs.FileMode(item.Posix.Mode).IsRegular():
				b.skip("skipping %s: %s is not a file, directory or symlink", display, fs.FileMode(item.Posix.Mode).Type())
				return tree.Entry{}, false, nil
			}
		}

		facts := fileFacts{size: item.Kind.Size, posix: item.Posix, etag: item.Kind.Etag}
		size, chunks, ctype, ok, err := b.processFile(ctx, src, rel, display, facts, parent)
		if err != nil {
			return tree.Entry{}, false, err
		}
		if !ok {
			return tree.Entry{}, false, nil // read failure: already recorded
		}
		b.stats.Files++
		entry.Type = uint8(tree.TypeFile)
		entry.Size = size
		entry.Chunks = chunks
		entry.ContentType = uint8(ctype)
		entry.MTimeNs = tree.Ptr(item.Kind.MTimeNs)
		if mk == tree.MetaS3 {
			// etag/vern are s3-only: every other kind must leave them
			// absent (§8.1), and no other source computes them.
			entry.Etag = item.Kind.Etag
			entry.Vern = item.Kind.Vern
		}
		switch mk {
		case tree.MetaPOSIX:
			if item.Posix == nil {
				b.skip("skipping %s: posix source did not provide metadata", display)
				return tree.Entry{}, false, nil
			}
			fillPosixMeta(&entry, item.Posix, true)
		case tree.MetaSFTP:
			// mode/uid/gid are recorded when the source has them; ctime
			// and the s3 fields must stay absent (§8.1).
			if item.Posix != nil {
				entry.Mode = tree.Ptr(item.Posix.Mode)
				entry.UID = tree.Ptr(item.Posix.UID)
				entry.GID = tree.Ptr(item.Posix.GID)
			}
		case tree.MetaS3, tree.MetaGeneric:
		}

	default:
		return tree.Entry{}, false, fmt.Errorf("backup: %s: unknown source item kind %d", display, item.Kind.Kind)
	}

	// Extended attributes only exist for a local (posix) source, read
	// from the entry's own path.
	if mk == tree.MetaPOSIX {
		if path, ok := source.LocalPathOf(src, rel); ok {
			entry.Xattrs = readXattrs(path)
		}
	}
	return entry, true, nil
}

// skip records one unreadable source item: a warning to the user and an
// error counter in the run report. The snapshot still lands; a backup
// that skipped something must say so, and the CLI exits non-zero on it.
func (b *backupRun) skip(format string, args ...any) {
	b.opts.warn(format, args...)
	b.report.Errors++
}

// writeTree stores one tree with the v3 discipline.
//
// A tree is content-addressed and write-once: the first writer puts it
// with PutIfAbsent and it is never rewritten. If the key is taken, the
// bytes already there are verified (decrypt, re-hash, decode); a corrupt
// resident is overwritten with the good bytes -- same name means same
// bytes, so the overwrite can never clobber a different legitimate tree.
// A tree that was already stored is not ours to have written, so its
// revival signal is refreshed: touch/<id>, 8 fixed bytes, OVERWRITING
// Put -- the refreshed backend mtime is the signal itself, and only an
// overwriting Put refreshes it. With replicas=1 the .r1 copy rides along
// (PutIfAbsent; identical bytes).
func (b *backupRun) writeTree(ctx context.Context, t *tree.Tree) (crypto.ID, error) {
	id, encoded, err := t.Encode(&b.repo.keys.Hash)
	if err != nil {
		return crypto.ID{}, err
	}

	sealed, err := crypto.Seal(&b.repo.keys.Meta, id[:], encoded, b.repo.nonceSource)
	if err != nil {
		return crypto.ID{}, fmt.Errorf("save tree %s: %w", id, err)
	}

	// Same content, same tree, many times in one walk: store it once.
	if _, seen := b.seenTrees[id]; seen {
		return id, nil
	}
	b.seenTrees[id] = struct{}{}

	primary := true
	switch err := backend.PutBytesIfAbsent(ctx, b.repo.backend, tree.Key(id), sealed); {
	case err == nil:
	case errors.Is(err, backend.ErrExists):
		// Already stored: prove the resident bytes are good, heal if not.
		if _, err := tree.LoadAt(ctx, b.repo.backend, b.repo.keys, tree.Key(id), id); err != nil {
			b.opts.warn("tree %s is corrupt; healing with good bytes: %v", id, err)
			if err := b.repo.backend.Put(ctx, tree.Key(id), bytes.NewReader(sealed), int64(len(sealed))); err != nil {
				return crypto.ID{}, fmt.Errorf("heal tree %s: %w", id, err)
			}
		}
		primary = false
	default:
		return crypto.ID{}, fmt.Errorf("save tree %s: %w", id, err)
	}

	if b.repo.config.Replicas > 0 {
		if err := backend.PutBytesIfAbsent(ctx, b.repo.backend, tree.ReplicaKey(id), sealed); err != nil && !errors.Is(err, backend.ErrExists) {
			return crypto.ID{}, fmt.Errorf("save tree %s: write replica: %w", id, err)
		}
	}

	if !primary {
		// A reused tree: refresh its revival signal with an overwriting
		// Put. PutIfAbsent would leave the old mtime in place, and a mark
		// written between two reuses would then out-age the signal.
		if err := b.repo.touchTree(ctx, id); err != nil {
			return crypto.ID{}, err
		}
	} else if _, markedAlready := b.marked[id]; markedAlready {
		// A tree created anew under a mark an earlier sweep left behind
		// (the mark outlives the deletion by one run). The fresh object
		// is as alive as a touched one, but the commit gate reads only
		// the touch signal, so without this Put the backup would be
		// refused until a prune run revived the tree for it. The signal
		// is honest: this backup does use the tree from here on.
		if err := b.repo.touchTree(ctx, id); err != nil {
			return crypto.ID{}, err
		}
	}
	return id, nil
}

// saveTreeSegments writes entries as one tree, or as a chain of at most
// MaxNodesPerTree-entry trees linked by Prev when the directory is huge.
// The ID of the LAST segment is what the parent records; earlier
// segments of an unchanged directory keep their names and are reused.
func (b *backupRun) saveTreeSegments(ctx context.Context, entries []tree.Entry) (crypto.ID, error) {
	return b.saveTreeSegmentsFrom(ctx, entries, nil)
}

// saveTreeSegmentsFrom is saveTreeSegments with the chain already under
// way: prev names the segment the next one links back to, which is how
// walkDir flushes huge directories in bounded pieces.
func (b *backupRun) saveTreeSegmentsFrom(ctx context.Context, entries []tree.Entry, prev *crypto.ID) (crypto.ID, error) {
	for len(entries) > tree.MaxNodesPerTree {
		part := entries[:tree.MaxNodesPerTree]
		entries = entries[tree.MaxNodesPerTree:]
		id, err := b.writeSegment(ctx, part, prev)
		if err != nil {
			return crypto.ID{}, err
		}
		prev = &id
	}
	return b.writeSegment(ctx, entries, prev)
}

// writeSegment stores one segment of a directory tree.
func (b *backupRun) writeSegment(ctx context.Context, entries []tree.Entry, prev *crypto.ID) (crypto.ID, error) {
	t := tree.New(entries)
	t.Prev = prev
	return b.writeTree(ctx, t)
}

// processFile chunks one file, and fills in what its entry needs:
// (size, chunks, content type). The bool reports whether the file was
// backed up; false with a nil error means it was skipped with a warning.
//
// Order matters: the parent's fast path first (a provably unchanged file
// is never read), then the run's own hard-link table (a second name for
// an already-read inode is never re-chunked), and only then the read.
func (b *backupRun) processFile(ctx context.Context, src source.Source, rel []byte, display string, facts fileFacts, parent *tree.Entry) (uint64, []crypto.ID, tree.ContentType, bool, error) {
	if size, chunks, ctype, ok := b.tryReuse(ctx, facts, parent); ok {
		b.report.FilesReused++
		return size, chunks, ctype, true, nil
	}

	// Hard links only exist for a local source: the identity is the
	// kernel's, and a remote item has nothing to name one with.
	var key hardLinkKey
	isHardlink := false
	if facts.posix != nil && facts.posix.NLink > 1 {
		key = hardLinkKey{device: facts.posix.Dev, inode: facts.posix.Inode}
		isHardlink = true
		if first, seen := b.hard[key]; seen {
			return first.size, first.chunks, first.ctype, true, nil
		}
	}

	rc, err := src.Read(ctx, rel)
	if err != nil {
		b.skip("skipping %s: %v", display, err)
		return 0, nil, 0, false, nil
	}
	defer func() { _ = rc.Close() }()

	chunks, n, err := b.chunkAll(ctx, rc)
	if err != nil {
		// Cancellation is the caller's decision, not the file's problem;
		// any other read error skips just this file.
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return 0, nil, 0, false, err
		}
		b.skip("skipping %s: %v", display, err)
		return 0, nil, 0, false, nil
	}
	// The size on disk when it was read, not when it was listed: a file
	// growing during a backup would otherwise be recorded with a length
	// its chunks do not cover, and restore would produce a short file
	// with no complaint.
	size := n
	b.countBytes(key, isHardlink, n)

	ctype := tree.ContentDirect
	if len(chunks) > tree.MaxInlineChunks {
		encoded, err := crypto.Marshal(tree.NewChunkList(chunks))
		if err != nil {
			return 0, nil, 0, false, fmt.Errorf("backup %s: encode chunk list: %w", display, err)
		}
		listChunks, _, err := b.chunkAll(ctx, bytes.NewReader(encoded))
		if err != nil {
			return 0, nil, 0, false, fmt.Errorf("backup %s: store chunk list: %w", display, err)
		}
		chunks = listChunks
		ctype = tree.ContentIndirect
	}

	if isHardlink {
		b.hard[key] = fileContents{size: size, chunks: chunks, ctype: ctype}
	}
	return size, chunks, ctype, true, nil
}

// countBytes adds one file's length to the stats. Hard-link content is
// counted once per (dev,ino) group across the whole snapshot; files with
// no hard-link identity are counted per name.
func (b *backupRun) countBytes(key hardLinkKey, isHardlink bool, n uint64) {
	if isHardlink {
		if _, counted := b.countedHardlinks[key]; counted {
			return
		}
		b.countedHardlinks[key] = struct{}{}
	}
	b.stats.Bytes += n
}

func (b *backupRun) chunkAll(ctx context.Context, r io.Reader) ([]crypto.ID, uint64, error) {
	c := b.chunker
	if c == nil {
		var err error
		if c, err = chunker.NewParams(r, b.repo.config.Chunker.chunkerParams()); err != nil {
			return nil, 0, err
		}
		b.chunker = c
	} else if err := c.Reset(r); err != nil {
		return nil, 0, err
	}

	var (
		ids   []crypto.ID
		total uint64
	)
	for {
		if err := ctx.Err(); err != nil {
			return nil, 0, err
		}

		chunk, err := c.Next()
		if errors.Is(err, io.EOF) {
			return ids, total, nil
		}
		if err != nil {
			return nil, 0, err
		}

		id := crypto.ContentID(&b.repo.keys.Hash, chunk.Data)
		ids = append(ids, id)
		total += uint64(len(chunk.Data))

		if b.has(id) {
			// Already stored: the bytes were not uploaded again, and the
			// report says the chunk was accounted for without that.
			b.report.ChunksRead++
			continue
		}
		if err := b.add(ctx, id, chunk.Data); err != nil {
			return nil, 0, err
		}
	}
}

// has reports whether a chunk is already stored, by this run or by the
// repository, in a pack that is going to stay.
//
// A chunk whose pack prune has marked counts as absent and is uploaded
// again into a pack of this run's. v3 backups never remove marks (the
// backup role holds no Delete permission): re-uploading is what keeps
// the data safe, and the next prune re-evaluates the mark on its own --
// by then a live snapshot resolves chunks to this run's pack, so the
// marked pack is genuinely dead and its space is reclaimed.
func (b *backupRun) has(id crypto.ID) bool {
	if _, ok := b.uploaded[id]; ok {
		return true
	}
	loc, ok := b.repo.index.Lookup(id)
	if !ok {
		return false
	}
	if _, doomed := b.marked[loc.Pack]; doomed {
		b.report.PacksRevived++
		return false
	}

	b.referenced[loc.Pack] = struct{}{}
	return true
}

// verifyBeforeCommit is the v3 commit gate. A backup that ran longer
// than the grace period never commits (its trees may already have been
// deleted along with their marks, beyond any after-the-fact check), and
// every chunk this snapshot refers to must resolve in a freshly loaded
// index to a pack that exists and whose mark, if any, is young. Trees
// are checked through their touch signals: a reachable tree whose mark
// has expired by now, or that was already marked when this backup
// started, must still exist and carry a touch no older than the mark --
// the only defence against prune's HEAD-to-DELETE window.
func (b *backupRun) verifyBeforeCommit(ctx context.Context) error {
	grace := b.opts.gcGrace()
	elapsed := b.repo.now().UTC().Sub(b.started)
	margin := graceSafetyMargin
	if margin > grace/2 {
		margin = grace / 2
	}
	if elapsed+margin >= grace {
		return fmt.Errorf("%w (elapsed %s, grace %s)", ErrBackupTooLong, elapsed, grace)
	}

	needChecks := len(b.referenced) > 0 || len(b.seenTrees) > 0
	if !needChecks {
		return nil
	}
	if err := b.repo.refreshIndex(ctx, b.opts.warn); err != nil {
		return err
	}
	marks, err := b.repo.listMarkedPacks(ctx)
	if err != nil {
		return err
	}
	now := b.repo.now().UTC()

	for id := range b.referenced {
		if _, own := b.ownPacks[id]; own {
			continue
		}
		if markedAt, doomed := marks[id]; doomed && now.Sub(markedAt) >= grace {
			return fmt.Errorf("pack %s has been marked for deletion longer than the grace period", id)
		}
		if _, err := b.repo.backend.Stat(ctx, pack.Key(id)); err != nil {
			return fmt.Errorf("pack %s, referenced by this backup, is missing: %w", id, err)
		}
	}

	// Reachable trees with marks. Times are whole seconds: "touched in
	// the same second as the mark" counts as touched, matching prune's
	// safe-side comparison.
	for id, markedAt := range marks {
		if _, reached := b.seenTrees[id]; !reached || now.Sub(markedAt) < grace {
			continue
		}
		if err := b.repo.headTreeAndTouch(ctx, id, markedAt); err != nil {
			return err
		}
	}
	for id, markedAt := range b.marked {
		if _, reached := b.seenTrees[id]; !reached {
			continue
		}
		if err := b.repo.headTreeAndTouch(ctx, id, markedAt); err != nil {
			return err
		}
	}
	return nil
}

// add puts one new chunk into the current pack, flushing when full.
func (b *backupRun) add(ctx context.Context, id crypto.ID, data []byte) error {
	if b.writer == nil {
		w, err := pack.NewWriterParams(b.repo.keys, b.opts.SpoolDir, b.repo.config.PackTargetSize, b.repo.nonceSource)
		if err != nil {
			return err
		}
		if b.opts.Parity > 0 {
			w.SetParity(b.opts.Parity, b.opts.warn)
		}
		b.writer = w
	}

	if err := b.writer.Add(id, data); err != nil {
		return err
	}
	b.uploaded[id] = struct{}{}
	b.report.ChunksNew++

	if b.writer.Full() {
		return b.flush(ctx)
	}
	return nil
}

// flush finishes the current pack and records its entries.
func (b *backupRun) flush(ctx context.Context) error {
	if b.writer == nil || b.writer.Count() == 0 {
		return nil
	}

	packID, entries, size, err := b.writer.Finish(ctx, b.repo.backend)
	b.writer = nil
	if err != nil {
		return fmt.Errorf("backup: %w", err)
	}

	// Plain AddPack into the ranked in-memory view: this run's own pack
	// is unmarked, so the name comparison alone cannot lose it to a
	// marked pack, and has() consults b.uploaded before the index anyway.
	b.repo.index.AddPack(packID, entries)
	b.written[packID] = index.PackInfo{Size: size, Entries: entries}
	b.ownPacks[packID] = struct{}{}
	b.report.PacksNew++
	for _, e := range entries {
		b.report.BytesStored += e.Length
	}
	return nil
}

func (b *backupRun) abort() {
	if b.writer != nil {
		b.writer.Abort()
		b.writer = nil
	}
}
