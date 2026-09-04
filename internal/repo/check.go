package repo

import (
	"context"
	"errors"
	"fmt"
	"sort"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

// CheckOptions configure an integrity check.
type CheckOptions struct {
	// ReadData fetches and verifies every byte of every pack rather than
	// only their trailers.
	//
	// The two levels catch different damage, and neither subsumes the
	// other. Structural checking catches a missing pack, a truncated one,
	// a tree that refers to a chunk nobody has -- everything that is
	// visible from the shape of the repository. Only ReadData catches a
	// bit flipped inside chunk data, because nothing else ever decrypts
	// it. It costs a full read of the repository, which is why it is not
	// the default.
	ReadData bool

	// Progressf receives one line per phase, for a human watching.
	Progressf func(format string, args ...any)
}

func (o CheckOptions) progress(format string, args ...any) {
	if o.Progressf != nil {
		o.Progressf(format, args...)
	}
}

// CheckReport is what a check found.
type CheckReport struct {
	Snapshots int
	Trees     int
	Chunks    int
	Packs     int

	// Problems are the findings, in the order they were discovered. A
	// check with any problem is a failure, whatever else it managed to
	// verify.
	Problems []string
}

// OK reports whether the repository is intact.
func (r CheckReport) OK() bool { return len(r.Problems) == 0 }

// Check verifies a repository.
func (r *Repository) Check(ctx context.Context, opts CheckOptions) (CheckReport, error) {
	var report CheckReport
	problem := func(format string, args ...any) {
		report.Problems = append(report.Problems, fmt.Sprintf(format, args...))
	}

	// 1. Every pack that exists must have a readable, consistent trailer.
	//
	// The trailers are also what the index is rebuilt from, so they are
	// read once and used twice. A pack whose trailer will not parse is
	// recorded and skipped: a check that stops at the first damaged
	// object cannot tell you how much of a repository survived, which is
	// the question you are asking when you run it.
	opts.progress("checking pack trailers")
	stored := make(map[crypto.ID]struct{})
	rebuilt := index.New()

	err := r.backend.List(ctx, pack.Prefix, func(fi backend.FileInfo) error {
		id, err := crypto.ParseID(fi.Key[len(pack.Prefix):])
		if err != nil {
			problem("%s is not named like a pack: %v", fi.Key, err)
			return nil
		}
		stored[id] = struct{}{}
		report.Packs++

		entries, err := pack.ReadTrailer(ctx, r.backend, r.keys, id)
		if err != nil {
			problem("pack %s: %v", id, err)
			return nil
		}
		rebuilt.AddPack(id, entries)
		return nil
	})
	if err != nil {
		return report, fmt.Errorf("check: %w", err)
	}
	report.Chunks = rebuilt.Len()

	// 2. The cached index must not refer to packs that are not there. A
	// disagreement is a finding, not a failure -- the index is a cache,
	// and rebuild-index is the answer -- but it must be reported.
	for _, id := range r.index.Packs() {
		if _, ok := stored[id]; !ok {
			problem("the index refers to pack %s, which is not stored", id)
		}
	}

	// 3. Every snapshot must resolve, all the way down to chunks that
	// something actually holds.
	opts.progress("walking snapshots")
	handles, err := snapshot.List(ctx, r.backend, "")
	if err != nil {
		return report, fmt.Errorf("check: %w", err)
	}

	seenTrees := make(map[crypto.ID]struct{})
	usedPacks := make(map[crypto.ID]struct{})
	for _, handle := range handles {
		report.Snapshots++

		snap, err := snapshot.Load(ctx, r.backend, r.keys, handle.Key)
		if err != nil {
			problem("snapshot %s: %v", handle.Key, err)
			continue
		}
		r.walkTree(ctx, snap.Root, handle.Key, rebuilt, seenTrees, usedPacks, problem)
	}
	report.Trees = len(seenTrees)

	// 4. Optionally, read everything. This is the only step that can see
	// a flipped bit inside a chunk.
	if opts.ReadData {
		opts.progress("reading and verifying %d packs", len(stored))
		ids := make([]crypto.ID, 0, len(stored))
		for id := range stored {
			ids = append(ids, id)
		}
		sort.Slice(ids, func(i, j int) bool { return ids[i].String() < ids[j].String() })

		for _, id := range ids {
			reader, err := pack.OpenReader(ctx, r.backend, r.keys, id)
			if err != nil {
				problem("pack %s: %v", id, err)
				continue
			}
			if err := reader.VerifyAll(ctx); err != nil {
				problem("pack %s: %v", id, err)
			}
		}
	}

	return report, nil
}

// walkTree descends one snapshot, recording what it reaches.
func (r *Repository) walkTree(
	ctx context.Context,
	id crypto.ID,
	origin string,
	ix *index.Index,
	seenTrees, usedPacks map[crypto.ID]struct{},
	problem func(string, ...any),
) {
	if _, done := seenTrees[id]; done {
		// Shared subtrees are the point of content addressing; walking
		// one twice would turn a check into an exponential walk.
		return
	}
	seenTrees[id] = struct{}{}

	t, err := tree.Load(ctx, r.backend, r.keys, id)
	if err != nil {
		problem("%s: tree %s: %v", origin, id, err)
		return
	}

	for _, entry := range t.Entries {
		switch entry.Type {
		case tree.TypeDir:
			r.walkTree(ctx, entry.Subtree, origin, ix, seenTrees, usedPacks, problem)
		case tree.TypeFile:
			for _, chunkID := range entry.Chunks {
				loc, ok := ix.Lookup(chunkID)
				if !ok {
					problem("%s: tree %s: %q refers to chunk %s, which no pack holds", origin, id, entry.Name, chunkID)
					continue
				}
				usedPacks[loc.Pack] = struct{}{}
			}
		case tree.TypeSymlink:
			// A symlink has no content to check beyond its target, which
			// the tree already validated.
		default:
			problem("%s: tree %s: %q has unknown type %d", origin, id, entry.Name, entry.Type)
		}
	}
}

// ErrCheckFailed is what a caller gets when a check found problems.
var ErrCheckFailed = errors.New("repository check found problems")
