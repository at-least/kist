// A hostile repository can be an arbitrarily deep DIR chain: one tree per
// level, written by hand -- exactly the shape a repository holder can
// produce (the threat model for check and restore is "owns the
// repository"). The DIR walk recurses, so a chain past maxTreeDepth must
// be refused cleanly -- reported as a problem, not run until the stack
// dies. (Prev segments are a different axis: walked in a loop, see
// TestPrevChainIsWalkedIteratively.) The limit is an implementation limit
// both implementations share
// (docs/format.md §8.4); mirror of the Rust test in
// crates/kist-core/tests/restore_hardening.rs.
package repo

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime/debug"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

// storeTree seals and stores a hand-written tree, returning its ID.
func storeTree(t *testing.T, ctx context.Context, r *Repository, tr *tree.Tree) crypto.ID {
	t.Helper()
	id, encoded, err := tr.Encode(&r.keys.Hash)
	if err != nil {
		t.Fatalf("encode tree: %v", err)
	}
	sealed, err := crypto.Seal(&r.keys.Meta, id[:], encoded, r.nonceSource)
	if err != nil {
		t.Fatalf("seal tree: %v", err)
	}
	// Hand-written chains share content-addressed prefixes; a tree is
	// immutable, so "already exists" is the store succeeding twice.
	if err := backend.PutBytesIfAbsent(ctx, r.backend, tree.Key(id), sealed); err != nil && !errors.Is(err, backend.ErrExists) {
		t.Fatalf("store tree: %v", err)
	}
	return id
}

// saveRootSnapshot saves a synthetic snapshot: the seed's shape, one
// root replaced, timestamp moved so the key is a later one.
func saveRootSnapshot(t *testing.T, ctx context.Context, r *Repository, seedKey string, root crypto.ID, path string) string {
	t.Helper()
	snap, err := snapshot.Load(ctx, r.backend, r.keys, seedKey)
	if err != nil {
		t.Fatalf("load seed snapshot: %v", err)
	}
	snap.TimeNs += 1_000_000_000
	snap.Roots = []snapshot.Root{{Path: []byte(path), Tree: root}}
	handle, err := snap.Save(ctx, r.backend, r.keys, r.nonceSource, 0)
	if err != nil {
		t.Fatalf("save snapshot: %v", err)
	}
	return handle.Key
}

// seedBackup commits one real backup so synthetic snapshots have the
// object shape (config, key slot, packs) an honest repo has.
func seedBackup(t *testing.T, ctx context.Context, r *Repository, base string) string {
	t.Helper()
	src := filepath.Join(base, "src")
	if err := os.MkdirAll(src, 0o755); err != nil {
		t.Fatal(err)
	}
	summary, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	return summary.Handle.Key
}

func TestOverlyDeepTreeChainIsRefusedCleanly(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "deep-tree-chain")
	base := t.TempDir()

	leaf := storeTree(t, ctx, r, tree.New([]tree.Entry{{
		Name:        []byte("f"),
		Type:        uint8(tree.TypeFile),
		MetaKind:    uint8(tree.MetaGeneric),
		ContentType: uint8(tree.ContentDirect),
	}}))
	buildChain := func(depth int) crypto.ID {
		t.Helper()
		child := leaf
		for i := 0; i < depth; i++ {
			child = storeTree(t, ctx, r, tree.New([]tree.Entry{{
				Name:     []byte("d"),
				Type:     uint8(tree.TypeDir),
				MetaKind: uint8(tree.MetaGeneric),
				Subtree:  &child,
			}}))
		}
		return child
	}
	seed := seedBackup(t, ctx, r, base)
	okKey := saveRootSnapshot(t, ctx, r, seed, buildChain(maxTreeDepth-1), "/ok")
	if _, err := r.Check(ctx, CheckOptions{}); err != nil {
		t.Fatalf("one level below the limit is honest depth; check must pass: %v", err)
	}
	if _, err := r.Restore(ctx, okKey, filepath.Join(base, "ok"), RestoreOptions{}); err != nil {
		t.Fatalf("restore at maxTreeDepth-1 must succeed: %v", err)
	}

	// Past the limit: check reports the depth (prune's safety gate), and
	// restore refuses instead of exhausting the stack. The chain shares its
	// content-addressed bottom with the maxTreeDepth-1 chain above, and a
	// visited tree is not re-walked at a deeper level -- so the hostile
	// chain needs more than maxTreeDepth levels ABOVE that shared prefix
	// for the walk to still be descending unique trees when the cap fires.
	hostileKey := saveRootSnapshot(t, ctx, r, seed, buildChain(2*maxTreeDepth), "/deep")
	report, err := r.Check(ctx, CheckOptions{})
	if err != nil {
		t.Fatalf("check itself must not fail: %v", err)
	}
	joined := strings.Join(report.Problems, "; ")
	if !strings.Contains(joined, "nesting deeper") {
		t.Fatalf("check must report the over-deep chain as a problem: %+v", report)
	}
	_, restoreErr := r.Restore(ctx, hostileKey, filepath.Join(base, "restore"), RestoreOptions{})
	if restoreErr == nil {
		t.Fatal("restore must refuse the over-deep chain")
	}
	if !strings.Contains(restoreErr.Error(), "nesting deeper") {
		t.Fatalf("restore error should name the depth limit: %v", restoreErr)
	}
	if !errors.Is(restoreErr, tree.ErrCorrupt) {
		t.Fatalf("restore error should be corruption, not a crash: %v", restoreErr)
	}
}

