package repo

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/tree"
)

// RestoreOptions configure one restore.
type RestoreOptions struct {
	// Warnf receives what could not be restored faithfully -- ownership
	// without privileges, extended attributes, a hard link that had to
	// become a copy. A restore that quietly does less than it claims is
	// worse than one that says what it did.
	Warnf func(format string, args ...any)
}

func (o RestoreOptions) warn(format string, args ...any) {
	if o.Warnf != nil {
		o.Warnf(format, args...)
	}
}

// RestoreStats summarise what a restore wrote.
type RestoreStats struct {
	Files    uint64
	Dirs     uint64
	Symlinks uint64
	Links    uint64
	Bytes    uint64
}

// Restore writes a snapshot's contents into target.
//
// target must not already exist, or must be an empty directory. Restoring
// over live data is not something a backup tool should do by inference.
//
// The v3 mapping (format-v3-draft.md §9): each root's locator loses its
// scheme and is split on "/" (empty components dropped), and the source's
// entries land under target/<relative path>. A root whose tree holds
// exactly one non-directory entry named like the locator's last
// component is a file or symlink SOURCE: it lands at
// target/<locator-minus-last>/<name>, the same place the v2
// absolute-path restore put it. Hard-link identity is snapshot-wide:
// two names of one inode are re-linked even across roots.
func (r *Repository) Restore(ctx context.Context, key, target string, opts RestoreOptions) (RestoreStats, error) {
	snap, err := r.LoadSnapshot(ctx, key)
	if err != nil {
		return RestoreStats{}, err
	}

	abs, err := filepath.Abs(target)
	if err != nil {
		return RestoreStats{}, fmt.Errorf("restore: resolve %s: %w", target, err)
	}
	switch entries, err := os.ReadDir(abs); {
	case err == nil && len(entries) > 0:
		return RestoreStats{}, fmt.Errorf("restore: %s is not empty", abs)
	case err != nil && !errors.Is(err, fs.ErrNotExist):
		return RestoreStats{}, fmt.Errorf("restore: %w", err)
	}
	if err := os.MkdirAll(abs, 0o700); err != nil {
		return RestoreStats{}, fmt.Errorf("restore: %w", err)
	}

	run := &restoreRun{
		repo:   r,
		opts:   opts,
		target: abs,
		chunks: r.NewChunkSource(),
		links:  make(map[hardLinkKey]string),
	}
	for _, root := range snap.Roots {
		if err := ctx.Err(); err != nil {
			return run.stats, err
		}
		rel, err := locatorComponents(root.Path)
		if err != nil {
			return run.stats, err
		}
		entries, err := r.LoadTreeChain(ctx, root.Tree)
		if err != nil {
			return run.stats, fmt.Errorf("restore: %w", err)
		}
		base, err := run.rootBase(abs, rel, entries)
		if err != nil {
			return run.stats, err
		}
		if err := run.mkdirAllNoFollow(base); err != nil {
			return run.stats, err
		}
		for _, entry := range entries {
			if err := ctx.Err(); err != nil {
				return run.stats, err
			}
			path, err := safeJoin(base, string(entry.Name))
			if err != nil {
				return run.stats, err
			}
			if err := run.restoreNode(ctx, entry, path); err != nil {
				return run.stats, err
			}
		}
	}
	return run.stats, nil
}

// locatorComponents turns a root's locator bytes into the components of
// its path under the restore target: the "scheme://" prefix goes, the
// rest splits on "/", empty and "." components disappear, and a ".."
// component cannot mean anything hostile here, so it becomes a literal
// name -- the mapping is locked by cross-language vectors and must not
// escape the target.
func locatorComponents(locator []byte) ([]string, error) {
	rest := locator
	if i := bytes.IndexByte(locator, ':'); i >= 0 && len(locator) >= i+3 && locator[i+1] == '/' && locator[i+2] == '/' {
		rest = locator[i+3:]
	}
	var comps []string
	for _, c := range strings.Split(string(rest), "/") {
		switch c {
		case "", ".":
		case "..":
			comps = append(comps, "__parent__")
		default:
			if strings.ContainsAny(c, "/\x00") {
				return nil, fmt.Errorf("restore: %w: locator component %q is not a path component", tree.ErrCorrupt, c)
			}
			comps = append(comps, c)
		}
	}
	return comps, nil
}

