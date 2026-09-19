//go:build linux || darwin

// Package mount exposes a repository's snapshots as a read-only
// filesystem: <clientID>/<timestamp>/<the backed-up tree>.
//
// Everything below a snapshot is immutable, which the kernel is told
// through long cache timeouts; the two top levels change as backups
// land and are re-listed on every look.
package mount

import (
	"bytes"
	"container/list"
	"context"
	"errors"
	"fmt"
	iofs "io/fs"
	"slices"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/hanwen/go-fuse/v2/fs"
	"github.com/hanwen/go-fuse/v2/fuse"

	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

// Options configure a mount.
type Options struct {
	// ChunkCache is how many decoded chunks to keep. Zero means 8, which
	// with 8 MiB chunks bounds the cache at 64 MiB.
	ChunkCache int

	// Warnf receives non-fatal problems: a snapshot that will not load.
	Warnf func(format string, args ...any)

	// Debug logs every FUSE request.
	Debug bool
}

func (o Options) warn(format string, args ...any) {
	if o.Warnf != nil {
		o.Warnf(format, args...)
	}
}

// Server is a mounted filesystem.
type Server struct {
	server *fuse.Server
	dir    string
}

// Mount serves the repository at dir and returns once the mount is
// visible. The caller unmounts.
func Mount(ctx context.Context, r *repo.Repository, dir string, opts Options) (*Server, error) {
	if opts.ChunkCache <= 0 {
		opts.ChunkCache = 8
	}
	f := &filesystem{repo: r, chunks: r.NewChunkSource(), cache: newLRU(opts.ChunkCache), opts: opts, seen: map[string]struct{}{}}
	root := &rootNode{fs: f}
	hour := time.Hour
	server, err := fs.Mount(dir, root, &fs.Options{
		MountOptions: fuse.MountOptions{FsName: "kist", Name: "kist", Options: []string{"ro"}, Debug: opts.Debug},
		EntryTimeout: &hour, AttrTimeout: &hour,
	})
	if err != nil {
		return nil, fmt.Errorf("mount %s: %w", dir, err)
	}
	if err := server.WaitMount(); err != nil {
		if uerr := server.Unmount(); uerr != nil {
			opts.warn("unmount after a failed mount: %v", uerr)
		}
		return nil, fmt.Errorf("mount %s: %w", dir, err)
	}
	_ = ctx
	return &Server{server: server, dir: dir}, nil
}

// Wait blocks until the filesystem is unmounted.
func (s *Server) Wait() { s.server.Wait() }

// Unmount unmounts. It fails while something holds a file open.
func (s *Server) Unmount() error {
	if err := s.server.Unmount(); err != nil {
		return fmt.Errorf("unmount %s: %w", s.dir, err)
	}
	return nil
}

// Dir is the mountpoint.
func (s *Server) Dir() string { return s.dir }

type filesystem struct {
	repo   *repo.Repository
	chunks *repo.ChunkSource
	cache  *lru
	opts   Options

	// seen holds the snapshot keys already served. A key not in it was
	// committed after the last look, and may reference packs the index
	// loaded at mount time does not know: refresh before serving it.
	mu   sync.Mutex
	seen map[string]struct{}
}

func (f *filesystem) snapshots(ctx context.Context, client string) ([]snapshot.Handle, error) {
	handles, err := f.repo.Snapshots(ctx, client)
	if err != nil {
		return nil, err
	}
	f.mu.Lock()
	fresh := false
	for _, h := range handles {
		if _, ok := f.seen[h.Key]; !ok {
			fresh = true
			f.seen[h.Key] = struct{}{}
		}
	}
	f.mu.Unlock()
	if fresh {
		if err := f.repo.Refresh(ctx, f.opts.warn); err != nil {
			return nil, err
		}
	}
	return handles, nil
}

// chunk returns a decoded chunk, through the cache.
func (f *filesystem) chunk(ctx context.Context, id crypto.ID) ([]byte, error) {
	if data, ok := f.cache.get(id); ok {
		return data, nil
	}
	data, err := f.chunks.Chunk(ctx, id)
	if err != nil {
		return nil, err
	}
	f.cache.put(id, data)
	return data, nil
}

// The two top levels re-list on every look and are cached briefly; a
// snapshot's contents never change and are cached for a long time.
const (
	volatileTimeout  = time.Second
	immutableTimeout = 24 * time.Hour
)

func volatile(out *fuse.EntryOut) {
	out.SetEntryTimeout(volatileTimeout)
	out.SetAttrTimeout(volatileTimeout)
}

func immutable(out *fuse.EntryOut) {
	out.SetEntryTimeout(immutableTimeout)
	out.SetAttrTimeout(immutableTimeout)
}

// rootNode lists clients.
type rootNode struct {
	fs.Inode
	fs *filesystem
}

var (
	_ fs.NodeReaddirer = (*rootNode)(nil)
	_ fs.NodeLookuper  = (*rootNode)(nil)
	_ fs.NodeGetattrer = (*rootNode)(nil)
)

func (n *rootNode) Getattr(_ context.Context, _ fs.FileHandle, out *fuse.AttrOut) syscall.Errno {
	out.Mode = syscall.S_IFDIR | 0o555
	out.SetTimeout(volatileTimeout)
	return 0
}

func (n *rootNode) Readdir(ctx context.Context) (fs.DirStream, syscall.Errno) {
	handles, err := n.fs.snapshots(ctx, "")
	if err != nil {
		return nil, errno(err)
	}
	seen := map[string]struct{}{}
	var entries []fuse.DirEntry
	for _, h := range handles {
		if _, ok := seen[h.ClientID]; ok {
			continue
		}
		seen[h.ClientID] = struct{}{}
		entries = append(entries, fuse.DirEntry{Name: h.ClientID, Mode: syscall.S_IFDIR})
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name < entries[j].Name })
	return fs.NewListDirStream(entries), 0
}

