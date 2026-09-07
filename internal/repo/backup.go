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
	"sort"
	"time"

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

// Backup walks paths and commits a snapshot.
//
// The order is forced by the format: chunks go into packs, packs are
// uploaded, trees are written bottom-up, the index blob is written, and
// only then is the snapshot committed. Dying anywhere before that last
// step leaves objects nothing refers to -- wasted space that prune
// reclaims, never a repository that cannot be read.
//
// v2 GC obligations (docs/format.md §13): this backup is strictly
// Put-only. Packs that carry a gc mark are not deduplicated against --
// their chunks are uploaded again -- and no mark is ever removed. Before
// committing, the index is reloaded and every referenced chunk must
// resolve to a pack that exists and whose mark, if any, is young; and
// the whole run must be shorter than the grace period.
func (r *Repository) Backup(ctx context.Context, paths []string, opts BackupOptions) (*snapshot.Snapshot, snapshot.Handle, error) {
	if len(paths) == 0 {
		return nil, snapshot.Handle{}, errors.New("backup: no paths given")
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
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}
	if err := r.refreshIndex(ctx, opts.warn); err != nil {
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}
	if backupHooks.afterMarks != nil {
		backupHooks.afterMarks()
	}

	b := &backupRun{
		repo:         r,
		opts:         opts,
		marked:       marksAtStart,
		uploaded:     make(map[crypto.ID]struct{}),
		referenced:   make(map[crypto.ID]struct{}),
		written:      make(map[crypto.ID]index.PackInfo),
		ownPacks:     make(map[crypto.ID]struct{}),
		hard:         make(map[hardLinkKey]fileContents),
		writtenTrees: make(map[crypto.ID]struct{}),
		started:      started,
	}
	defer b.abort()

	roots, err := normalisePaths(paths)
	if err != nil {
		return nil, snapshot.Handle{}, err
	}

	entries, err := b.backupRoots(ctx, roots)
	if err != nil {
		return nil, snapshot.Handle{}, err
	}

	if err := b.flush(ctx); err != nil {
		return nil, snapshot.Handle{}, err
	}

	rootID, err := b.saveTreeSegments(ctx, entries)
	if err != nil {
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}

	// The index blob is a cache, so it is written before the commit but a
	// failure to write it is still fatal: a backup that cannot record
	// where it put things has not finished.
	if len(b.written) > 0 {
		if _, err := index.Save(ctx, r.backend, r.keys, b.written, nil, r.nonceSource); err != nil {
			return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
		}
	}

	if err := b.verifyBeforeCommit(ctx); err != nil {
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}

	snap := &snapshot.Snapshot{
		Version:  snapshot.Version,
		Root:     rootID,
		TimeNs:   started.UnixNano(),
		Host:     host,
		User:     user,
		Paths:    roots,
		ClientID: r.clientID,
		Stats:    b.stats,
	}
	handle, err := snap.Save(ctx, r.backend, r.keys, r.nonceSource)
	if err != nil {
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}
	return snap, handle, nil
}

// backupHooks are set by tests to interleave a backup with a prune at
// the point where the interleaving matters. Nil in production.
var backupHooks struct {
	afterMarks func()
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

	written      map[crypto.ID]index.PackInfo
	ownPacks     map[crypto.ID]struct{}
	hard         map[hardLinkKey]fileContents
	writtenTrees map[crypto.ID]struct{}
	stats        snapshot.Stats
	started      time.Time
}

// normalisePaths turns the caller's paths into absolute, deduplicated,
// sorted roots, as byte strings.
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
	out := make([][]byte, len(strs))
	for i, s := range strs {
		out[i] = []byte(s)
	}
	return out, nil
}

// backupRoots produces the entries of the synthetic root tree.
//
// Each source path becomes one entry named by the path's own bytes --
// the unified v2 rule, so a Rust-written snapshot and a Go-written
// snapshot of the same directory agree on the root tree's name too, and
// restore rebuilds the full absolute path under the target.
func (b *backupRun) backupRoots(ctx context.Context, roots [][]byte) ([]tree.Entry, error) {
	entries := make([]tree.Entry, 0, len(roots))

	for _, root := range roots {
		path := string(root)
		info, err := os.Lstat(path)
		if err != nil {
			return nil, fmt.Errorf("backup: %w", err)
		}

		entry, ok, err := b.entryFor(ctx, path, root, info)
		if err != nil {
			return nil, err
		}
		if !ok {
			continue
		}
		entries = append(entries, entry)
	}
	if len(entries) == 0 {
		return nil, errors.New("backup: nothing to back up; every source path was skipped")
	}
	return entries, nil
}

