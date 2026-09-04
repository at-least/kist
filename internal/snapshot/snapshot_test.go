package snapshot

import (
	"context"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

var update = flag.Bool("update", false, "rewrite testdata golden files")

var (
	goldenMaster = crypto.Key{
		0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
		0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
		0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
		0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
	}
	goldenRepoID = crypto.RepoID{
		0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
		0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
	}
	goldenTime = time.Date(2026, 1, 2, 3, 4, 5, 123456789, time.UTC)
)

func testKeys(t *testing.T) *crypto.Keys {
	t.Helper()

	keys, err := crypto.DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive keys: %v", err)
	}
	return keys
}

func testBackend(t *testing.T) backend.Backend {
	t.Helper()

	b, err := backend.CreateLocal(filepath.Join(t.TempDir(), "repo"))
	if err != nil {
		t.Fatalf("create backend: %v", err)
	}
	t.Cleanup(func() {
		if err := b.Close(); err != nil {
			t.Errorf("close backend: %v", err)
		}
	})
	return b
}

func rootID(b byte) crypto.ID {
	var id crypto.ID
	id[0] = b
	return id
}

func sample(clientID string, at time.Time) *Snapshot {
	return &Snapshot{
		Version:  Version,
		Root:     rootID(0x42),
		TimeNs:   at.UnixNano(),
		Host:     "workstation",
		Paths:    []string{"/home/newlix", "/etc"},
		ClientID: clientID,
		Stats:    Stats{Files: 12, Dirs: 3, Bytes: 4096, ChunksNew: 5, PacksAdded: 1},
	}
}