// rootBase decides where one root's entries land, mirroring the
// cross-language restore mapping: a directory source's entries go under
// target/<locator>; a file or symlink source -- the root tree holds
// exactly one non-directory entry named like the locator's last
// component -- lands one level up, so the file's path is
// target/<locator>/<name> either way.
func (run *restoreRun) rootBase(target string, comps []string, entries []tree.Entry) (string, error) {
	fileRoot := len(comps) > 0 &&
		len(entries) == 1 &&
		tree.NodeType(entries[0].Type) != tree.TypeDir &&
		string(entries[0].Name) == comps[len(comps)-1]
	if fileRoot {
		comps = comps[:len(comps)-1]
	}
	path := target
	for _, c := range comps {
		var err error
		if path, err = safeJoin(path, c); err != nil {
			return "", err
		}
	}
	return path, nil
}

type restoreRun struct {
	repo *Repository
	opts RestoreOptions

	// target is the restore root the user named, already created: every
	// path below it is snapshot content, and mkdirAllNoFollow trusts
	// nothing below it.
	target string

	chunks *ChunkSource

	// links maps an inode seen in the snapshot to the first path it was
	// restored to, so the second name becomes a hard link rather than a
	// second copy. The map lives on the run, not per root: hard-link
	// identity is snapshot-wide across roots (format-v3-draft.md §8.3).
	links map[hardLinkKey]string

	stats RestoreStats
}

// restoreNode restores one tree entry to path. Errors abort: a missing
// chunk inside a snapshot means the snapshot is not restorable as a
// whole, and pretending otherwise would be worse than stopping.
func (run *restoreRun) restoreNode(ctx context.Context, entry tree.Entry, path string) error {
	switch tree.NodeType(entry.Type) {
	case tree.TypeDir:
		if entry.Subtree == nil {
			return fmt.Errorf("restore: %w: entry %q has no subtree", tree.ErrCorrupt, entry.Name)
		}
		if err := run.mkdirAllNoFollow(path); err != nil {
			return fmt.Errorf("restore: %w", err)
		}
		children, err := run.repo.LoadTreeChain(ctx, *entry.Subtree)
		if err != nil {
			return fmt.Errorf("restore: %w", err)
		}
		for _, child := range children {
			if err := ctx.Err(); err != nil {
				return err
			}
			childPath, err := safeJoin(path, string(child.Name))
			if err != nil {
				return err
			}
			if err := run.restoreNode(ctx, child, childPath); err != nil {
				return err
			}
		}
		run.stats.Dirs++
		// Permissions last: a read-only directory cannot be filled.
		return run.applyMetadata(path, entry, false)

	case tree.TypeSymlink:
		if err := run.mkdirAllNoFollow(filepath.Dir(path)); err != nil {
			return fmt.Errorf("restore: %w", err)
		}
		if err := os.Symlink(string(entry.Target), path); err != nil {
			return fmt.Errorf("restore: %w", err)
		}
		run.stats.Symlinks++
		return run.applyMetadata(path, entry, true)

	case tree.TypeFile:
		if err := run.mkdirAllNoFollow(filepath.Dir(path)); err != nil {
			return fmt.Errorf("restore: %w", err)
		}
		if err := run.restoreFile(ctx, entry, path); err != nil {
			return err
		}
		return run.applyMetadata(path, entry, false)

	default:
		return fmt.Errorf("restore: %w: entry %q has unknown type %d", tree.ErrCorrupt, entry.Name, entry.Type)
	}
}

