package repo

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strings"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/index"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/parity"
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

	// Repair rewrites a damaged pack from its parity object, when it
	// has one and the damage is within what the parity can rebuild.
	// The rewrite happens only after the reconstruction hashes to the
	// pack's own name, and is verified again afterwards. Implies
	// ReadData: damage inside chunks is only visible by reading them.
	Repair bool

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

	// ParityPacks is how many packs have a parity object. Absence is
	// not a problem: parity is a per-client choice.
	ParityPacks int

	// Repaired lists packs rewritten from parity; Unrepairable lists
	// damaged packs that could not be, and why is in Problems.
	Repaired     []crypto.ID
	Unrepairable []crypto.ID

	// Problems are the findings, in the order they were discovered. A
	// check with any problem is a failure, whatever else it managed to
	// verify.
	Problems []string

	// Warnings are findings that do not make the repository unhealthy: a
	// missing replica when replicas are configured, for instance.
	Warnings []string
}

// OK reports whether the repository is intact.
func (r CheckReport) OK() bool { return len(r.Problems) == 0 }

// Check verifies a repository.
func (r *Repository) Check(ctx context.Context, opts CheckOptions) (CheckReport, error) {
	var (
		report CheckReport
		err    error
	)
	if opts.Repair {
		opts.ReadData = true
	}
	problem := func(format string, args ...any) {
		report.Problems = append(report.Problems, fmt.Sprintf(format, args...))
	}

	// 0. Which packs have parity. Read once; consulted whenever a pack
	// turns out to be damaged.
	withParity := make(map[crypto.ID]struct{})
	err = r.backend.List(ctx, parity.Prefix, func(fi backend.FileInfo) error {
		if id, err := crypto.ParseID(fi.Key[len(parity.Prefix):]); err == nil {
			withParity[id] = struct{}{}
		}
		return nil
	})
	if err != nil {
		return report, fmt.Errorf("check: %w", err)
	}
	// repair tries to rewrite one damaged pack. It returns whether the
	// caller should try the pack again.
	repair := func(id crypto.ID, cause error) bool {
		if !opts.Repair {
			return false
		}
		if _, ok := withParity[id]; !ok {
			problem("pack %s: %v; no parity to repair it from", id, cause)
			report.Unrepairable = append(report.Unrepairable, id)
			return false
		}
		if err := r.repairPack(ctx, id); err != nil {
			problem("pack %s: %v; repair failed: %v", id, cause, err)
			report.Unrepairable = append(report.Unrepairable, id)
			return false
		}
		opts.progress("repaired pack %s from parity", id)
		report.Repaired = append(report.Repaired, id)
		return true
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

	err = r.backend.List(ctx, pack.Prefix, func(fi backend.FileInfo) error {
		id, err := crypto.ParseID(fi.Key[len(pack.Prefix):])
		if err != nil {
			problem("%s is not named like a pack: %v", fi.Key, err)
			return nil
		}
		stored[id] = struct{}{}
		report.Packs++
		if _, ok := withParity[id]; ok {
			report.ParityPacks++
		}

		entries, err := pack.ReadTrailer(ctx, r.backend, r.keys, id)
		if err != nil && repair(id, err) {
			entries, err = pack.ReadTrailer(ctx, r.backend, r.keys, id)
		}
		if err != nil {
			if !opts.Repair {
				problem("pack %s: %v", id, err)
			}
			return nil
		}
		rebuilt.AddPack(id, entries)
		return nil
	})
	if err != nil {
		return report, fmt.Errorf("check: %w", err)
	}
	report.Chunks = rebuilt.Len()

	// 2. Every stored index blob must be readable, and the cached index
	// must not refer to packs that are not there.
	//
	// Neither is fatal -- the index is a cache and rebuild-index is the
	// answer -- but both must be reported. The blobs are re-read here
	// rather than trusting what open() managed to load, so that a check
	// says what is in the repository and not what this process
	// remembers.
	opts.progress("checking index blobs")
	_, skipped, err := index.LoadAll(ctx, r.backend, r.keys)
	if err != nil {
		return report, fmt.Errorf("check: %w", err)
	}
	for _, s := range skipped {
		problem("%v; run rebuild-index to repair it", s)
	}

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
	checkChunks := r.NewChunkSource()
	for _, handle := range handles {
		report.Snapshots++

		snap, err := snapshot.Load(ctx, r.backend, r.keys, handle.Key)
		if err != nil {
			problem("snapshot %s: %v", handle.Key, err)
			continue
		}
		for _, root := range snap.Roots {
			r.walkTree(ctx, root.Tree, 1, handle.Key, rebuilt, seenTrees, usedPacks, checkChunks, problem)
		}
	}
	report.Trees = len(seenTrees)

	// 3b. Replica health (docs/format.md §13.5). An orphan replica --
	// ".r1" present, primary tree gone -- is the disaster signal the
	// replica exists to catch, and prune deliberately never cleans one
	// up: it is reported here as a problem. When the repository keeps
	// replicas, a live tree without its replica is only a warning: the
	// data is safe, the redundancy is repairable by re-running a backup.
	err = r.backend.List(ctx, tree.Prefix, func(fi backend.FileInfo) error {
		name, ok := strings.CutSuffix(fi.Key, tree.ReplicaSuffix)
		if !ok {
			return nil
		}
		id, err := crypto.ParseID(strings.TrimPrefix(name, tree.Prefix))
		if err != nil {
			return nil //nolint:nilerr // not a replica key; not a replica problem
		}
		if _, err := r.backend.Stat(ctx, tree.Key(id)); err != nil {
			problem("%s: replica exists but its primary tree is missing (possible data-loss event; data can be recovered from the replica)", fi.Key)
		}
		return nil
	})
	if err != nil {
		return report, fmt.Errorf("check: %w", err)
	}
	if r.config.Replicas > 0 {
		for _, id := range sortedTrees(seenTrees) {
			if _, err := r.backend.Stat(ctx, tree.ReplicaKey(id)); err != nil {
				report.Warnings = append(report.Warnings, fmt.Sprintf("%s: tree replica is missing (repairable)", tree.ReplicaKey(id)))
			}
		}
	}

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
			if slices.Contains(report.Repaired, id) || slices.Contains(report.Unrepairable, id) {
				continue // already dealt with when its trailer failed
			}
			err := verifyPack(ctx, r, id)
			if err != nil && repair(id, err) {
				err = verifyPack(ctx, r, id)
			}
			if err != nil && (!opts.Repair || !slices.Contains(report.Unrepairable, id)) {
				problem("pack %s: %v", id, err)
			}
		}
	}

	return report, nil
}