func TestSaveLoadRoundTrip(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	original := sample("client01", goldenTime)
	handle, err := original.Save(ctx, b, keys, crypto.DeterministicReader("snap"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	if want := "snapshots/client01/20260102t030405.123456789z"; handle.Key != want {
		t.Errorf("key = %q, want %q", handle.Key, want)
	}

	loaded, err := Load(ctx, b, keys, handle.Key)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	if loaded.Root != original.Root || loaded.Host != original.Host ||
		loaded.ClientID != original.ClientID || loaded.TimeNs != original.TimeNs ||
		strings.Join(loaded.Paths, ",") != strings.Join(original.Paths, ",") ||
		loaded.Stats != original.Stats {
		t.Errorf("loaded = %+v, want %+v", loaded, original)
	}
}

// The key format must sort chronologically and must be legal on Windows,
// which rules out the colons of RFC 3339.
func TestKeysSortChronologicallyAndAvoidColons(t *testing.T) {
	times := []time.Time{
		time.Date(2025, 12, 31, 23, 59, 59, 999999999, time.UTC),
		time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC),
		time.Date(2026, 1, 1, 0, 0, 0, 1, time.UTC),
		time.Date(2026, 10, 1, 0, 0, 0, 0, time.UTC),
	}

	var previous string
	for _, at := range times {
		key := Key("c", at)
		if strings.Contains(key, ":") {
			t.Errorf("key %q contains a colon, which is illegal in a Windows filename", key)
		}
		if err := backend.ValidateKey(key); err != nil {
			t.Errorf("key %q is not a valid repository key: %v", key, err)
		}
		if previous != "" && key <= previous {
			t.Errorf("key %q does not sort after %q", key, previous)
		}
		previous = key
	}
}

func TestParseKeyRoundTrip(t *testing.T) {
	handle, err := ParseKey(Key("abc123", goldenTime))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if handle.ClientID != "abc123" {
		t.Errorf("client = %q, want abc123", handle.ClientID)
	}
	if !handle.Time.Equal(goldenTime) {
		t.Errorf("time = %s, want %s", handle.Time, goldenTime)
	}
}

func TestParseKeyRejectsMalformed(t *testing.T) {
	for name, key := range map[string]string{
		"wrong prefix":  "packs/abc",
		"no timestamp":  "snapshots/client01",
		"empty client":  "snapshots//20260102t030405.123456789z",
		"bad timestamp": "snapshots/client01/yesterday",
		"rfc3339":       "snapshots/client01/2026-01-02T03:04:05Z",
	} {
		t.Run(name, func(t *testing.T) {
			if _, err := ParseKey(key); !errors.Is(err, ErrCorrupt) {
				t.Fatalf("parse %q: err = %v, want ErrCorrupt", key, err)
			}
		})
	}
}

// Two clients that pick the same nanosecond are two backups, not one.
func TestSaveNeverOverwrites(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	first, err := sample("client01", goldenTime).Save(ctx, b, keys, crypto.DeterministicReader("a"))
	if err != nil {
		t.Fatalf("first save: %v", err)
	}

	other := sample("client01", goldenTime)
	other.Root = rootID(0x99)
	second, err := other.Save(ctx, b, keys, crypto.DeterministicReader("b"))
	if err != nil {
		t.Fatalf("second save: %v", err)
	}

	if first.Key == second.Key {
		t.Fatalf("both snapshots landed on %s", first.Key)
	}
	if !second.Time.After(first.Time) {
		t.Errorf("second snapshot at %s, want after %s", second.Time, first.Time)
	}

	original, err := Load(ctx, b, keys, first.Key)
	if err != nil {
		t.Fatalf("load first: %v", err)
	}
	if original.Root != rootID(0x42) {
		t.Error("the first snapshot was overwritten")
	}
}

// The AAD is the full key, so a snapshot moved into another client's
// namespace stops opening.
func TestSnapshotIsBoundToItsKey(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	handle, err := sample("client01", goldenTime).Save(ctx, b, keys, crypto.DeterministicReader("bind"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	sealed, err := backend.GetAll(ctx, b, handle.Key)
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	moved := Key("client02", goldenTime)
	if err := backend.PutBytesIfAbsent(ctx, b, moved, sealed); err != nil {
		t.Fatalf("store: %v", err)
	}
	if _, err := Load(ctx, b, keys, moved); !errors.Is(err, crypto.ErrDecrypt) {
		t.Fatalf("load from another namespace: err = %v, want ErrDecrypt", err)
	}
}

func TestListIsOldestFirst(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	for i, at := range []time.Time{
		time.Date(2026, 3, 1, 0, 0, 0, 0, time.UTC),
		time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC),
		time.Date(2026, 2, 1, 0, 0, 0, 0, time.UTC),
	} {
		client := fmt.Sprintf("client%02d", i%2)
		if _, err := sample(client, at).Save(ctx, b, keys, crypto.DeterministicReader(fmt.Sprintf("list-%d", i))); err != nil {
			t.Fatalf("save: %v", err)
		}
	}
	want := []string{
		"snapshots/client01/20260101t000000.000000000z",
		"snapshots/client00/20260201t000000.000000000z",
		"snapshots/client00/20260301t000000.000000000z",
	}

	handles, err := List(ctx, b, "")
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(handles) != len(want) {
		t.Fatalf("listed %d snapshots, want %d", len(handles), len(want))
	}
	for i := range want {
		if handles[i].Key != want[i] {
			t.Errorf("snapshot %d = %s, want %s", i, handles[i].Key, want[i])
		}
	}

	perClient, err := List(ctx, b, "client00")
	if err != nil {
		t.Fatalf("list client00: %v", err)
	}
	if len(perClient) != 2 {
		t.Errorf("client00 has %d snapshots, want 2", len(perClient))
	}
}

func TestListOnAnEmptyRepository(t *testing.T) {
	handles, err := List(context.Background(), testBackend(t), "")
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(handles) != 0 {
		t.Errorf("listed %d snapshots, want 0", len(handles))
	}
}

func TestSaveRejectsIncompleteSnapshots(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	for name, mutate := range map[string]func(*Snapshot){
		"no root":         func(s *Snapshot) { s.Root = crypto.ID{} },
		"no client":       func(s *Snapshot) { s.ClientID = "" },
		"slash in client": func(s *Snapshot) { s.ClientID = "a/b" },
		"no time":         func(s *Snapshot) { s.TimeNs = 0 },
		"no paths":        func(s *Snapshot) { s.Paths = nil },
	} {
		t.Run(name, func(t *testing.T) {
			s := sample("client01", goldenTime)
			mutate(s)
			if _, err := s.Save(ctx, b, keys, crypto.DeterministicReader("bad")); !errors.Is(err, ErrCorrupt) {
				t.Fatalf("save: err = %v, want ErrCorrupt", err)
			}
		})
	}
}

// The key is a name and the object is the record. A reader that trusts
// one without checking the other can be lied to by a rename.
func TestLoadRejectsAKeyThatContradictsTheObject(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	// Seal a snapshot whose body claims a different time than its key.
	key := Key("client01", goldenTime)
	lying := sample("client01", goldenTime.Add(time.Hour))
	encoded, err := crypto.Marshal(lying)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	sealed, err := crypto.Seal(&keys.Meta, []byte(key), encoded, crypto.DeterministicReader("lie"))
	if err != nil {
		t.Fatalf("seal: %v", err)
	}
	if err := backend.PutBytesIfAbsent(ctx, b, key, sealed); err != nil {
		t.Fatalf("store: %v", err)
	}

	if _, err := Load(ctx, b, keys, key); !errors.Is(err, ErrCorrupt) {
		t.Fatalf("load: err = %v, want ErrCorrupt", err)
	}
}

func TestGoldenSnapshot(t *testing.T) {
	ctx := context.Background()
	keys, b := testKeys(t), testBackend(t)

	handle, err := sample("client01", goldenTime).Save(ctx, b, keys, crypto.DeterministicReader("golden-snapshot"))
	if err != nil {
		t.Fatalf("save: %v", err)
	}
	sealed, err := backend.GetAll(ctx, b, handle.Key)
	if err != nil {
		t.Fatalf("get: %v", err)
	}

	var out strings.Builder
	fmt.Fprintf(&out, "key %s\n", handle.Key)
	fmt.Fprintf(&out, "size %d\n", len(sealed))
	fmt.Fprintf(&out, "sealed %s\n", hex.EncodeToString(sealed))

	path := filepath.Join("testdata", "snapshot.txt")
	if *update {
		if err := os.WriteFile(path, []byte(out.String()), 0o644); err != nil {
			t.Fatalf("write golden: %v", err)
		}
		return
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read golden: %v (run: go test ./internal/snapshot/ -update)", err)
	}
	if out.String() != string(want) {
		t.Error("the snapshot object format changed")
	}
}
