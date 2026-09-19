package repo

import (
	"bytes"
	"context"
	"fmt"

	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/source"
	"github.com/at-least/kist/internal/tree"
)

// findParent returns this client's newest snapshot when it is a valid
// parent for this backup: one whose roots are exactly the locators being
// backed up now, same paths in the same order (format-v3-draft.md §9).
// Anything else -- a different path set, an unreadable object -- means
// no parent, and the backup simply re-reads everything.
//
// The parent is an accelerator only: nothing reads it back, and the
// snapshot records it so humans (and other clients' fast paths) can see
// the lineage.
func (r *Repository) findParent(ctx context.Context, locators [][]byte) (string, *snapshot.Snapshot, error) {
	handles, err := r.Snapshots(ctx, r.clientIDHex)
	if err != nil {
		return "", nil, fmt.Errorf("backup: %w", err)
	}
	if len(handles) == 0 {
		return "", nil, nil
	}
	// Oldest first; the newest is the only candidate.
	latest := handles[len(handles)-1]
	snap, err := r.LoadSnapshot(ctx, latest.Key)
	if err != nil {
		r.warnf("cannot read previous snapshot %s: %v; not using it as parent", latest.Key, err)
		return "", nil, nil
	}
	if len(snap.Roots) != len(locators) {
		return "", nil, nil
	}
	for i, root := range snap.Roots {
		if !bytes.Equal(root.Path, locators[i]) {
			return "", nil, nil
		}
	}
	return latest.Key, snap, nil
}

// A parentStream consults the previous snapshot's tree with a merge-join
// cursor: the walker asks for names in ascending order, the stream walks
// its segments (each at most tree.MaxNodesPerTree entries, linked by
// Prev) in step, and only one segment is ever resident. Building a map
// of a parent directory instead would put a million-entry directory's
// whole metadata in memory twice over -- the large-repository memory
// gate.
type parentStream struct {
	repo *Repository

	parts   []crypto.ID  // segment IDs not yet scanned, oldest first
	current []tree.Entry // the segment being scanned
	pos     int          // cursor into current
	dead    bool         // a segment that would not load stops all reuse
}

// openParentStream resolves a tree chain into its segment IDs, oldest
// first. The entries are not held: they arrive segment by segment as the
// walk asks for them.
func openParentStream(ctx context.Context, r *Repository, last crypto.ID) (*parentStream, error) {
	var rev []crypto.ID
	seen := make(map[crypto.ID]struct{})
	for next := last; ; {
		if _, dup := seen[next]; dup {
			return nil, fmt.Errorf("tree chain loops at %s", next)
		}
		seen[next] = struct{}{}
		rev = append(rev, next)
		t, err := r.readTree(ctx, next)
		if err != nil {
			return nil, err
		}
		if t.Prev == nil {
			break
		}
		next = *t.Prev
	}
	s := &parentStream{repo: r}
	for i := len(rev) - 1; i >= 0; i-- {
		s.parts = append(s.parts, rev[i])
	}
	return s, nil
}

// takeName returns the parent's entry for name, or nil when the chain
// has none. Callers must ask for ascending names. A segment that fails
// to load disables the stream: the walker re-reads those files, the same
// "cannot read the parent, rebuild" answer a missing tree gets.
func (s *parentStream) takeName(ctx context.Context, name []byte) *tree.Entry {
	if s == nil || s.dead {
		return nil
	}
	for {
		for s.pos < len(s.current) && bytes.Compare(s.current[s.pos].Name, name) < 0 {
			s.pos++
		}
		if s.pos < len(s.current) {
			e := &s.current[s.pos]
			switch bytes.Compare(e.Name, name) {
			case 0:
				s.pos++
				return e
			case 1:
				return nil // not in the chain; the cursor stays for bigger names
			}
		}
		if len(s.parts) == 0 {
			return nil
		}
		next := s.parts[0]
		s.parts = s.parts[1:]
		t, err := s.repo.readTree(ctx, next)
		if err != nil {
			s.dead = true
			s.repo.warnf("cannot read parent tree %s: %v; parent reuse disabled", next, err)
			return nil
		}
		s.current = t.Entries
		s.pos = 0
	}
}

// fileFacts is what a listed file tells the fast path: the size at
// listing time, the posix metadata when the source is local, and the
// content fingerprint when the source vouches for one.
type fileFacts struct {
	size  uint64
	posix *source.PosixMeta
	etag  []byte
}

// posixUnchanged reports whether a file's kernel-maintained metadata
// matches what the parent snapshot recorded, and is old enough to
// believe. A file modified within the parent backup's own time window
// can look unchanged and be different -- the "racily clean" trap -- so
// its timestamps must fall clearly before the parent's start (the racy
// guard, format-v3-draft.md §8.2). ctime and inode are compared only
// when the parent recorded them: a platform without them, or a
// single-link file (whose identity is not recorded), must not fail the
// comparison for a field that was never there.
func posixUnchanged(parent *tree.Entry, now *source.PosixMeta, parentStartNs int64) bool {
	if parent.MTimeNs == nil || *parent.MTimeNs != now.MTimeNs {
		return false
	}
	hasCTime := parent.CTimeNs != nil && *parent.CTimeNs != 0
	if hasCTime && *parent.CTimeNs != now.CTimeNs {
		return false
	}
	if parent.Inode != nil && *parent.Inode != 0 && *parent.Inode != now.Inode {
		return false
	}
	if now.MTimeNs >= parentStartNs {
		return false
	}
	if hasCTime && now.CTimeNs >= parentStartNs {
		return false
	}
	return true
}

