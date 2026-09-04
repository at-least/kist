package repo

import (
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

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
}

func (o BackupOptions) warn(format string, args ...any) {
	if o.Warnf != nil {
		o.Warnf(format, args...)
	}
}

// Backup walks paths and commits a snapshot.
//
// The order is forced by the format: chunks go into packs, packs are
// uploaded, trees are written bottom-up, the index blob is written, and
// only then is the snapshot committed. Dying anywhere before that last
// step leaves objects nothing refers to -- wasted space that prune
// reclaims, never a repository that cannot be read.
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

	// Three things happen before any file is read, in this order, and a
	// backup that cannot do all three does not run. Prune's sweep argues
	// from them: a client is registered before it deduplicates against
	// anything, so a sweep that lists clients after this point waits for
	// this backup; and a backup that started after a pack was marked
	// read the marks after the mark, so it sees the mark and uploads the
	// pack's chunks again. A client whose
	// credentials cannot register or list gc/ is on a data-loss path,
	// not in a degraded mode.
	started := r.now().UTC()
	if err := r.register(ctx, started); err != nil {
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}
	// Marks before the index. Prune rewrites the index before it removes
	// the mark of a pack that is gone, so a backup that misses the mark
	// is one that lists after the removal, and its index refresh, later
	// still, cannot name the pack.
	marked, err := r.listMarkedPacks(ctx)
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
		repo:       r,
		opts:       opts,
		marked:     marked,
		uploaded:   make(map[crypto.ID]struct{}),
		referenced: make(map[crypto.ID]struct{}),
		revived:    make(map[crypto.ID]struct{}),
		written:    make(map[crypto.ID][]pack.Entry),
		hard:       make(map[hardLinkKey][]crypto.ID),
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

	rootID, err := tree.New(entries).Save(ctx, r.backend, r.keys, r.nonceSource)
	if err != nil {
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}
	b.stats.Dirs++

	// The index blob is a cache, so it is written before the commit but a
	// failure to write it is still fatal: a backup that cannot record
	// where it put things has not finished.
	if len(b.written) > 0 {
		if _, err := index.Save(ctx, r.backend, r.keys, b.written, r.nonceSource); err != nil {
			return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
		}
	}

	// A pack this backup deduplicated against may have been marked by a
	// prune that ran meanwhile. It cannot have been deleted -- the mark is
	// younger than this backup, and this client, registered before the
	// mark, holds the sweep -- so removing the mark is enough to keep it.
	if err := b.reviveReferenced(ctx); err != nil {
		return nil, snapshot.Handle{}, fmt.Errorf("backup: %w", err)
	}

	snap := &snapshot.Snapshot{
		Version:  snapshot.Version,
		Root:     rootID,
		TimeNs:   started.UnixNano(),
		Host:     host,
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

type backupRun struct {
	repo *Repository
	opts BackupOptions

	writer *pack.Writer

	// marked is the set of packs prune has marked for deletion, as of
	// the start of this backup. See has.
	marked map[crypto.ID]struct{}

	// referenced is the set of packs this run deduplicated against: it
	// refers to their chunks without holding a copy. revived is the set
	// of marked packs whose mark this run has removed.
	referenced map[crypto.ID]struct{}
	revived    map[crypto.ID]struct{}

	// uploaded holds every chunk this run has put into a pack, finished
	// or not.
	//
	// The index only learns about a chunk when its pack is finished, so
	// without this a payload repeated inside one pack's worth of work --
	// two identical files, a duplicated directory -- would be added
	// twice. That produces a trailer listing one chunk twice, which fails
	// the trailer's own consistency check and makes the pack unreadable.
	// It does not show up in a small test: it needs the repeat to fall
	// inside a single pack. It is never cleared, because a chunk this run
	// re-uploaded out of a marked pack still resolves to the marked pack
	// in the index and would otherwise be re-uploaded on every repeat.
	uploaded map[crypto.ID]struct{}

	written map[crypto.ID][]pack.Entry
	hard    map[hardLinkKey][]crypto.ID
	stats   snapshot.Stats
}

// normalisePaths turns the caller's paths into absolute, deduplicated,
// sorted roots.
func normalisePaths(paths []string) ([]string, error) {
	seen := make(map[string]struct{}, len(paths))
	out := make([]string, 0, len(paths))

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
		out = append(out, abs)
	}
	sort.Strings(out)
	return out, nil
}

