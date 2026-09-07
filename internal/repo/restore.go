package repo

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/snapshot"
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
func (r *Repository) Restore(ctx context.Context, key, target string, opts RestoreOptions) (RestoreStats, error) {
	snap, err := snapshot.Load(ctx, r.backend, r.keys, key)
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
		chunks: r.NewChunkSource(),
		links:  make(map[hardLinkKey]string),
	}
	if err := run.restoreTree(ctx, snap.Root, abs); err != nil {
		return run.stats, err
	}
	return run.stats, nil
}

type restoreRun struct {
	repo *Repository
	opts RestoreOptions

	chunks *ChunkSource

	// links maps an inode seen in the snapshot to the first path it was
	// restored to, so the second name becomes a hard link rather than a
	// second copy.
	links map[hardLinkKey]string

	stats RestoreStats
}

func (run *restoreRun) restoreTree(ctx context.Context, id crypto.ID, dir string) error {
	t, err := tree.Load(ctx, run.repo.backend, run.repo.keys, id)
	if err != nil {
		return fmt.Errorf("restore: %w", err)
	}
	run.stats.Dirs++

	if t.Prev != nil {
		// A segmented directory: earlier segments come first on disk, so
		// they are restored before this one's entries.
		if err := run.restoreTree(ctx, *t.Prev, dir); err != nil {
			return err
		}
	}

	for _, entry := range t.Entries {
		if err := ctx.Err(); err != nil {
			return err
		}
		path, err := entryPath(dir, entry.Name)
		if err != nil {
			return err
		}

		switch tree.NodeType(entry.Type) {
		case tree.TypeDir:
			if entry.Subtree == nil {
				return fmt.Errorf("restore: %w: entry %q has no subtree", tree.ErrCorrupt, entry.Name)
			}
			if err := os.MkdirAll(path, 0o700); err != nil {
				return fmt.Errorf("restore: %w", err)
			}
			if err := run.restoreTree(ctx, *entry.Subtree, path); err != nil {
				return err
			}
			// Permissions last: a read-only directory cannot be filled.
			if err := run.applyMetadata(path, entry, false); err != nil {
				return err
			}

		case tree.TypeSymlink:
			// A root-level entry's name is an absolute path; its parents
			// do not exist yet under the target.
			if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
				return fmt.Errorf("restore: %w", err)
			}
			if err := os.Symlink(string(entry.Target), path); err != nil {
				return fmt.Errorf("restore: %w", err)
			}
			run.stats.Symlinks++
			if err := run.applyMetadata(path, entry, true); err != nil {
				return err
			}

		case tree.TypeFile:
			if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
				return fmt.Errorf("restore: %w", err)
			}
			if err := run.restoreFile(ctx, entry, path); err != nil {
				return err
			}
			if err := run.applyMetadata(path, entry, false); err != nil {
				return err
			}

		default:
			return fmt.Errorf("restore: %w: entry %q has unknown type %d", tree.ErrCorrupt, entry.Name, entry.Type)
		}
	}
	return nil
}

// safeJoin builds a path for one tree entry inside dir, refusing anything
// that would land outside it.
//
// tree.validate already rejects names containing a separator, a NUL, "."
// or "..", so a tree that reached here cannot carry a traversal. This
// checks again anyway, because the two guards protect against different
// things: that one keeps kist from *writing* a bad tree, this one keeps a
// repository someone else controls from making a restore write outside
// the directory the user named. A restore is the moment an attacker who
// owns the repository gets to choose filenames on the victim's machine.
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

// entryPath places one tree entry under dir. Child entries are single
// components. A root-tree entry's name is the backup source's absolute
// path -- that is the v2 naming rule -- and lands under dir component by
// component, each component checked exactly as a child name is.
func entryPath(dir string, name []byte) (string, error) {
	if len(name) > 0 && name[0] == '/' {
		clean := strings.Trim(string(name), "/")
		path := dir
		for _, comp := range strings.Split(clean, "/") {
			var err error
			path, err = safeJoin(path, comp)
			if err != nil {
				return "", err
			}
		}
		return path, nil
	}
	return safeJoin(dir, string(name))
}

func (run *restoreRun) restoreFile(ctx context.Context, entry tree.Entry, path string) error {
	if entry.Links > 1 {
		key := hardLinkKey{device: entry.Device, inode: entry.Inode}
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
// cannot do rather than failing the restore over it.
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
	if entry.UID != 0 || entry.GID != 0 {
		if err := chown(path, entry.UID, entry.GID); err != nil {
			run.opts.warn("could not restore ownership of %s (uid %d gid %d): %v", path, entry.UID, entry.GID, err)
		}
	}

	// Perm() alone would drop the same three bits for a different reason.
	mode := entry.FileMode() & (fs.ModePerm | fs.ModeSetuid | fs.ModeSetgid | fs.ModeSticky)
	// path came from safeJoin, which rejects anything that is not a
	// single component inside the parent directory.
	if err := os.Chmod(path, mode); err != nil { //nolint:gosec // path is bounded by safeJoin
		return fmt.Errorf("restore: set mode on %s: %w", path, err)
	}

	// Times last: chmod does not touch them, but chown updates ctime and
	// a future writer here would.
	if entry.MTimeNs != 0 {
		at := time.Unix(0, entry.MTimeNs)
		if err := os.Chtimes(path, at, at); err != nil {
			run.opts.warn("could not set the modification time of %s: %v", path, err)
		}
	}

	return nil
}