// entryFor produces the tree entry for one filesystem object, recursing
// into directories. The bool reports whether the entry should be
// included; false means it was skipped with a warning.
func (b *backupRun) entryFor(ctx context.Context, path string, name []byte, info fs.FileInfo) (tree.Entry, bool, error) {
	if err := ctx.Err(); err != nil {
		return tree.Entry{}, false, err
	}

	entry := tree.Entry{
		Name:    name,
		Mode:    uint32(info.Mode()),
		MTimeNs: info.ModTime().UnixNano(),
	}
	fillOwnership(&entry, info)
	entry.Xattrs = readXattrs(path)

	switch {
	case info.Mode()&fs.ModeSymlink != 0:
		target, err := os.Readlink(path)
		if err != nil {
			b.opts.warn("skipping %s: read link: %v", path, err)
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
				b.opts.warn("skipping %s: %v", path, err)
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
		b.opts.warn("skipping %s: %s is not a file, directory or symlink", path, info.Mode().Type())
		return tree.Entry{}, false, nil
	}
}

// backupDir returns the tree ID (of the last segment) for one directory.
func (b *backupRun) backupDir(ctx context.Context, path string) (crypto.ID, error) {
	names, err := os.ReadDir(path)
	if err != nil {
		if isSkippable(err) {
			b.opts.warn("reading %s: %v; storing it empty", path, err)
			return b.saveTreeSegments(ctx, nil)
		}
		return crypto.ID{}, fmt.Errorf("backup: read directory %s: %w", path, err)
	}

	entries := make([]tree.Entry, 0, len(names))
	for _, child := range names {
		info, err := childInfo(path, child)
		if err != nil {
			if isSkippable(err) {
				b.opts.warn("skipping %s: %v", filepath.Join(path, child.Name()), err)
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
		id, err := t.Save(ctx, b.repo.backend, b.repo.keys, b.repo.nonceSource)
		if err != nil {
			return crypto.ID{}, err
		}
		b.writtenTrees[id] = struct{}{}
		prev = &id
	}
	t := tree.New(entries)
	t.Prev = prev
	id, err := t.Save(ctx, b.repo.backend, b.repo.keys, b.repo.nonceSource)
	if err != nil {
		return crypto.ID{}, err
	}
	b.writtenTrees[id] = struct{}{}
	return id, nil
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

	if key, links, ok := hardLinkOf(info); ok && links > 1 {
		entry.Device, entry.Inode, entry.Links = key.device, key.inode, links
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
	b.stats.Bytes += n

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

	if key, links, ok := hardLinkOf(info); ok && links > 1 {
		b.hard[key] = fileContents{size: n, chunks: entry.Chunks, ctype: ctype}
	}
	return nil
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
// again into a pack of this run's. v2 backups never remove marks (the
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
		b.stats.PacksRevived++
		return false
	}

	b.referenced[loc.Pack] = struct{}{}
	return true
}

// verifyBeforeCommit is the v2 commit gate: a backup that ran longer
// than the grace period never commits (its trees may already have been
// deleted along with their marks, beyond any after-the-fact check), and
// every chunk this snapshot refers to must resolve in a freshly loaded
// index to a pack that exists and whose mark, if any, is young. Trees
// this run put get their own check: a tree with an expired mark must
// have been rewritten after the mark (our unconditional Put refreshes
// its mtime), and a tree already marked when this backup started must
// still exist with an mtime newer than that mark -- prune may have
// deleted it after our put and cleaned the mark, invisible afterwards.
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

	needMarks := len(b.referenced) > 0 || len(b.writtenTrees) > 0
	if !needMarks {
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

	// Trees this run put. Times are whole seconds: "rewritten in the
	// same second as the mark" counts as rewritten, matching prune's
	// not-older comparison.
	for id := range b.writtenTrees {
		if markedAt, doomed := marks[id]; doomed && now.Sub(markedAt) >= grace {
			info, err := b.repo.backend.Stat(ctx, tree.Key(id))
			if err != nil {
				return fmt.Errorf("tree %s, written by this backup, is missing: %w", id, err)
			}
			if info.Modified.Before(markedAt) {
				return fmt.Errorf("tree %s was marked for deletion longer than the grace period and has not been rewritten since", id)
			}
		}
		if markedAt, wasMarked := b.marked[id]; wasMarked {
			info, err := b.repo.backend.Stat(ctx, tree.Key(id))
			if err != nil {
				return fmt.Errorf("tree %s, written by this backup, is missing: %w", id, err)
			}
			if info.Modified.Before(markedAt) {
				return fmt.Errorf("tree %s was marked before this backup started and has not been rewritten since", id)
			}
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
	b.stats.ChunksNew++

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
	b.stats.PacksAdded++
	for _, e := range entries {
		b.stats.BytesStored += e.Length
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
