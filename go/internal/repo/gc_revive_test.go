package repo

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/tree"
)

// A tree rewritten in the SAME second as its mark must count as newer --
// the safe side, exactly what the pack path in this file already does and
// what this function's own doc promises. A strict-After check deletes a
// same-second heal that the Rust peer (prune.rs: >=) keeps.
func TestTreeRewrittenInTheMarkSecondRevives(t *testing.T) {
	ctx := context.Background()
	r, repoDir := initRepo(t, "same-second-revive")
	base := t.TempDir()
	src := filepath.Join(base, "src")
	if err := os.MkdirAll(src, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "a.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatalf("backup: %v", err)
	}

	// Any stored tree object (not a replica).
	var treeID crypto.ID
	found := false
	err := r.backend.List(ctx, tree.Prefix, func(fi backend.FileInfo) error {
		if strings.HasSuffix(fi.Key, ".r1") || found {
			return nil
		}
		id, err := crypto.ParseID(strings.TrimPrefix(fi.Key, tree.Prefix))
		if err != nil {
			return nil
		}
		treeID, found = id, true
		return nil
	})
	if err != nil {
		t.Fatalf("list trees: %v", err)
	}
	if !found {
		t.Fatal("backup must have written at least one tree")
	}

	// Pin the tree's mtime to a whole second and drop the touch signal,
	// so the mtime comparison is the only thing under test.
	T := time.Now().Truncate(time.Second)
	treePath := filepath.Join(repoDir, filepath.FromSlash(tree.Key(treeID)))
	if err := os.Chtimes(treePath, T, T); err != nil {
		t.Fatalf("chtimes tree: %v", err)
	}
	// A NEW tree has no touch signal (only reused trees get touched), so
	// the mtime comparison is the only thing under test; remove a touch
	// if one exists anyway.
	touchPath := filepath.Join(repoDir, filepath.FromSlash(tree.TouchKey(treeID)))
	_ = os.Remove(touchPath)

	revived, err := r.treeRevivedAfterMark(ctx, treeID, T)
	if err != nil {
		t.Fatalf("treeRevivedAfterMark: %v", err)
	}
	if !revived {
		t.Fatal("a tree rewritten in the mark's own second must revive: same-second counts as newer (the safe side), same rule as packs and the Rust peer")
	}
}

// Parity shards feed the RS matrix: the CLI bounds 0..=8 (the Rust CLI
// does the same), but the repo layer must not trust its caller -- a
// negative or oversized parity must fail the backup loudly, not warn
// parity objects away after the fact.
func TestBackupRejectsParityOutsideZeroToEight(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "parity-bounds")
	base := t.TempDir()
	src := filepath.Join(base, "src")
	if err := os.MkdirAll(src, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "a.txt"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	for _, parity := range []int{-1, 9, 200} {
		opts := BackupOptions{SpoolDir: t.TempDir(), Parity: parity}
		if _, err := r.Backup(ctx, []string{src}, opts); err == nil {
			t.Fatalf("parity %d must be rejected by the repo layer", parity)
		} else if !strings.Contains(err.Error(), "parity") {
			t.Fatalf("parity %d: error must name the knob: %v", parity, err)
		}
	}
	if _, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir(), Parity: 8}); err != nil {
		t.Fatalf("parity 8 is inside the range: %v", err)
	}
}
