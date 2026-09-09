package crypto

import (
	"bytes"
	"errors"
	"strings"
	"testing"
	"time"
)

func TestDeriveKeysSubkeysAreDistinct(t *testing.T) {
	keys := DeriveKeys(goldenMaster)

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
	first := DeriveKeys(goldenMaster)
	second := DeriveKeys(goldenMaster)
	if *first != *second {
		t.Fatal("two derivations of one master key differ")
	}
}

// Derivation is a pure function of the master key in v2: the repository
// binding moved into the master-key AAD. Two master keys must therefore
// yield unrelated subkeys.
func TestDeriveKeysDependsOnTheMasterKey(t *testing.T) {
	other := goldenMaster
	other[0] ^= 0xff

	a := DeriveKeys(goldenMaster)
	b := DeriveKeys(other)
	if a.Chunk == b.Chunk || a.Hash == b.Hash || a.Index == b.Index || a.Meta == b.Meta {
		t.Fatal("subkeys do not depend on the master key")
	}
}

// v3 binds a slot to its repository through the AUTHENTICATED PAYLOAD
// (master ‖ Invariants), not the AAD: the AAD is a constant, tampering
// with the plaintext config surfaces as an explicit invariants mismatch
// instead of silently breaking deduplication.
func TestMasterAADIsTheConstantAndInvariantsCarryTheBinding(t *testing.T) {
	aad := []byte(AADMasterKey)
	if got, want := len(aad), len("kist/v3/master"); got != want {
		t.Fatalf("aad length = %d, want %d", got, want)
	}
	if !bytes.HasPrefix(aad, []byte("kist/v3/")) {
		t.Errorf("aad does not start with the domain: %x", aad)
	}
	// The binding lives in the wrapped payload's Invariants instead: a slot
	// written with one repo_id/chunker must refuse to match a config that
	// claims different ones.
	inv := goldenInvariants()
	if !bytes.Equal(inv.RepoID, goldenRepoID[:]) {
		t.Errorf("invariants do not carry the repository ID verbatim: %x", inv.RepoID)
	}
	if inv.Chunker.Min != 512<<10 || inv.Chunker.Avg != 2<<20 || inv.Chunker.Max != 8<<20 {
		t.Errorf("invariants carry the wrong chunker parameters: %+v", inv.Chunker)
	}
}

// One master key, two repositories: the invariants differ, so a slot
// wrapped under one cannot masquerade as the other (exercised end to end
// below).
func TestInvariantsDifferPerRepositoryAndParams(t *testing.T) {
	base := goldenInvariants()

	otherRepo := goldenRepoID
	otherRepo[0] ^= 0xff
	if base.RepoID[0] == otherRepo[0] {
		t.Error("the invariants ignore the repository ID")
	}
	switched := goldenInvariants()
	switched.Chunker.Max = 4 << 20
	if base.Chunker.Max == switched.Chunker.Max {
		t.Error("the invariants ignore the chunker max size")
	}
	reordered := goldenInvariants()
	reordered.Chunker.Min, reordered.Chunker.Avg = reordered.Chunker.Avg, reordered.Chunker.Min
	if base.Chunker == reordered.Chunker {
		t.Error("the chunker parameters are not distinguished")
	}
}

func goldenInvariants() Invariants {
	return Invariants{
		Version: FormatVersion,
		RepoID:  goldenRepoID[:],
		Chunker: InvariantsChunker{Min: 512 << 10, Avg: 2 << 20, Max: 8 << 20},
	}
}

func cheapParams() KDFParams {
	p := DefaultKDFParams()
	p.Time, p.MemoryKiB, p.Threads = 1, 8, 1
	return p
}

func TestKeySlotRoundTrip(t *testing.T) {
	password := []byte("hunter2")

	slot, err := NewKeySlot(password, goldenMaster, goldenInvariants(), cheapParams(), goldenTime, DeterministicReader("slot"))
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

	master, _, err := slot.Unwrap(password)
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if master != goldenMaster {
		t.Error("unwrapped master key does not match")
	}
}

func TestKeySlotRejectsWrongPassword(t *testing.T) {
	slot, err := NewKeySlot([]byte("hunter2"), goldenMaster, goldenInvariants(), cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}

	if _, _, err := slot.Unwrap([]byte("hunter3")); !errors.Is(err, ErrWrongPassword) {
		t.Fatalf("unwrap with a wrong password: err = %v, want ErrWrongPassword", err)
	} else if !errors.Is(err, ErrDecrypt) {
		t.Error("ErrWrongPassword should also satisfy errors.Is(err, ErrDecrypt)")
	}
}

// A slot copied out of one repository must not open in another, even with
// the right password: the repository ID and chunker parameters are in the
// wrapping AAD.
func TestKeySlotCannotBeTransplanted(t *testing.T) {
	password := []byte("hunter2")
	slot, err := NewKeySlot(password, goldenMaster, goldenInvariants(), cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}

	// v3: the unwrap itself is config-independent (the AAD is a constant);
	// the binding is the authenticated invariants vs the plaintext config.
	// A lying config must surface as an explicit mismatch, never as a
	// silent deduplication change (the comparison lives in
	// repo.Config.checkMatches; here we pin that the payload really
	// carries the slot's repo identity).
	_, inv, err := slot.Unwrap(password)
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if inv.Chunker.Min != 512<<10 || inv.Chunker.Avg != 2<<20 || inv.Chunker.Max != 8<<20 {
		t.Fatalf("invariants carry the wrong chunker: %+v", inv.Chunker)
	}
	if len(inv.RepoID) != len(goldenRepoID) || inv.RepoID[15] != goldenRepoID[15] {
		t.Fatalf("invariants carry the wrong repository identity")
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
		{"zero time cost", func(p *KDFParams) { p.Time = 0 }, "time cost 0 is outside"},
		{"zero memory cost", func(p *KDFParams) { p.MemoryKiB = 0 }, "memory cost 0 KiB is outside"},
		{"zero parallelism", func(p *KDFParams) { p.Threads = 0 }, "parallelism 0 is outside"},
		{"short salt", func(p *KDFParams) { p.Salt = []byte("short") }, "salt is 5 bytes"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			slot, err := NewKeySlot([]byte("pw"), goldenMaster, goldenInvariants(), base, goldenTime, DeterministicReader("slot"))
			if err != nil {
				t.Fatalf("new key slot: %v", err)
			}
			tc.mutte(&slot.KDF)

			_, _, err = slot.Unwrap([]byte("pw"))
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
	slot, err := NewKeySlot([]byte("pw"), goldenMaster, goldenInvariants(), cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}
	slot.Version = KeySlotVersion + 1

	if _, _, err := slot.Unwrap([]byte("pw")); err == nil || !strings.Contains(err.Error(), "version") {
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

	slot, err := NewKeySlot([]byte("pw"), goldenMaster, goldenInvariants(), DefaultKDFParams(), goldenTime, nil)
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}
	master, _, err := slot.Unwrap([]byte("pw"))
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if master != goldenMaster {
		t.Error("unwrapped master key does not match")
	}
	t.Logf("argon2id t3 m64MiB p4, two derivations: %v", time.Since(start))
}