// safeJoin builds a path for one tree entry inside dir, refusing anything
// that would land outside it.
//
// tree.Validate already rejects names containing a separator, ".",
// ".." or the empty name, so a tree that reached here cannot carry a
// traversal. This checks again anyway, because the two guards protect
// against different things: that one keeps kist from *writing* a bad
// tree, this one keeps a repository someone else controls from making a
// restore write outside the directory the user named. A restore is the
// moment an attacker who owns the repository gets to choose filenames on
// the victim's machine.
func safeJoin(dir, name string) (string, error) {
	if name == "" || name == "." || name == ".." || strings.ContainsAny(name, "/\x00") || strings.ContainsRune(name, os.PathSeparator) {
		return "", fmt.Errorf("restore: %w: entry name %q is not a single path component", tree.ErrCorrupt, name)
	}

	path := filepath.Join(dir, name)
	within, err := filepath.Rel(dir, path)
	if err != nil || within != name {
		return "", fmt.Errorf("restore: %w: entry name %q escapes %s", tree.ErrCorrupt, name, dir)
	}
	return path, nil
}

// mkdirAllNoFollow creates dir and any missing parents below the
// restore target, refusing to create or pass through a symlink in any
// component from the target downward. A symlink there can only have
// been planted by this very snapshot -- restore starts from an empty
// target -- and os.MkdirAll would follow it, turning the snapshot's own
// content into a write outside the directory the user named; this gives
// directory paths the protection the file path's O_EXCL open already
// has. The target itself and everything above it are the user's own
// path and stay trusted.
func (run *restoreRun) mkdirAllNoFollow(dir string) error {
	rel, err := filepath.Rel(run.target, dir)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(os.PathSeparator)) {
		return fmt.Errorf("restore: %s is not under the restore target %s", dir, run.target)
	}
	cur := run.target
	for _, comp := range strings.Split(rel, string(os.PathSeparator)) {
		if comp == "" || comp == "." {
			continue
		}
		cur = filepath.Join(cur, comp)
		info, err := os.Lstat(cur)
		switch {
		case err == nil:
			if info.Mode()&fs.ModeSymlink != 0 {
				return fmt.Errorf("restore: %w: %s is a symlink", tree.ErrCorrupt, cur)
			}
			if !info.IsDir() {
				return fmt.Errorf("restore: %s exists and is not a directory", cur)
			}
		case errors.Is(err, fs.ErrNotExist):
			if err := os.Mkdir(cur, 0o700); err != nil {
				if !errors.Is(err, fs.ErrExist) {
					return fmt.Errorf("restore: %w", err)
				}
				info, err := os.Lstat(cur)
				if err != nil {
					return fmt.Errorf("restore: %w", err)
				}
				if info.Mode()&fs.ModeSymlink != 0 {
					return fmt.Errorf("restore: %w: %s is a symlink", tree.ErrCorrupt, cur)
				}
				if !info.IsDir() {
					return fmt.Errorf("restore: %s exists and is not a directory", cur)
				}
			}
		default:
			return fmt.Errorf("restore: %w", err)
		}
	}
	return nil
}

func (run *restoreRun) restoreFile(ctx context.Context, entry tree.Entry, path string) error {
	if entry.Links != nil && *entry.Links > 1 {
		key := hardLinkKey{device: deref64(entry.Device), inode: deref64(entry.Inode)}
		if first, seen := run.links[key]; seen {
			if err := os.Link(first, path); err == nil {
				run.stats.Links++
				return nil
			} else { //nolint:revive // the fallback is the point, not an else-branch style issue
				run.opts.warn("%s was a hard link to %s; restoring it as a copy: %v", path, first, err)
			}
		} else {
			defer func() { run.links[key] = path }()
		}
	}

	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600) //nolint:gosec // path is under the restore target
	if err != nil {
		return fmt.Errorf("restore: %w", err)
	}
	defer func() { _ = f.Close() }()

	chunkIDs := entry.Chunks
	if tree.ContentType(entry.ContentType) == tree.ContentIndirect {
		ids, err := run.resolveChunkList(ctx, entry.Chunks)
		if err != nil {
			return fmt.Errorf("restore %s: %w", path, err)
		}
		chunkIDs = ids
	}

	var written uint64
	for _, chunkID := range chunkIDs {
		data, err := run.chunk(ctx, chunkID)
		if err != nil {
			return fmt.Errorf("restore %s: %w", path, err)
		}
		n, err := f.Write(data)
		if err != nil {
			return fmt.Errorf("restore %s: %w", path, err)
		}
		written += uint64(n) //nolint:gosec // io.Writer forbids a negative n without an error
	}

	if written != entry.Size {
		return fmt.Errorf("restore %s: wrote %d bytes, the snapshot says %d", path, written, entry.Size)
	}
	if err := f.Close(); err != nil {
		return fmt.Errorf("restore %s: %w", path, err)
	}

	run.stats.Files++
	run.stats.Bytes += written
	return nil
}