// reusableChunk is one chunk the fast path would carry over, with the
// pack that holds it.
type reusableChunk struct {
	id   crypto.ID
	pack crypto.ID
}

// proven reports whether the file may be reused from parent without
// being read, graded by what each metadata family can prove
// (format-v3-draft.md §8.2): posix by the kernel's own bookkeeping, s3
// by the source's etag, and nothing at all for sftp/generic -- their
// mtime is a claim, so the chunk dedup absorbs the re-read. A kind
// mismatch (the same path backed up from a different kind of source)
// re-reads too.
func (f fileFacts) proven(parent *tree.Entry, parentStartNs int64) bool {
	if tree.NodeType(parent.Type) != tree.TypeFile {
		return false
	}
	switch {
	case f.posix != nil && tree.MetaKind(parent.MetaKind) == tree.MetaPOSIX:
		return f.size == parent.Size && posixUnchanged(parent, f.posix, parentStartNs)
	case f.posix == nil && tree.MetaKind(parent.MetaKind) == tree.MetaS3:
		// The etag is the source's own content fingerprint: if it and the
		// size are unchanged, the contents are, no matter what the mtime
		// says. No racy guard applies -- nothing here can race a clock.
		return len(f.etag) > 0 && len(parent.Etag) > 0 &&
			bytes.Equal(parent.Etag, f.etag) && parent.Size == f.size
	default:
		return false
	}
}

// tryReuse carries a file's chunk list over from the parent snapshot
// when the facts prove the contents unchanged. Beyond the metadata
// proof, every chunk the entry names must still resolve to a pack that
// prune has not marked: reuse is only as good as the data behind it.
// The bool reports whether the file was reused; false means re-read it.
func (b *backupRun) tryReuse(ctx context.Context, f fileFacts, parent *tree.Entry) (uint64, []crypto.ID, tree.ContentType, bool) {
	if parent == nil || !f.proven(parent, b.parentStartNs) {
		return 0, nil, 0, false
	}

	// Indirect entries name their chunk list as chunks: those must be
	// present too, and the list reassembled to check the data chunks
	// behind it.
	dataIDs := parent.Chunks
	var listRefs []reusableChunk
	if tree.ContentType(parent.ContentType) == tree.ContentIndirect {
		var refs []reusableChunk
		for _, id := range parent.Chunks {
			loc, ok := b.repo.index.Lookup(id)
			if !ok || b.isMarked(loc.Pack) {
				return 0, nil, 0, false
			}
			refs = append(refs, reusableChunk{id: id, pack: loc.Pack})
		}
		resolved, err := b.repo.NewChunkSource().ChunkList(ctx, parent.Chunks)
		if err != nil {
			b.opts.warn("cannot read previous chunk list: %v; re-reading file", err)
			return 0, nil, 0, false
		}
		dataIDs = resolved
		listRefs = refs
	}

	refs := make([]reusableChunk, 0, len(dataIDs))
	for _, id := range dataIDs {
		loc, ok := b.repo.index.Lookup(id)
		if !ok || b.isMarked(loc.Pack) {
			return 0, nil, 0, false
		}
		refs = append(refs, reusableChunk{id: id, pack: loc.Pack})
	}
	// Referenced packs are recorded only now that every chunk passed: a
	// failed reuse must not leave this backup committed to packs its
	// snapshot does not use.
	b.recordReferenced(refs)
	b.recordReferenced(listRefs)

	// Bytes are a data fact: a reused file's length is still in the
	// snapshot's totals, counted once per hard-link group like any other
	// read (format-v3-draft.md §9.1). The group is the file's own, from
	// the facts the source just reported.
	var key hardLinkKey
	isHardlink := false
	if f.posix != nil && f.posix.NLink > 1 {
		key = hardLinkKey{device: f.posix.Dev, inode: f.posix.Inode}
		isHardlink = true
	}
	b.countBytes(key, isHardlink, parent.Size)
	b.report.ChunksRead += uint64(len(dataIDs))
	return parent.Size, parent.Chunks, tree.ContentType(parent.ContentType), true
}

// recordReferenced notes the chunks this backup deduplicated and the
// packs it read them from, so the commit gate can re-resolve each one.
func (b *backupRun) recordReferenced(refs []reusableChunk) {
	for _, ref := range refs {
		if b.referenced[ref.pack] == nil {
			b.referenced[ref.pack] = make(map[crypto.ID]struct{})
		}
		b.referenced[ref.pack][ref.id] = struct{}{}
	}
}

// isMarked reports whether a pack carries a gc mark from before this
// backup started.
func (b *backupRun) isMarked(pack crypto.ID) bool {
	_, marked := b.marked[pack]
	return marked
}
