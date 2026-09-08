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
	"github.com/at-least/kist/internal/tree"
)

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
	// without being read. This implementation has no fast path and always
	// re-reads, so it is always 0; the field exists so reports parse the
	// same everywhere.
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
	if len(paths) == 0 {
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
	if err := r.refreshIndex(ctx, opts.warn); err != nil {
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

	roots, err := normalisePaths(paths)
	if err != nil {
		return BackupSummary{}, err
	}

	snapRoots, err := b.backupRoots(ctx, roots)
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

	snap := &snapshot.Snapshot{
		Version:  snapshot.Version,
		Roots:    snapRoots,
		TimeNs:   started.UnixNano(),
		Host:     host,
		User:     user,
		ClientID: r.clientID,
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

// backupRoots produces one snapshot root per source path.
//
// There is no synthetic root in v3: each source's directory CONTENTS
// become that root's tree, and entry names are always single path
// components. A source that is a file or symlink yields a root tree with
// exactly one entry, named by the source path's last component.
func (b *backupRun) backupRoots(ctx context.Context, roots [][]byte) ([]snapshot.Root, error) {
	out := make([]snapshot.Root, 0, len(roots))

	for _, root := range roots {
		path := string(root)
		info, err := os.Lstat(path)
		if err != nil {
			return nil, fmt.Errorf("backup: %w", err)
		}

		var id crypto.ID
		switch {
		case info.IsDir():
			id, err = b.backupDir(ctx, path)
		default:
			// A file or symlink source: the root tree carries the one
			// entry, under the path's own last component.
			name := lastComponent(root)
			if len(name) == 0 {
				return nil, fmt.Errorf("backup: cannot derive a name for source path %s", path)
			}
			entry, ok, err := b.entryFor(ctx, path, name, info)
			if err != nil {
				return nil, err
			}
			if !ok {
				return nil, fmt.Errorf("backup: %s: source could not be read", path)
			}
			id, err = b.writeTree(ctx, tree.New([]tree.Entry{entry}))
			if err != nil {
				return nil, err
			}
		}
		if err != nil {
			return nil, err
		}
		out = append(out, snapshot.Root{Path: slices.Clone(root), Tree: id})
	}
	return out, nil
}

// lastComponent returns the bytes after the final path separator, as the
// entry name of a file or symlink source.
func lastComponent(path []byte) []byte {
	if i := bytes.LastIndexByte(path, '/'); i >= 0 {
		return path[i+1:]
	}
	return path
}

// entryFor produces the tree entry for one filesystem object, recursing
// into directories. The bool reports whether the entry should be
// included; false means it was skipped with a warning.
func (b *backupRun) entryFor(ctx context.Context, path string, name []byte, info fs.FileInfo) (tree.Entry, bool, error) {
	if err := ctx.Err(); err != nil {
		return tree.Entry{}, false, err
	}

	// v3: a local source is always a posix entry. mode/uid/gid/mtime are
	// required (uid 0 is root, a real value); ctime and the hard-link
	// identity are recorded when the platform has them.
	entry := tree.Entry{
		Name:     name,
		Type:     uint8(tree.TypeFile),
		MetaKind: uint8(tree.MetaPOSIX),
		Mode:     tree.Ptr(uint32(info.Mode())),
		MTimeNs:  tree.Ptr(info.ModTime().UnixNano()),
	}
	fillOwnership(&entry, info)
	entry.Xattrs = readXattrs(path)

	switch {
	case info.Mode()&fs.ModeSymlink != 0:
		target, err := os.Readlink(path)
		if err != nil {
			b.skip("skipping %s: read link: %v", path, err)
			return tree.Entry{}, false, nil
		}
		entry.Type = uint8(tree.TypeSymlink)
		entry.Target = []byte(target)
		b.stats.Symlinks++
		return entry, true, nil

	case info.IsDir():
		subtree, err := b.backupDir(ctx, path)
		if err != nil {
			return tree.Entry{}, false, err
		}
		entry.Type = uint8(tree.TypeDir)
		entry.Subtree = &subtree
		b.stats.Dirs++
		return entry, true, nil

	case info.Mode().IsRegular():
		if err := b.backupFile(ctx, path, info, &entry); err != nil {
			if isSkippable(err) {
				b.skip("skipping %s: %v", path, err)
				return tree.Entry{}, false, nil
			}
			return tree.Entry{}, false, err
		}
		b.stats.Files++
		return entry, true, nil

	default:
		// Sockets, FIFOs and device nodes. Restoring one faithfully needs
		// privileges a restore should not assume, and its contents are
		// never what the user meant to save.
		b.skip("skipping %s: %s is not a file, directory or symlink", path, info.Mode().Type())
		return tree.Entry{}, false, nil
	}
}

// skip records one unreadable source item: a warning to the user and an
// error counter in the run report. The snapshot still lands; a backup
// that skipped something must say so, and the CLI exits non-zero on it.
func (b *backupRun) skip(format string, args ...any) {
	b.opts.warn(format, args...)
	b.report.Errors++
}

// backupDir returns the tree ID (of the last segment) for one directory.
func (b *backupRun) backupDir(ctx context.Context, path string) (crypto.ID, error) {
	names, err := os.ReadDir(path)
	if err != nil {
		if isSkippable(err) {
			b.skip("reading %s: %v; storing it empty", path, err)
			return b.saveTreeSegments(ctx, nil)
		}
		return crypto.ID{}, fmt.Errorf("backup: read directory %s: %w", path, err)
	}

	entries := make([]tree.Entry, 0, len(names))
	for _, child := range names {
		info, err := childInfo(path, child)
		if err != nil {
			if isSkippable(err) {
				b.skip("skipping %s: %v", filepath.Join(path, child.Name()), err)
				continue
			}
			return crypto.ID{}, fmt.Errorf("backup: stat %s: %w", filepath.Join(path, child.Name()), err)
		}

		entry, ok, err := b.entryFor(ctx, filepath.Join(path, child.Name()), []byte(child.Name()), info)
		if err != nil {
			return crypto.ID{}, err
		}
		if ok {
			entries = append(entries, entry)
		}
	}

	return b.saveTreeSegments(ctx, entries)
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
	var prev *crypto.ID
	for len(entries) > tree.MaxNodesPerTree {
		part := entries[:tree.MaxNodesPerTree]
		entries = entries[tree.MaxNodesPerTree:]
		t := tree.New(part)
		t.Prev = prev
		id, err := b.writeTree(ctx, t)
		if err != nil {
			return crypto.ID{}, err
		}
		prev = &id
	}
	t := tree.New(entries)
	t.Prev = prev
	return b.writeTree(ctx, t)
}

// childInfo returns a directory entry's metadata. Files take the parent
// listing's word for it; directories are stat'ed themselves.
//
// On NTFS a directory's modification time as reported by its parent's
// listing lags the directory's own record for a while after the
// directory changed. Two walks of an unchanged tree then disagree about
// a subdirectory's mtime, the subtree gets a new name, and deduplication
// of unchanged directories -- the point of content addressing -- fails.
// The Windows CI run found it; stat'ing the directory itself reads the
// authoritative record on every platform, at one extra system call per
// directory.
func childInfo(parent string, child fs.DirEntry) (fs.FileInfo, error) {
	if child.IsDir() {
		return os.Lstat(filepath.Join(parent, child.Name()))
	}
	return child.Info()
}

// backupFile chunks a file and fills in its entry.
//
// A file reachable through several hard links is chunked once: the second
// name reuses the first's chunk list, which is what the device and inode
// fields are recorded for.
//
// A file with more than MaxInlineChunks chunks switches to indirect
// storage: the list itself is encoded and chunked like data, and the
// entry points at those chunks.
func (b *backupRun) backupFile(ctx context.Context, path string, info fs.FileInfo, entry *tree.Entry) error {
	entry.Type = uint8(tree.TypeFile)
	entry.Size = uint64(max(info.Size(), 0))

	var key hardLinkKey
	var links uint64
	if k, l, ok := hardLinkOf(info); ok && l > 1 {
		key, links = k, l
		entry.Device, entry.Inode, entry.Links = tree.Ptr(key.device), tree.Ptr(key.inode), tree.Ptr(links)
		if first, seen := b.hard[key]; seen {
			entry.Chunks = first.chunks
			entry.ContentType = uint8(first.ctype)
			entry.Size = first.size
			return nil
		}
	}

	f, err := os.Open(path) //nolint:gosec // path comes from walking what the user asked to back up
	if err != nil {
		return err
	}
	defer func() { _ = f.Close() }()

	chunks, n, err := b.chunkAll(ctx, f)
	if err != nil {
		return fmt.Errorf("backup %s: %w", path, err)
	}
	// The size on disk when it was read, not when it was stat'ed: a file
	// growing during a backup would otherwise be recorded with a length
	// its chunks do not cover, and restore would produce a short file
	// with no complaint.
	entry.Size = n
	b.countBytes(key, links > 1, n)

	ctype := tree.ContentDirect
	if len(chunks) > tree.MaxInlineChunks {
		encoded, err := crypto.Marshal(tree.NewChunkList(chunks))
		if err != nil {
			return fmt.Errorf("backup %s: encode chunk list: %w", path, err)
		}
		listChunks, _, err := b.chunkAll(ctx, bytes.NewReader(encoded))
		if err != nil {
			return fmt.Errorf("backup %s: store chunk list: %w", path, err)
		}
		chunks = listChunks
		ctype = tree.ContentIndirect
	}
	entry.Chunks = chunks
	entry.ContentType = uint8(ctype)

	if links > 1 {
		b.hard[key] = fileContents{size: n, chunks: entry.Chunks, ctype: ctype}
	}
	return nil
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

// isSkippable reports whether an error is one file's problem rather than
// the backup's. A permission denied on one file must not abandon the
// other hundred thousand.
func isSkippable(err error) bool {
	return errors.Is(err, fs.ErrPermission) || errors.Is(err, fs.ErrNotExist)
}