func deref64(p *uint64) uint64 {
	if p == nil {
		return 0
	}
	return *p
}

func deref32(p *uint32) uint32 {
	if p == nil {
		return 0
	}
	return *p
}

// chunk fetches one chunk, reusing an open pack reader when it can.
func (run *restoreRun) chunk(ctx context.Context, id crypto.ID) ([]byte, error) {
	return run.chunks.Chunk(ctx, id)
}

// resolveChunkList reassembles and decodes an indirect chunk list.
func (run *restoreRun) resolveChunkList(ctx context.Context, chunks []crypto.ID) ([]crypto.ID, error) {
	var buf []byte
	for _, id := range chunks {
		data, err := run.chunk(ctx, id)
		if err != nil {
			return nil, err
		}
		buf = append(buf, data...)
	}
	var list tree.ChunkList
	if err := crypto.Unmarshal(buf, &list); err != nil {
		return nil, fmt.Errorf("%w: chunk list: %w", tree.ErrCorrupt, err)
	}
	if list.Version != tree.Version {
		return nil, fmt.Errorf("%w: chunk list declares version %d, this build reads %d", tree.ErrCorrupt, list.Version, tree.Version)
	}
	return list.Chunks, nil
}

// applyMetadata restores mode, times and ownership, warning about what it
// cannot do rather than failing the restore over it. A field the source
// did not record (a nil pointer, or mode 0 from a mode-less platform) is
// left alone: absent must not become zero.
func (run *restoreRun) applyMetadata(path string, entry tree.Entry, isSymlink bool) error {
	if len(entry.Xattrs) > 0 {
		run.opts.warn("%s had %d extended attributes; restoring them is not implemented", path, len(entry.Xattrs))
	}
	if isSymlink {
		// A symlink's own mode and times are not portably settable, and
		// nothing depends on them.
		return nil
	}

	// Ownership first, mode second. POSIX chown clears the setuid and
	// setgid bits, so doing it the other way round silently strips them:
	//
	//	after chmod:  ugrwxr-xr-x
	//	after lchown: -rwxr-xr-x
	//
	// A restored binary that quietly lost its setuid bit is a system that
	// does not work and does not say why.
	uid, gid := deref32(entry.UID), deref32(entry.GID)
	if entry.UID != nil || entry.GID != nil {
		if uid != 0 || gid != 0 {
			if err := chown(path, uid, gid); err != nil {
				run.opts.warn("could not restore ownership of %s (uid %d gid %d): %v", path, uid, gid, err)
			}
		}
	}

	// Perm() alone would drop the same three bits for a different reason.
	// Mode 0 means "not recorded" as much as nil does (Rust restores
	// nothing for it either).
	if mode := entry.FileMode(); mode != 0 {
		mode &= fs.ModePerm | fs.ModeSetuid | fs.ModeSetgid | fs.ModeSticky
		// path came from safeJoin, which rejects anything that is not a
		// single component inside the parent directory.
		if err := os.Chmod(path, mode); err != nil { //nolint:gosec // path is bounded by safeJoin
			return fmt.Errorf("restore: set mode on %s: %w", path, err)
		}
	}

	// Times last: chmod does not touch them, but chown updates ctime and
	// a future writer here would.
	if entry.MTimeNs != nil && *entry.MTimeNs != 0 {
		at := time.Unix(0, *entry.MTimeNs)
		if err := os.Chtimes(path, at, at); err != nil {
			run.opts.warn("could not set the modification time of %s: %v", path, err)
		}
	}

	return nil
}