func verifyPack(ctx context.Context, r *Repository, id crypto.ID) error {
	reader, err := pack.OpenReader(ctx, r.backend, r.keys, id, uint64(r.config.Chunker.MaxSize))
	if err != nil {
		return err
	}
	return reader.VerifyAll(ctx)
}

// repairPack rewrites one pack from its parity object. The rewrite is
// the one place kist replaces an object it did not just create, and it
// happens only for bytes proven, by hashing to the pack's name, to be
// the bytes that were there before the damage.
func (r *Repository) repairPack(ctx context.Context, id crypto.ID) error {
	raw, err := backend.GetAll(ctx, r.backend, parity.Key(id))
	if err != nil {
		return fmt.Errorf("read parity: %w", err)
	}
	obj, err := parity.Parse(raw)
	if err != nil {
		return err
	}
	damaged, err := backend.GetAll(ctx, r.backend, pack.Key(id))
	if err != nil && !errors.Is(err, backend.ErrNotFound) {
		return fmt.Errorf("read pack: %w", err)
	}
	fixed, err := obj.Repair(id, damaged)
	if err != nil {
		return err
	}
	if err := r.backend.Put(ctx, pack.Key(id), bytes.NewReader(fixed), int64(len(fixed))); err != nil {
		return fmt.Errorf("write repaired pack: %w", err)
	}
	if err := verifyPack(ctx, r, id); err != nil {
		return fmt.Errorf("repaired pack does not verify: %w", err)
	}
	return nil
}

// maxTreeDepth caps DIR nesting for the recursive tree walks (check,
// prune, restore). It is an implementation limit, not a format rule: honest
// trees are bounded by source path lengths (far shallower), and a chain
// beyond the limit can only come from a corrupt or hostile repository --
// refuse it cleanly instead of exhausting the stack. Must match the Rust
// implementation's kist_core::MAX_TREE_DEPTH; see docs/format.md §8.4.
const maxTreeDepth = 256

// walkTree descends one snapshot, recording what it reaches. depth is the
// DIR nesting level (roots = 1); prev segments do not count toward it --
// they split one directory, and an honest large directory can chain long.
func (r *Repository) walkTree(
	ctx context.Context,
	id crypto.ID,
	depth int,
	origin string,
	ix *index.Index,
	seenTrees, usedPacks map[crypto.ID]struct{},
	chunks *ChunkSource,
	problem func(string, ...any),
) {
	if _, done := seenTrees[id]; done {
		// Shared subtrees are the point of content addressing; walking
		// one twice would turn a check into an exponential walk.
		return
	}
	seenTrees[id] = struct{}{}

	t, err := r.readTree(ctx, id)
	if err != nil {
		problem("%s: tree %s: %v", origin, id, err)
		return
	}

	if t.Prev != nil {
		r.walkTree(ctx, *t.Prev, depth, origin, ix, seenTrees, usedPacks, chunks, problem)
	}

	for _, entry := range t.Entries {
		switch tree.NodeType(entry.Type) {
		case tree.TypeDir:
			if entry.Subtree == nil {
				problem("%s: tree %s: %q has no subtree", origin, id, entry.Name)
				continue
			}
			if depth >= maxTreeDepth {
				problem("%s: tree %s: nesting deeper than %d levels (corrupt or hostile repository)", origin, id, maxTreeDepth)
				continue
			}
			r.walkTree(ctx, *entry.Subtree, depth+1, origin, ix, seenTrees, usedPacks, chunks, problem)
		case tree.TypeFile:
			// An indirect entry's Chunks name the encoded ChunkList; the
			// data chunks it resolves to are what keeps packs live, so
			// both sets must be walked (docs/format.md §13.1).
			// The list chunks themselves are referenced too (their pack
			// holds the encoded ChunkList the snapshot needs).
			for _, chunkID := range entry.Chunks {
				if loc, ok := ix.Lookup(chunkID); ok {
					usedPacks[loc.Pack] = struct{}{}
				}
			}
			chunkIDs := entry.Chunks
			if tree.ContentType(entry.ContentType) == tree.ContentIndirect {
				list, err := chunks.ChunkList(ctx, entry.Chunks)
				if err != nil {
					problem("%s: tree %s: %q: chunk list: %v", origin, id, entry.Name, err)
					continue
				}
				chunkIDs = list
			}
			for _, chunkID := range chunkIDs {
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

// sortedTrees returns a tree set in a stable order.
func sortedTrees(trees map[crypto.ID]struct{}) []crypto.ID {
	ids := make([]crypto.ID, 0, len(trees))
	for id := range trees {
		ids = append(ids, id)
	}
	slices.SortFunc(ids, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
	return ids
}
