package repo

import (
	"bytes"
	"context"
	"path/filepath"
	"slices"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/parity"
	"github.com/at-least/kist/internal/snapshot"
)

// parityRepo backs up sampleFiles with two parity shards per pack.
func parityRepo(t *testing.T, seed string) (*Repository, string, snapshot.Handle) {
	t.Helper()
	r, dir := initRepo(t, seed)
	source := t.TempDir()
	writeTree(t, source, sampleFiles(t))
	_, handle, err := r.Backup(context.Background(), []string{source}, BackupOptions{SpoolDir: t.TempDir(), Parity: 2})
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	return r, dir, handle
}

// flipAt flips one byte at each offset of the object at key.
func flipAt(t *testing.T, r *Repository, key string, offsets ...int) {
	t.Helper()
	data := mustGet(t, r, key)
	for _, o := range offsets {
		if o < 0 {
			o += len(data)
		}
		data[o] ^= 0x5a
	}
	replace(t, r, key, data)
}

func TestBackupWritesParityBesideEachPack(t *testing.T) {
	r, _, _ := parityRepo(t, "parity-write")
	ctx := context.Background()
	packs := countKeys(t, r.Backend(), pack.Prefix)
	if n := countKeys(t, r.Backend(), parity.Prefix); n != packs || n == 0 {
		t.Fatalf("%d parity objects for %d packs", n, packs)
	}
	id := anyPack(t, r)
	raw, err := backend.GetAll(ctx, r.Backend(), parity.Key(id))
	if err != nil {
		t.Fatal(err)
	}
	obj, err := parity.Parse(raw)
	if err != nil {
		t.Fatal(err)
	}
	packBytes := mustGet(t, r, pack.Key(id))
	if obj.M != 2 || obj.PackSize != uint64(len(packBytes)) {
		t.Errorf("parity for %s: m=%d size=%d, pack is %d bytes", id, obj.M, obj.PackSize, len(packBytes))
	}
	report, err := r.Check(ctx, CheckOptions{})
	if err != nil || !report.OK() || report.ParityPacks != packs {
		t.Fatalf("check: %v %v parity=%d", err, report.Problems, report.ParityPacks)
	}
}

func TestCheckRepairsADamagedPackFromParity(t *testing.T) {
	r, _, handle := parityRepo(t, "parity-repair")
	ctx := context.Background()
	id := anyPack(t, r)
	original := mustGet(t, r, pack.Key(id))
	shardLen := (len(original) + parity.DataShards - 1) / parity.DataShards

	// One flip in the body and two in the trailer's shard: two damaged
	// shards, exactly what m=2 can rebuild.
	flipAt(t, r, pack.Key(id), shardLen*3+7, -1, -5)

	damaged := reopen(t, r.Backend().Location(), "parity-repair-2")
	before, err := damaged.Check(ctx, CheckOptions{ReadData: true})
	if err != nil {
		t.Fatal(err)
	}
	if before.OK() {
		t.Fatal("check did not notice the damage")
	}

	after, err := damaged.Check(ctx, CheckOptions{Repair: true})
	if err != nil {
		t.Fatal(err)
	}
	if !after.OK() || len(after.Repaired) != 1 || after.Repaired[0] != id || len(after.Unrepairable) != 0 {
		t.Fatalf("check --repair: problems %v, repaired %v, unrepairable %v", after.Problems, after.Repaired, after.Unrepairable)
	}
	if !bytes.Equal(mustGet(t, damaged, pack.Key(id)), original) {
		t.Fatal("repaired pack is not byte-identical to the original")
	}
	clean, err := damaged.Check(ctx, CheckOptions{ReadData: true})
	if err != nil || !clean.OK() {
		t.Fatalf("check after repair: %v %v", err, clean.Problems)
	}
	target := filepath.Join(t.TempDir(), "out")
	if _, err := damaged.Restore(ctx, handle.Key, target, RestoreOptions{}); err != nil {
		t.Fatalf("restore after repair: %v", err)
	}
}