func (n *rootNode) Lookup(ctx context.Context, name string, out *fuse.EntryOut) (*fs.Inode, syscall.Errno) {
	handles, err := n.fs.snapshots(ctx, name)
	if err != nil {
		return nil, errno(err)
	}
	if len(handles) == 0 {
		return nil, syscall.ENOENT
	}
	out.Mode = syscall.S_IFDIR | 0o555
	volatile(out)
	return n.NewInode(ctx, &clientNode{fs: n.fs, client: name}, fs.StableAttr{Mode: syscall.S_IFDIR}), 0
}

// clientNode lists one client's snapshots by timestamp.
type clientNode struct {
	fs.Inode
	fs     *filesystem
	client string
}

var (
	_ fs.NodeReaddirer = (*clientNode)(nil)
	_ fs.NodeLookuper  = (*clientNode)(nil)
	_ fs.NodeGetattrer = (*clientNode)(nil)
)

func (n *clientNode) Getattr(_ context.Context, _ fs.FileHandle, out *fuse.AttrOut) syscall.Errno {
	out.Mode = syscall.S_IFDIR | 0o555
	out.SetTimeout(volatileTimeout)
	return 0
}

func (n *clientNode) Readdir(ctx context.Context) (fs.DirStream, syscall.Errno) {
	handles, err := n.fs.snapshots(ctx, n.client)
	if err != nil {
		return nil, errno(err)
	}
	entries := make([]fuse.DirEntry, 0, len(handles))
	for _, h := range handles {
		entries = append(entries, fuse.DirEntry{Name: snapshot.FormatKeyTime(h.Time), Mode: syscall.S_IFDIR})
	}
	return fs.NewListDirStream(entries), 0
}

