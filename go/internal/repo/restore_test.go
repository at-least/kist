package repo

import (
	"context"
	"os"
	"path/filepath"
	"runtime"
	"testing"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

func ptrUint32(v uint32) *uint32 { return &v }

func ptrInt64(v int64) *int64 { return &v }

// A snapshot can carry a symlink and, through a second root whose
// locator passes under it, files that belong under that symlink. That is
// what backing up "/" and "/data2/sub" produces on a machine where
// /data2 is a symlink (the "/" root does not lexically swallow
// "/data2/sub", so both survive path normalisation) -- and it is also
// what a repository holder can write by hand. Restore must not let the
// second root's directory writes pass through the symlink the first
// root planted: a write through it lands outside the directory the user
// named, the same escape the file path's O_EXCL already refuses.
func TestRestoreRefusesToWriteThroughAPlantedSymlink(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("creating symlinks needs privileges on windows")
	}
	ctx := context.Background()
	r, _ := initRepo(t, "restore-symlink-escape")

	base := t.TempDir()
	src := filepath.Join(base, "src")
	victim := filepath.Join(base, "victim")
	if err := os.MkdirAll(filepath.Join(victim, "data"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(src, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(victim, filepath.Join(src, "link")); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "harmless.txt"), []byte("kept"), 0o644); err != nil {
		t.Fatal(err)
	}

	// A real backup for the first root: it records the symlink.
	summary, err := r.Backup(ctx, []string{src}, BackupOptions{SpoolDir: t.TempDir()})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}

	// The second root is crafted, exactly as a repository holder could
	// write it: a tree of one file, stored under the first root's
	// symlink. The path normaliser drops a lexically nested second root,
	// so this shape arrives by the "/" quirk above or by hand -- restore
	// must survive either.
	secret := tree.New([]tree.Entry{{
		Name:        []byte("secret.txt"),
		Type:        uint8(tree.TypeFile),
		MetaKind:    uint8(tree.MetaPOSIX),
		ContentType: uint8(tree.ContentDirect),
		Size:        7,
		Chunks:      []crypto.ID{crypto.ContentID(&r.keys.Hash, []byte("escaped"))},
		Mode:        ptrUint32(0o644),
		UID:         ptrUint32(1000),
		GID:         ptrUint32(1000),
		MTimeNs:     ptrInt64(1_750_000_000_000_000_000),
	}})
	treeID, encoded, err := secret.Encode(&r.keys.Hash)
	if err != nil {
		t.Fatalf("encode crafted tree: %v", err)
	}
	sealed, err := crypto.Seal(&r.keys.Meta, treeID[:], encoded, r.nonceSource)
	if err != nil {
		t.Fatalf("seal crafted tree: %v", err)
	}
	if err := backend.PutBytesIfAbsent(ctx, r.backend, tree.Key(treeID), sealed); err != nil {
		t.Fatalf("store crafted tree: %v", err)
	}

	snap, err := snapshot.Load(ctx, r.backend, r.keys, summary.Handle.Key)
	if err != nil {
		t.Fatal(err)
	}
	snap.TimeNs += 1_000_000_000 // a later key, not the one just committed
	snap.Roots = append(snap.Roots, snapshot.Root{
		Path: []byte(filepath.Join(src, "link", "data")),
		Tree: treeID,
	})
	hostile, err := snap.Save(ctx, r.backend, r.keys, r.nonceSource, 0)
	if err != nil {
		t.Fatalf("save crafted snapshot: %v", err)
	}

	// Everything the victim held leaves the picture before the restore:
	// whatever reappears under it came through the planted symlink.
	if err := os.RemoveAll(victim); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(victim, 0o755); err != nil { // keep the symlink's target a directory, so the escape stays observable
		t.Fatal(err)
	}

	target := filepath.Join(base, "restore")
	if _, err := r.Restore(ctx, hostile.Key, target, RestoreOptions{}); err == nil {
		t.Fatal("restore whose root passes through a snapshot-planted symlink must fail, not claim success")
	}
	if _, err := os.Stat(filepath.Join(victim, "data", "secret.txt")); err == nil {
		t.Fatal("restore wrote through the planted symlink: victim/data/secret.txt exists outside the restore target")
	}
	// The first root itself still restored fine; only the escaping root
	// is refused.
	if _, err := os.Stat(filepath.Join(target, src, "harmless.txt")); err != nil {
		t.Fatalf("the non-escaping root should have restored its files: %v", err)
	}
}