func TestCheckReportsWhatParityCannotRepair(t *testing.T) {
	ctx := context.Background()

	t.Run("too much damage", func(t *testing.T) {
		r, _, _ := parityRepo(t, "parity-toomuch")
		id := anyPack(t, r)
		original := mustGet(t, r, pack.Key(id))
		shardLen := (len(original) + parity.DataShards - 1) / parity.DataShards
		flipAt(t, r, pack.Key(id), 0, shardLen, 2*shardLen)
		report, err := r.Check(ctx, CheckOptions{Repair: true})
		if err != nil {
			t.Fatal(err)
		}
		if report.OK() || len(report.Unrepairable) != 1 || len(report.Repaired) != 0 {
			t.Fatalf("report: %+v", report)
		}
		if bytes.Equal(mustGet(t, r, pack.Key(id)), original) {
			t.Fatal("the pack was rewritten although it could not be repaired")
		}
	})

	t.Run("no parity", func(t *testing.T) {
		r, _, _ := backedUpRepo(t, "parity-none")
		id := anyPack(t, r)
		flipAt(t, r, pack.Key(id), 10)
		report, err := r.Check(ctx, CheckOptions{Repair: true})
		if err != nil {
			t.Fatal(err)
		}
		if report.OK() || len(report.Unrepairable) != 1 || report.ParityPacks != 0 {
			t.Fatalf("report: %+v", report)
		}
		if !slices.ContainsFunc(report.Problems, func(p string) bool { return bytes.Contains([]byte(p), []byte("no parity")) }) {
			t.Errorf("problems do not say there is no parity: %v", report.Problems)
		}
	})

	t.Run("forged parity", func(t *testing.T) {
		r, _, _ := parityRepo(t, "parity-forged")
		id := anyPack(t, r)
		damagedBytes := append([]byte(nil), mustGet(t, r, pack.Key(id))...)
		damagedBytes[3] ^= 0xff
		// A parity object computed for the damaged bytes, under the
		// real pack's name: every shard hash matches, the name does not.
		forged, err := parity.Encode(crypto.CiphertextID(damagedBytes), damagedBytes, 2)
		if err != nil {
			t.Fatal(err)
		}
		replace(t, r, parity.Key(id), forged)
		replace(t, r, pack.Key(id), damagedBytes)
		report, err := r.Check(ctx, CheckOptions{Repair: true})
		if err != nil {
			t.Fatal(err)
		}
		if report.OK() || len(report.Unrepairable) != 1 {
			t.Fatalf("forged parity: %+v", report)
		}
		if !bytes.Equal(mustGet(t, r, pack.Key(id)), damagedBytes) {
			t.Fatal("the pack was rewritten from forged parity")
		}
	})

	t.Run("damaged parity object", func(t *testing.T) {
		r, _, _ := parityRepo(t, "parity-broken")
		id := anyPack(t, r)
		replace(t, r, parity.Key(id), []byte("not a parity object"))
		flipAt(t, r, pack.Key(id), 10)
		report, err := r.Check(ctx, CheckOptions{Repair: true})
		if err != nil {
			t.Fatal(err)
		}
		if report.OK() || len(report.Unrepairable) != 1 {
			t.Fatalf("damaged parity: %+v", report)
		}
	})
}

func TestPruneRemovesParityWithThePack(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	_, h, err := a.Backup(context.Background(), []string{src}, BackupOptions{SpoolDir: t.TempDir(), Parity: 1})
	if err != nil {
		t.Fatal(err)
	}
	s.sources[h.Key] = src
	if countKeys(t, a.Backend(), parity.Prefix) != 1 {
		t.Fatal("no parity written")
	}
	s.forget(a, h)
	p := s.pruner()
	s.prune(p, shortGrace)
	s.clock.advance(2 * time.Hour)
	s.backup(a, s.source("two", 10<<10))
	report := s.prune(p, shortGrace)
	if len(report.Deleted) != 1 {
		t.Fatalf("sweep: %+v", report)
	}
	if n := countKeys(t, p.Backend(), parity.Prefix); n != 0 {
		t.Errorf("%d parity objects left after the pack was deleted, want 0", n)
	}

	// An orphaned parity object -- its pack gone by other means -- is
	// removed by the next run.
	if err := backend.PutBytesIfAbsent(context.Background(), p.Backend(), parity.Key(crypto.ID{9}), []byte("orphan")); err != nil {
		t.Fatal(err)
	}
	s.prune(p, shortGrace)
	if n := countKeys(t, p.Backend(), parity.Prefix); n != 0 {
		t.Errorf("orphaned parity not removed: %d left", n)
	}
	s.healthy()
}
