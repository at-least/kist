package crypto

import (
	"encoding/hex"
	"flag"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

var update = flag.Bool("update", false, "rewrite testdata golden files")

// checkGolden compares got against testdata/<name>, or rewrites it under
// -update. Format-relevant code must not change these files by accident:
// a diff here means the on-disk format moved.
func checkGolden(t *testing.T, name string, got []byte) {
	t.Helper()

	path := filepath.Join("testdata", name)
	if *update {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatalf("golden %s: %v", name, err)
		}
		if err := os.WriteFile(path, got, 0o644); err != nil {
			t.Fatalf("golden %s: %v", name, err)
		}
		return
	}

	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("golden %s: %v (run: go test ./... -update)", name, err)
	}
	if string(got) != string(want) {
		t.Errorf("golden %s changed.\n got: %s\nwant: %s", name, got, want)
	}
}

func hexDump(b []byte) []byte { return []byte(hex.EncodeToString(b) + "\n") }

// The frozen inputs every golden in this package is built from.
var (
	goldenMaster = Key{
		0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
		0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
		0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
		0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
	}
	goldenRepoID = RepoID{
		0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
		0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
	}
	goldenTime = time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
)

func TestGoldenEnvelope(t *testing.T) {
	sealed, err := Seal(&goldenMaster, []byte("kist/v1/golden"), []byte("kist golden plaintext"), DeterministicReader("golden-envelope"))
	if err != nil {
		t.Fatalf("seal: %v", err)
	}
	checkGolden(t, "envelope.hex", hexDump(sealed))
}

func TestGoldenSubkeys(t *testing.T) {
	keys := DeriveKeys(goldenMaster)

	var b strings.Builder
	for _, sub := range []struct {
		name string
		key  Key
	}{
		{"chunk", keys.Chunk},
		{"hash", keys.Hash},
		{"index", keys.Index},
		{"meta", keys.Meta},
	} {
		b.WriteString(sub.name + " " + hex.EncodeToString(sub.key[:]) + "\n")
	}
	checkGolden(t, "subkeys.txt", []byte(b.String()))
}

func TestGoldenContentID(t *testing.T) {
	keys := DeriveKeys(goldenMaster)

	var b strings.Builder
	for _, in := range []string{"", "a", "hello world"} {
		b.WriteString(ContentID(&keys.Hash, []byte(in)).String() + " content " + hex.EncodeToString([]byte(in)) + "\n")
		b.WriteString(CiphertextID([]byte(in)).String() + " ciphertext " + hex.EncodeToString([]byte(in)) + "\n")
	}
	checkGolden(t, "ids.txt", []byte(b.String()))
}

func TestGoldenKeySlot(t *testing.T) {
	params := DefaultKDFParams()
	// Cheap parameters: a golden file must be recomputable in a unit test,
	// and 64 MiB of Argon2 per run is not that. The production defaults
	// are exercised by TestDefaultKDFParamsMatchRFC9106.
	params.Time, params.MemoryKiB, params.Threads = 1, 8, 1

	slot, err := NewKeySlot([]byte("correct horse battery staple"), goldenMaster, goldenInvariants(), params, goldenTime, DeterministicReader("golden-slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}

	encoded, err := Marshal(slot)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	checkGolden(t, "keyslot.hex", hexDump(encoded))

	// The golden is only meaningful if it still opens.
	var decoded KeySlot
	if err := Unmarshal(encoded, &decoded); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	master, invariants, err := decoded.Unwrap([]byte("correct horse battery staple"))
	_ = invariants
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if master != goldenMaster {
		t.Error("unwrapped master key does not match")
	}
}