func (n *clientNode) Lookup(ctx context.Context, name string, out *fuse.EntryOut) (*fs.Inode, syscall.Errno) {
	key := snapshot.Prefix + n.client + "/" + name
	if _, err := snapshot.ParseKey(key); err != nil {
		return nil, syscall.ENOENT
	}
	if _, err := n.fs.snapshots(ctx, n.client); err != nil { // refreshes the index if this is new
		return nil, errno(err)
	}
	snap, err := n.fs.repo.LoadSnapshot(ctx, key)
	if err != nil {
		if isNotFound(err) {
			return nil, syscall.ENOENT
		}
		n.fs.opts.warn("%s: %v", key, err)
		return nil, errno(err)
	}
	// The snapshot's roots: their locators ("/tmp/x/src", "s3://b/p")
	// are not single directory components. expandRoots turns them into a
	// virtual hierarchy of intermediate directories; the leaves are the
	// roots' own trees (a synthetic dir entry carrying the subtree), a
	// file or symlink source's single entry, or -- for a locator with no
	// components at all -- the root's entries flattened to the top level.
	top, table, err := expandRoots(ctx, n.fs.repo, snap.Roots, snap.TimeNs)
	if err != nil {
		n.fs.opts.warn("%s: %v", key, err)
		return nil, errno(err)
	}
	root := tree.Entry{
		Type: uint8(tree.TypeDir), MetaKind: uint8(tree.MetaPOSIX),
		Mode: tree.Ptr(uint32(iofs.ModeDir | 0o555)), MTimeNs: tree.Ptr(snap.TimeNs),
		CTimeNs: tree.Ptr(snap.TimeNs),
	}
	setAttr(&out.Attr, root)
	immutable(out)
	return n.NewInode(ctx, &dirNode{fs: n.fs, entry: root, synthetic: top, table: table}, fs.StableAttr{Mode: syscall.S_IFDIR}), 0
}

// expandRoots converts a snapshot's roots into a one-level entry list
// plus a table of deeper synthetic levels, mirroring the restore
// mapping's component rules (format-v3-draft.md §9): the locator loses
// its scheme and splits on "/" (empty and "." components dropped). All
// but the last component are synthesized directories; the last one is
// the leaf, whose shape the root tree decides:
//
//   - a directory source: a synthetic DIR entry carrying the root tree
//     as its subtree (children load lazily);
//   - a file or symlink source -- exactly one non-dir entry named like
//     the locator's last component: that entry itself;
//   - a locator with no components ("/" or a bare bucket): the root
//     tree's entries flattened onto the level the parents ended at --
//     the top level, since there are none.
//
// Real entries always win over synthetic ones at the same name.
func expandRoots(ctx context.Context, r *repo.Repository, roots []snapshot.Root, mtimeNs int64) ([]tree.Entry, map[string][]tree.Entry, error) {
	table := make(map[string][]tree.Entry)
	hasName := func(level []tree.Entry, name string) bool {
		for _, e := range level {
			if string(e.Name) == name {
				return true
			}
		}
		return false
	}
	push := func(key string, e tree.Entry) {
		level := table[key]
		if i := slices.IndexFunc(level, func(x tree.Entry) bool {
			return x.Subtree == nil && string(x.Name) == string(e.Name)
		}); i >= 0 {
			level[i] = e // a real entry replaces the synthetic placeholder
		} else {
			table[key] = append(level, e)
		}
	}
	synthetic := func(name string) tree.Entry {
		// Subtree nil marks a synthetic directory whose children live in
		// the table, not in the repository.
		return tree.Entry{
			Name: []byte(name), Type: uint8(tree.TypeDir),
			Mode: tree.Ptr(uint32(iofs.ModeDir | 0o555)), MTimeNs: tree.Ptr(mtimeNs),
		}
	}

	for _, root := range roots {
		comps, err := locatorComponents(root.Path)
		if err != nil {
			return nil, nil, err
		}
		entries, err := r.LoadTreeChain(ctx, root.Tree)
		if err != nil {
			return nil, nil, err
		}
		if len(comps) == 0 {
			// No components at all: flatten the root's contents onto the
			// top level. A file or symlink source cannot happen here (a
			// single entry would still flatten to that entry).
			for _, e := range entries {
				push("", e)
			}
			continue
		}
		last := comps[len(comps)-1]
		leaf := synthetic(last)
		if len(entries) == 1 && tree.NodeType(entries[0].Type) != tree.TypeDir &&
			string(entries[0].Name) == last {
			leaf = entries[0] // a file or symlink source: the entry itself
		} else {
			id := root.Tree
			leaf.Subtree = &id
		}
		parent := ""
		for _, comp := range comps[:len(comps)-1] {
			if !hasName(table[parent], comp) {
				table[parent] = append(table[parent], synthetic(comp))
			}
			parent = parent + "/" + comp
		}
		push(parent, leaf)
	}
	for path, lvl := range table {
		// Sort each level by name: Lookuper binary-searches.
		slices.SortFunc(lvl, func(a, b tree.Entry) int { return bytes.Compare(a.Name, b.Name) })
		table[path] = lvl
	}
	return table[""], table, nil
}