// backupRoots produces the entries of the synthetic root tree.
//
// Several source paths become several entries in one root, named by their
// base names. Two roots with the same base name would collide, so the
// second one takes the full path with separators replaced -- ugly, but
// visible, and better than silently dropping one.
func (b *backupRun) backupRoots(ctx context.Context, roots []string) ([]tree.Entry, error) {
	used := make(map[string]struct{}, len(roots))
	entries := make([]tree.Entry, 0, len(roots))

	for _, root := range roots {
		info, err := os.Lstat(root)
		if err != nil {
			return nil, fmt.Errorf("backup: %w", err)
		}

		name := rootName(root, used)
		entry, ok, err := b.entryFor(ctx, root, name, info)
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

func rootName(root string, used map[string]struct{}) string {
	name := filepath.Base(root)
	if name == "" || name == string(filepath.Separator) || name == "." {
		name = "root"
	}
	if _, taken := used[name]; taken {
		name = strings.ReplaceAll(strings.TrimPrefix(filepath.ToSlash(root), "/"), "/", "_")
	}
	used[name] = struct{}{}
	return name
}

// entryFor produces the tree entry for one filesystem object, recursing
// into directories. The bool reports whether the entry should be
// included; false means it was skipped with a warning.
func (b *backupRun) entryFor(ctx context.Context, path, name string, info fs.FileInfo) (tree.Entry, bool, error) {
	if err := ctx.Err(); err != nil {
		return tree.Entry{}, false, err
	}

	entry := tree.Entry{
		Name:    name,
		Mode:    uint32(info.Mode()),
		MTimeNs: info.ModTime().UnixNano(),
	}
	fillOwnership(&entry, info)

	switch {
	case info.Mode()&fs.ModeSymlink != 0:
		target, err := os.Readlink(path)
		if err != nil {
			b.opts.warn("skipping %s: read link: %v", path, err)
			return tree.Entry{}, false, nil
		}
		entry.Type = tree.TypeSymlink
		entry.Target = target
		b.stats.Symlinks++
		return entry, true, nil

	case info.IsDir():
		subtree, err := b.backupDir(ctx, path)
		if err != nil {
			return tree.Entry{}, false, err
		}
		entry.Type = tree.TypeDir
		entry.Subtree = subtree
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

func (b *backupRun) backupDir(ctx context.Context, path string) (crypto.ID, error) {
	names, err := os.ReadDir(path)
	if err != nil {
		if isSkippable(err) {
			b.opts.warn("reading %s: %v; storing it empty", path, err)
			return tree.New(nil).Save(ctx, b.repo.backend, b.repo.keys, b.repo.nonceSource)
		}
		return crypto.ID{}, fmt.Errorf("backup: read directory %s: %w", path, err)
	}

	entries := make([]tree.Entry, 0, len(names))
	for _, child := range names {
		info, err := child.Info()
		if err != nil {
			if isSkippable(err) {
				b.opts.warn("skipping %s: %v", filepath.Join(path, child.Name()), err)
				continue
			}
			return crypto.ID{}, fmt.Errorf("backup: stat %s: %w", filepath.Join(path, child.Name()), err)
		}

		entry, ok, err := b.entryFor(ctx, filepath.Join(path, child.Name()), child.Name(), info)
		if err != nil {
			return crypto.ID{}, err
		}
		if ok {
			entries = append(entries, entry)
		}
	}

	return tree.New(entries).Save(ctx, b.repo.backend, b.repo.keys, b.repo.nonceSource)
}

// backupFile chunks a file and fills in its entry.
//
// A file reachable through several hard links is chunked once: the second
// name reuses the first's chunk list, which is what the device and inode
// fields are recorded for.
func (b *backupRun) backupFile(ctx context.Context, path string, info fs.FileInfo, entry *tree.Entry) error {
	entry.Type = tree.TypeFile
	entry.Size = uint64(max(info.Size(), 0))

	if key, links, ok := hardLinkOf(info); ok && links > 1 {
		entry.Device, entry.Inode, entry.Links = key.device, key.inode, links
		if chunks, seen := b.hard[key]; seen {
			entry.Chunks = chunks
			b.stats.Bytes += entry.Size
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
	entry.Chunks = chunks
	// The size on disk when it was read, not when it was stat'ed: a file
	// growing during a backup would otherwise be recorded with a length
	// its chunks do not cover, and restore would produce a short file
	// with no complaint.
	entry.Size = n
	b.stats.Bytes += n

	if key, links, ok := hardLinkOf(info); ok && links > 1 {
		b.hard[key] = chunks
	}
	return nil
}

func (b *backupRun) chunkAll(ctx context.Context, r io.Reader) ([]crypto.ID, uint64, error) {
	c, err := chunker.New(r)
	if err != nil {
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

		if b.has(ctx, id) {
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
// A chunk whose pack prune has marked counts as absent: it is uploaded
// again, into a pack of this run's, and the mark is removed. Removing
// the mark alone would leave a race -- prune reads the mark, this client
// removes it, prune deletes the pack -- that no ordering of two
// unconditional operations can close. Uploading the chunk again closes
// it: whichever way the race goes, a pack holding the chunk survives.
// The upload costs nothing worth counting, since the data it repeats
// was about to be deleted.
func (b *backupRun) has(ctx context.Context, id crypto.ID) bool {
	if _, ok := b.uploaded[id]; ok {
		return true
	}
	loc, ok := b.repo.index.Lookup(id)
	if !ok {
		return false
	}
	if _, doomed := b.marked[loc.Pack]; doomed {
		b.revive(ctx, loc.Pack)
		return false
	}
	b.referenced[loc.Pack] = struct{}{}
	return true
}

// reviveReferenced removes any mark placed during this backup on a pack
// it refers to.
func (b *backupRun) reviveReferenced(ctx context.Context) error {
	if len(b.referenced) == 0 {
		return nil
	}
	marked, err := b.repo.listMarkedPacks(ctx)
	if err != nil {
		return err
	}
	for _, id := range sortedIDs(marked) {
		if _, ok := b.referenced[id]; ok {
			b.revive(ctx, id)
		}
	}
	return nil
}

// revive removes the mark from a pack this backup is about to reference,
// once per pack. The mark is what tells prune the pack is unreferenced;
// leaving it would have prune delete a pack that, after this backup
// commits, a snapshot resolves chunks to.
//
// The pack stays in marked: every chunk of a marked pack is uploaded
// again, not only the first one met. A failure to remove the mark is
// reported, not fatal: the re-upload in has is what keeps the data
// safe, and a backup role without Delete on gc/ must still be able to
// back up.
func (b *backupRun) revive(ctx context.Context, id crypto.ID) {
	if _, done := b.revived[id]; done {
		return
	}
	b.revived[id] = struct{}{}
	if err := b.repo.remove(ctx, gcKey(id)); err != nil {
		b.opts.warn("could not remove the gc mark on pack %s: %v; the data is re-uploaded, prune will re-evaluate the mark", id, err)
	}
	b.stats.PacksRevived++
}

// add puts one new chunk into the current pack, flushing when full.
func (b *backupRun) add(ctx context.Context, id crypto.ID, data []byte) error {
	if b.writer == nil {
		w, err := pack.NewWriter(b.repo.keys, b.opts.SpoolDir, b.repo.nonceSource)
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

	packID, entries, err := b.writer.Finish(ctx, b.repo.backend)
	b.writer = nil
	if err != nil {
		return fmt.Errorf("backup: %w", err)
	}

	b.repo.index.AddPack(packID, entries)
	b.written[packID] = entries
	b.stats.PacksAdded++
	for _, e := range entries {
		b.stats.BytesStored += uint64(e.Length)
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
