package crypto

import (
	"bytes"
	"errors"
	"strings"
	"testing"
	"time"
)

func TestDeriveKeysSubkeysAreDistinct(t *testing.T) {
	keys, err := DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}

	named := map[string]Key{
		"master": keys.Master,
		"chunk":  keys.Chunk,
		"hash":   keys.Hash,
		"index":  keys.Index,
		"meta":   keys.Meta,
	}
	for aName, a := range named {
		for bName, b := range named {
			if aName < bName && a == b {
				t.Errorf("%s and %s keys are identical", aName, bName)
			}
		}
	}
	if keys.Master != goldenMaster {
		t.Error("Master is not the key that was passed in")
	}
}

func TestDeriveKeysIsDeterministic(t *testing.T) {
	first, err := DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	second, err := DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	if *first != *second {
		t.Fatal("two derivations of one master key differ")
	}
}

// The repository ID salts derivation, so one master key reused in two
// repositories still yields unrelated subkeys.
func TestDeriveKeysIsSaltedByRepoID(t *testing.T) {
	other := goldenRepoID
	other[0] ^= 0xff

	a, err := DeriveKeys(goldenMaster, goldenRepoID)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	b, err := DeriveKeys(goldenMaster, other)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	if a.Chunk == b.Chunk || a.Hash == b.Hash || a.Index == b.Index || a.Meta == b.Meta {
		t.Fatal("subkeys do not depend on the repository ID")
	}
}

func cheapParams() KDFParams {
	p := DefaultKDFParams()
	p.Time, p.MemoryKiB, p.Threads = 1, 8, 1
	return p
}

func TestKeySlotRoundTrip(t *testing.T) {
	password := []byte("hunter2")

	slot, err := NewKeySlot(password, goldenRepoID, goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}
	if bytes.Contains(slot.WrappedMaster, goldenMaster[:]) {
		t.Fatal("master key appears verbatim in the wrapped slot")
	}
	if len(slot.KDF.Salt) != SaltSize {
		t.Errorf("salt is %d bytes, want %d", len(slot.KDF.Salt), SaltSize)
	}
	if slot.CreatedUnixNs != goldenTime.UnixNano() {
		t.Errorf("created = %d, want %d", slot.CreatedUnixNs, goldenTime.UnixNano())
	}

	master, err := slot.Unwrap(password, goldenRepoID)
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if master != goldenMaster {
		t.Error("unwrapped master key does not match")
	}
}

func TestKeySlotRejectsWrongPassword(t *testing.T) {
	slot, err := NewKeySlot([]byte("hunter2"), goldenRepoID, goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}

	if _, err := slot.Unwrap([]byte("hunter3"), goldenRepoID); !errors.Is(err, ErrWrongPassword) {
		t.Fatalf("unwrap with a wrong password: err = %v, want ErrWrongPassword", err)
	} else if !errors.Is(err, ErrDecrypt) {
		t.Error("ErrWrongPassword should also satisfy errors.Is(err, ErrDecrypt)")
	}
}

// A slot copied out of one repository must not open in another, even with
// the right password: the repository ID is in the wrapping AAD.
func TestKeySlotCannotBeTransplanted(t *testing.T) {
	password := []byte("hunter2")
	slot, err := NewKeySlot(password, goldenRepoID, goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}

	other := goldenRepoID
	other[15] ^= 0x01
	if _, err := slot.Unwrap(password, other); !errors.Is(err, ErrWrongPassword) {
		t.Fatalf("unwrap in another repository: err = %v, want failure", err)
	}
}

func TestKeySlotRejectsBadParameters(t *testing.T) {
	base := cheapParams()

	cases := []struct {
		name  string
		mutte func(*KDFParams)
		want  string
	}{
		{"unknown algorithm", func(p *KDFParams) { p.Alg = "scrypt" }, "unsupported kdf"},
		{"zero time cost", func(p *KDFParams) { p.Time = 0 }, "time cost is 0"},
		{"zero memory cost", func(p *KDFParams) { p.MemoryKiB = 0 }, "memory cost is 0"},
		{"zero parallelism", func(p *KDFParams) { p.Threads = 0 }, "parallelism is 0"},
		{"short salt", func(p *KDFParams) { p.Salt = []byte("short") }, "salt is 5 bytes"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			slot, err := NewKeySlot([]byte("pw"), goldenRepoID, goldenMaster, base, goldenTime, DeterministicReader("slot"))
			if err != nil {
				t.Fatalf("new key slot: %v", err)
			}
			tc.mutte(&slot.KDF)

			_, err = slot.Unwrap([]byte("pw"), goldenRepoID)
			if err == nil {
				t.Fatal("unwrap: got nil error, want failure")
			}
			if !strings.Contains(err.Error(), tc.want) {
				t.Errorf("error = %q, want it to contain %q", err, tc.want)
			}
		})
	}
}

func TestKeySlotRejectsUnknownVersion(t *testing.T) {
	slot, err := NewKeySlot([]byte("pw"), goldenRepoID, goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}
	slot.Version = KeySlotVersion + 1

	if _, err := slot.Unwrap([]byte("pw"), goldenRepoID); err == nil || !strings.Contains(err.Error(), "version") {
		t.Fatalf("unwrap of a future version: err = %v, want a version error", err)
	}
}

// RFC 9106 §4 second recommended option. Weakening these silently is the
// kind of change that should have to edit a test.
func TestDefaultKDFParamsMatchRFC9106(t *testing.T) {
	p := DefaultKDFParams()
	if p.Alg != KDFAlgArgon2id {
		t.Errorf("alg = %q, want %q", p.Alg, KDFAlgArgon2id)
	}
	if p.Time != 3 || p.MemoryKiB != 64*1024 || p.Threads != 4 {
		t.Errorf("params = t%d m%d p%d, want t3 m65536 p4", p.Time, p.MemoryKiB, p.Threads)
	}
}

func TestKeySlotWithProductionParameters(t *testing.T) {
	if testing.Short() {
		t.Skip("argon2id at 64 MiB is slow; covered by -short=false")
	}
	start := time.Now()

	slot, err := NewKeySlot([]byte("pw"), goldenRepoID, goldenMaster, DefaultKDFParams(), goldenTime, nil)
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}
	master, err := slot.Unwrap([]byte("pw"), goldenRepoID)
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if master != goldenMaster {
		t.Error("unwrapped master key does not match")
	}
	t.Logf("argon2id t3 m64MiB p4, two derivations: %v", time.Since(start))
}