// locatorComponents turns a snapshot root's locator into the path
// components the restore mapping defines: scheme stripped (a "scheme://"
// prefix), split on "/", empty and "." components dropped.
func locatorComponents(path []byte) ([]string, error) {
	rest := path
	if i := bytes.IndexByte(path, ':'); i >= 0 && len(path) >= i+3 && path[i+1] == '/' && path[i+2] == '/' {
		rest = path[i+3:]
	}
	var comps []string
	for _, c := range strings.Split(string(rest), "/") {
		switch c {
		case "", ".":
		case "..":
			comps = append(comps, "__parent__") // hostile locator: literal name, no escape
		default:
			if strings.ContainsAny(c, "/\x00") {
				return nil, fmt.Errorf("mount: %w: locator component %q is not a path component", iofs.ErrInvalid, c)
			}
			comps = append(comps, c)
		}
	}
	return comps, nil
}

// dirNode is a directory inside a snapshot: immutable, loaded once.
// A segmented directory keeps only its LAST segment's ID; load() walks
// the chain and presents the whole directory. Synthetic entries are the
// intermediate directories of absolute-path root names (see expandRoots).
type dirNode struct {
	fs.Inode
	fs    *filesystem
	entry tree.Entry
	// synthetic != nil means load() serves this list instead of reading
	// a tree from the repository: this is one of the intermediate
	// directories of an absolute-path root name (see expandRoots).
	synthetic []tree.Entry
	// virtualPath is this synthetic directory's path within the virtual
	// hierarchy ("/tmp", "/tmp/x"); table holds every synthetic level.
	virtualPath string
	table       map[string][]tree.Entry

	mu      sync.Mutex
	loaded  bool
	entries []tree.Entry
}

var (
	_ fs.NodeReaddirer   = (*dirNode)(nil)
	_ fs.NodeLookuper    = (*dirNode)(nil)
	_ fs.NodeGetattrer   = (*dirNode)(nil)
	_ fs.NodeGetxattrer  = (*dirNode)(nil)
	_ fs.NodeListxattrer = (*dirNode)(nil)
)

// load serves the directory's entries, caching them once they have
// loaded successfully. A failed load is NOT cached: over SFTP or S3 the
// first failure is usually a transient blip, and bricking one directory
// with EIO until remount -- what a sync.Once would do -- turns a hiccup
// into an outage.
func (n *dirNode) load(ctx context.Context) ([]tree.Entry, error) {
	n.mu.Lock()
	defer n.mu.Unlock()
	if n.loaded {
		return n.entries, nil
	}
	if n.synthetic != nil {
		n.entries, n.synthetic = n.synthetic, nil
		n.loaded = true
		return n.entries, nil
	}
	if n.entry.Subtree == nil {
		return nil, fmt.Errorf("directory has no subtree")
	}
	entries, err := n.fs.repo.LoadTreeChain(ctx, *n.entry.Subtree)
	if err != nil {
		return nil, err
	}
	n.entries, n.loaded = entries, true
	return n.entries, nil
}

func (n *dirNode) Getattr(_ context.Context, _ fs.FileHandle, out *fuse.AttrOut) syscall.Errno {
	setAttr(&out.Attr, n.entry)
	out.SetTimeout(immutableTimeout)
	return 0
}

func (n *dirNode) Readdir(ctx context.Context) (fs.DirStream, syscall.Errno) {
	t, err := n.load(ctx)
	if err != nil {
		return nil, errno(err)
	}
	entries := make([]fuse.DirEntry, 0, len(t))
	for _, e := range t {
		entries = append(entries, fuse.DirEntry{Name: string(e.Name), Mode: typeBits(e)})
	}
	return fs.NewListDirStream(entries), 0
}