// An honest huge directory chains long prev segments (MaxNodesPerTree
// splits it), and a hostile repository can chain them at will: walkTree
// must follow the chain in a LOOP. The recursive version burns a stack
// frame per segment and dies with an uncatchable fatal (goroutine stack
// exceeds the limit), taking check or prune down with it. The goroutine
// stack cap is lowered for this test so a chain a fraction of this
// length would kill the recursive walk; the loop walks them all and
// reports no problems. The Rust peer walks prev in a loop for the same
// reason (crates/kist-core/src/reach.rs; docs/format.md §8.4).
func TestPrevChainIsWalkedIteratively(t *testing.T) {
	old := debug.SetMaxStack(2 << 20)
	defer debug.SetMaxStack(old)

	ctx := context.Background()
	r, _ := initRepo(t, "long-prev-chain")
	base := t.TempDir()

	const segments = 15_000
	// One file per segment; distinct names keep every segment a distinct
	// tree so the seen-guard cannot shortcut the chain.
	child := storeTree(t, ctx, r, tree.New([]tree.Entry{{
		Name:        []byte("f0"),
		Type:        uint8(tree.TypeFile),
		MetaKind:    uint8(tree.MetaGeneric),
		ContentType: uint8(tree.ContentDirect),
	}}))
	for i := 1; i < segments; i++ {
		tr := tree.New([]tree.Entry{{
			Name:        []byte(fmt.Sprintf("f%d", i)),
			Type:        uint8(tree.TypeFile),
			MetaKind:    uint8(tree.MetaGeneric),
			ContentType: uint8(tree.ContentDirect),
		}})
		tr.Prev = &child
		child = storeTree(t, ctx, r, tr)
	}

	seed := seedBackup(t, ctx, r, base)
	key := saveRootSnapshot(t, ctx, r, seed, child, "/chain")

	report, err := r.Check(ctx, CheckOptions{})
	if err != nil {
		t.Fatalf("check must survive a %d-segment prev chain: %v", segments, err)
	}
	if len(report.Problems) != 0 {
		t.Fatalf("a long prev chain is honest large-directory shape; check must walk it all: %+v", report.Problems)
	}
	// Restore reads the chain through LoadTreeChain, which is already
	// iterative; asserting it here pins the whole chain as the honest
	// shape every walker must handle.
	out := filepath.Join(base, "out")
	if _, err := r.Restore(ctx, key, out, RestoreOptions{}); err != nil {
		t.Fatalf("restore of a %d-segment chain must succeed: %v", segments, err)
	}
	if _, err := os.Stat(filepath.Join(out, "chain", "f0")); err != nil {
		t.Fatalf("the oldest segment's entry must be restored: %v", err)
	}
}

// The write side uses the same depth ruler as the read side (§8.4): a
// legal-but-too-deep source directory must be skipped and counted, not
// written into a tree that restore refuses -- "backed up but not
// restorable" is damage the tool would have manufactured itself. The
// Rust peer pins the same behavior in
// crates/kist-core/tests/backup_restore.rs.
func TestBackupRefusesDepthBeyondTheReadSideLimit(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "deep-source")
	base := t.TempDir()

	// 260 nested directories: legal on ext4 (PATH_MAX fits ~2000 levels
	// of short names), past the readers' 256.
	src := filepath.Join(base, "src")
	deep := src
	for i := 0; i < 260; i++ {
		deep = filepath.Join(deep, fmt.Sprintf("d%d", i))
	}
	if err := os.MkdirAll(deep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(deep, "leaf.txt"), []byte("deep"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "top.txt"), []byte("top"), 0o644); err != nil {
		t.Fatal(err)
	}

	summary, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if summary.Report.Errors < 1 {
		t.Fatalf("a directory deeper than maxTreeDepth must be skipped and counted: %+v", summary.Report)
	}

	// The written depth stays within the read side's limit: restore
	// succeeds, and the restored chain stops at level maxTreeDepth.
	out := filepath.Join(base, "out")
	if _, err := r.Restore(ctx, summary.Handle.Key, out, RestoreOptions{}); err != nil {
		t.Fatalf("restore must succeed when the write side honored the cap: %v", err)
	}
	// Restore rebuilds the source's full absolute path under the target.
	restored := filepath.Join(out, strings.TrimPrefix(src, "/"))
	if _, err := os.Stat(filepath.Join(restored, "top.txt")); err != nil {
		t.Fatalf("shallow content must restore: %v", err)
	}
	limitOK := restored
	for i := 0; i < 255; i++ { // d0..d254 = levels 2..256
		limitOK = filepath.Join(limitOK, fmt.Sprintf("d%d", i))
	}
	if fi, err := os.Stat(limitOK); err != nil || !fi.IsDir() {
		t.Fatalf("depth within the limit must restore: %v (%s)", err, limitOK)
	}
	if _, err := os.Stat(filepath.Join(limitOK, "d255")); !os.IsNotExist(err) {
		t.Fatal("level 257 and beyond must be skipped, not written as a tree the read side refuses")
	}
}
