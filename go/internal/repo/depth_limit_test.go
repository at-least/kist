// A hostile repository can be an arbitrarily deep DIR chain: one tree per
// level, written by hand -- exactly the shape a repository holder can
// produce (the threat model for check and restore is "owns the
// repository"). The tree walks recurse, so a chain past maxTreeDepth must
// be refused cleanly -- reported as a problem, not run until the stack
// dies. The limit is an implementation limit both implementations share
// (docs/format.md §8.4); mirror of the Rust test in
// crates/kist-core/tests/restore_hardening.rs.
package repo

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

func TestOverlyDeepTreeChainIsRefusedCleanly(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "deep-tree-chain")
	base := t.TempDir()

	// Deepest level: a tree holding one empty file; upward, one DIR entry
	// per level. storeAndLink stores a tree and returns its ID.
	var store func(tr *tree.Tree) crypto.ID
	store = func(tr *tree.Tree) crypto.ID {
		t.Helper()
		id, encoded, err := tr.Encode(&r.keys.Hash)
		if err != nil {
			t.Fatalf("encode tree: %v", err)
		}
		sealed, err := crypto.Seal(&r.keys.Meta, id[:], encoded, r.nonceSource)
		if err != nil {
			t.Fatalf("seal tree: %v", err)
		}
		// The two chains share their content-addressed prefix; a tree is
		// immutable, so "already exists" is the store succeeding twice.
		if err := backend.PutBytesIfAbsent(ctx, r.backend, tree.Key(id), sealed); err != nil && !errors.Is(err, backend.ErrExists) {
			t.Fatalf("store tree: %v", err)
		}
		return id
	}
	leaf := store(tree.New([]tree.Entry{{
		Name:        []byte("f"),
		Type:        uint8(tree.TypeFile),
		MetaKind:    uint8(tree.MetaGeneric),
		ContentType: uint8(tree.ContentDirect),
	}}))
	buildChain := func(depth int) crypto.ID {
		t.Helper()
		child := leaf
		for i := 0; i < depth; i++ {
			child = store(tree.New([]tree.Entry{{
				Name:     []byte("d"),
				Type:     uint8(tree.TypeDir),
				MetaKind: uint8(tree.MetaGeneric),
				Subtree:  &child,
			}}))
		}
		return child
	}
	// A real backup gives the synthetic snapshots their seed shape.
	src := filepath.Join(base, "src")
	if err := os.MkdirAll(src, 0o755); err != nil {
		t.Fatal(err)
	}
	summary, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	saveRoot := func(root crypto.ID, path string) string {
		t.Helper()
		snap, err := snapshot.Load(ctx, r.backend, r.keys, summary.Handle.Key)
		if err != nil {
			t.Fatalf("load seed snapshot: %v", err)
		}
		snap.TimeNs += 1_000_000_000 // a later key, not the one just committed
		snap.Roots = []snapshot.Root{{Path: []byte(path), Tree: root}}
		handle, err := snap.Save(ctx, r.backend, r.keys, r.nonceSource, 0)
		if err != nil {
			t.Fatalf("save snapshot: %v", err)
		}
		return handle.Key
	}

	// maxTreeDepth - 1 is the promised depth: check must see nothing wrong.
	okKey := saveRoot(buildChain(maxTreeDepth-1), "/ok")
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
	hostileKey := saveRoot(buildChain(2*maxTreeDepth), "/deep")
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