func (n *dirNode) Lookup(ctx context.Context, name string, out *fuse.EntryOut) (*fs.Inode, syscall.Errno) {
	t, err := n.load(ctx)
	if err != nil {
		return nil, errno(err)
	}
	// Entries are sorted by name (the tree validates that), so this is
	// a binary search.
	want := []byte(name)
	i := sort.Search(len(t), func(i int) bool { return bytes.Compare(t[i].Name, want) >= 0 })
	if i == len(t) || !bytes.Equal(t[i].Name, want) {
		return nil, syscall.ENOENT
	}
	e := t[i]
	setAttr(&out.Attr, e)
	immutable(out)
	var node fs.InodeEmbedder
	switch tree.NodeType(e.Type) {
	case tree.TypeDir:
		if e.Subtree == nil && n.table != nil {
			// A synthetic intermediate directory (part of an absolute
			// root name): its children live in the table under the
			// joined virtual path.
			child := n.virtualPath + "/" + string(e.Name)
			node = &dirNode{fs: n.fs, entry: e, synthetic: n.table[child], virtualPath: child, table: n.table}
		} else {
			node = &dirNode{fs: n.fs, entry: e}
		}
	case tree.TypeFile:
		node = &fileNode{fs: n.fs, entry: e}
	case tree.TypeSymlink:
		node = &symlinkNode{entry: e}
	default:
		return nil, syscall.EIO
	}
	return n.NewInode(ctx, node, fs.StableAttr{Mode: typeBits(e)}), 0
}

func (n *dirNode) Getxattr(_ context.Context, attr string, dest []byte) (uint32, syscall.Errno) {
	return getxattr(n.entry, attr, dest)
}

func (n *dirNode) Listxattr(_ context.Context, dest []byte) (uint32, syscall.Errno) {
	return listxattr(n.entry, dest)
}

// symlinkNode is a symbolic link.
type symlinkNode struct {
	fs.Inode
	entry tree.Entry
}

var (
	_ fs.NodeReadlinker = (*symlinkNode)(nil)
	_ fs.NodeGetattrer  = (*symlinkNode)(nil)
)

func (n *symlinkNode) Getattr(_ context.Context, _ fs.FileHandle, out *fuse.AttrOut) syscall.Errno {
	setAttr(&out.Attr, n.entry)
	out.Size = uint64(len(n.entry.Target))
	out.SetTimeout(immutableTimeout)
	return 0
}

func (n *symlinkNode) Readlink(context.Context) ([]byte, syscall.Errno) {
	return n.entry.Target, 0
}

// fileNode is a regular file. An indirect entry's chunk list is
// resolved on first read. Chunk plaintext lengths ARE in the format
// (raw_len, new in v2), but reaching them needs the index, so the
// offset of chunk i is still learnt by decoding chunks 0..i once; ends
// remembers what has been learnt, shared by every open of the file.
type fileNode struct {
	fs.Inode
	fs    *filesystem
	entry tree.Entry

	chunks []crypto.ID
	// resolved marks a successful resolve; failures are retried on the
	// next call (see dirNode.load for why an error must not be cached).
	resolved bool

	mu   sync.Mutex
	ends []uint64 // ends[i] = plaintext offset just past chunk i
}

// resolve produces the data chunk list, following indirect entries.
func (n *fileNode) resolve(ctx context.Context) ([]crypto.ID, error) {
	n.mu.Lock()
	defer n.mu.Unlock()
	if n.resolved {
		return n.chunks, nil
	}
	if tree.ContentType(n.entry.ContentType) == tree.ContentIndirect {
		var buf []byte
		for _, id := range n.entry.Chunks {
			data, err := n.fs.chunk(ctx, id)
			if err != nil {
				return nil, err
			}
			buf = append(buf, data...)
		}
		var list tree.ChunkList
		if err := crypto.Unmarshal(buf, &list); err != nil {
			return nil, err
		}
		n.chunks, n.resolved = list.Chunks, true
		return n.chunks, nil
	}
	n.chunks, n.resolved = n.entry.Chunks, true
	return n.chunks, nil
}

var (
	_ fs.NodeGetattrer   = (*fileNode)(nil)
	_ fs.NodeOpener      = (*fileNode)(nil)
	_ fs.NodeReader      = (*fileNode)(nil)
	_ fs.NodeGetxattrer  = (*fileNode)(nil)
	_ fs.NodeListxattrer = (*fileNode)(nil)
)

func (n *fileNode) Getattr(_ context.Context, _ fs.FileHandle, out *fuse.AttrOut) syscall.Errno {
	setAttr(&out.Attr, n.entry)
	out.SetTimeout(immutableTimeout)
	return 0
}

func (n *fileNode) Open(_ context.Context, flags uint32) (fs.FileHandle, uint32, syscall.Errno) {
	if flags&(syscall.O_WRONLY|syscall.O_RDWR) != 0 {
		return nil, 0, syscall.EROFS
	}
	return nil, fuse.FOPEN_KEEP_CACHE, 0
}

func (n *fileNode) Read(ctx context.Context, _ fs.FileHandle, dest []byte, off int64) (fuse.ReadResult, syscall.Errno) {
	if off < 0 {
		return nil, syscall.EINVAL
	}
	chunks, err := n.resolve(ctx)
	if err != nil {
		n.fs.opts.warn("read %s: %v", n.entry.Name, err)
		return nil, errno(err)
	}

	n.mu.Lock()
	defer n.mu.Unlock()

	// Which chunk holds off? Decode forward until the table reaches it.
	start := uint64(off)
	i := 0
	for {
		if i < len(n.ends) {
			if start < n.ends[i] {
				break
			}
			i++
			continue
		}
		if i >= len(chunks) {
			return fuse.ReadResultData(nil), 0 // past the end
		}
		data, err := n.fs.chunk(ctx, chunks[i])
		if err != nil {
			n.fs.opts.warn("read %s: %v", n.entry.Name, err)
			return nil, errno(err)
		}
		var prev uint64
		if i > 0 {
			prev = n.ends[i-1]
		}
		n.ends = append(n.ends, prev+uint64(len(data))) //nolint:gosec // a length is never negative
	}

	// Copy from chunk i onwards until dest is full or the file ends.
	filled := 0
	pos := start
	for filled < len(dest) && i < len(chunks) {
		data, err := n.fs.chunk(ctx, chunks[i])
		if err != nil {
			n.fs.opts.warn("read %s: %v", n.entry.Name, err)
			return nil, errno(err)
		}
		var chunkStart uint64
		if i > 0 {
			chunkStart = n.ends[i-1]
		}
		if i == len(n.ends) {
			n.ends = append(n.ends, chunkStart+uint64(len(data))) //nolint:gosec // a length is never negative
		}
		filled += copy(dest[filled:], data[pos-chunkStart:])
		pos = start + uint64(filled) //nolint:gosec // a length is never negative
		i++
	}
	return fuse.ReadResultData(dest[:filled]), 0
}

func (n *fileNode) Getxattr(_ context.Context, attr string, dest []byte) (uint32, syscall.Errno) {
	return getxattr(n.entry, attr, dest)
}

func (n *fileNode) Listxattr(_ context.Context, dest []byte) (uint32, syscall.Errno) {
	return listxattr(n.entry, dest)
}

func getxattr(e tree.Entry, attr string, dest []byte) (uint32, syscall.Errno) {
	for _, kv := range e.Xattrs {
		if string(kv.Name) == attr {
			if len(dest) < len(kv.Value) {
				return uint32(len(kv.Value)), syscall.ERANGE //nolint:gosec // an xattr value is far below 4 GiB
			}
			return uint32(copy(dest, kv.Value)), 0 //nolint:gosec // bounded by len(dest), a kernel buffer
		}
	}
	return 0, syscall.ENODATA
}

func listxattr(e tree.Entry, dest []byte) (uint32, syscall.Errno) {
	names := make([]string, 0, len(e.Xattrs))
	for _, kv := range e.Xattrs {
		names = append(names, string(kv.Name))
	}
	sort.Strings(names)
	var need int
	for _, k := range names {
		need += len(k) + 1
	}
	if len(dest) < need {
		return uint32(need), syscall.ERANGE
	}
	n := 0
	for _, k := range names {
		n += copy(dest[n:], k)
		dest[n] = 0
		n++
	}
	return uint32(n), 0
}

// setAttr fills a FUSE attribute block from a tree entry. Fields the
// source did not record leave the attribute at its zero: absent must not
// become a fabricated value. Hard links come out as separate files with
// one link each: the mount serves bytes and modes, not inode identity.
func setAttr(a *fuse.Attr, e tree.Entry) {
	a.Mode = fuseMode(e.FileMode())
	a.Size = e.Size
	a.Nlink = 1
	if e.UID != nil {
		a.Uid = *e.UID
	}
	if e.GID != nil {
		a.Gid = *e.GID
	}
	mtime := time.Unix(0, deref64s(e.MTimeNs))
	ctime := mtime
	if e.CTimeNs != nil {
		ctime = time.Unix(0, *e.CTimeNs)
	}
	a.SetTimes(&mtime, &mtime, &ctime)
	if tree.NodeType(e.Type) == tree.TypeFile {
		a.Blocks = (e.Size + 511) / 512
	}
}

func deref64s(p *int64) int64 {
	if p == nil {
		return 0
	}
	return *p
}

// typeBits is the file-type part of a mode, for directory entries and
// stable attributes.
func typeBits(e tree.Entry) uint32 {
	switch tree.NodeType(e.Type) {
	case tree.TypeDir:
		return syscall.S_IFDIR
	case tree.TypeSymlink:
		return syscall.S_IFLNK
	case tree.TypeFile:
		return syscall.S_IFREG
	default:
		return syscall.S_IFREG
	}
}

// fuseMode converts Go's fs.FileMode -- type in the high bits, setuid
// and friends as flags -- to the st_mode layout FUSE expects.
func fuseMode(m iofs.FileMode) uint32 {
	out := uint32(m.Perm())
	switch {
	case m&iofs.ModeDir != 0:
		out |= syscall.S_IFDIR
	case m&iofs.ModeSymlink != 0:
		out |= syscall.S_IFLNK
	default:
		out |= syscall.S_IFREG
	}
	if m&iofs.ModeSetuid != 0 {
		out |= syscall.S_ISUID
	}
	if m&iofs.ModeSetgid != 0 {
		out |= syscall.S_ISGID
	}
	if m&iofs.ModeSticky != 0 {
		out |= syscall.S_ISVTX
	}
	return out
}

func isNotFound(err error) bool {
	return errors.Is(err, iofs.ErrNotExist) || errorsIsNotFound(err)
}

func errno(err error) syscall.Errno {
	switch {
	case err == nil:
		return 0
	case isNotFound(err):
		return syscall.ENOENT
	case errors.Is(err, context.Canceled), errors.Is(err, context.DeadlineExceeded):
		return syscall.EINTR
	default:
		return syscall.EIO
	}
}

// lru is a small cache of decoded chunks.
type lru struct {
	mu    sync.Mutex
	cap   int
	order *list.List
	items map[crypto.ID]*list.Element
}

type lruItem struct {
	id   crypto.ID
	data []byte
}

func newLRU(capacity int) *lru {
	return &lru{cap: capacity, order: list.New(), items: make(map[crypto.ID]*list.Element)}
}

func (c *lru) get(id crypto.ID) ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	el, ok := c.items[id]
	if !ok {
		return nil, false
	}
	c.order.MoveToFront(el)
	item, ok := el.Value.(*lruItem)
	if !ok {
		return nil, false
	}
	return item.data, true
}

func (c *lru) put(id crypto.ID, data []byte) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if el, ok := c.items[id]; ok {
		c.order.MoveToFront(el)
		return
	}
	c.items[id] = c.order.PushFront(&lruItem{id: id, data: data})
	for c.order.Len() > c.cap {
		last := c.order.Back()
		if item, ok := last.Value.(*lruItem); ok {
			delete(c.items, item.id)
		}
		c.order.Remove(last)
	}
}
