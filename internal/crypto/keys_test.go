package crypto

import (
	"bytes"
	"encoding/binary"
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

// The master-key AAD is what binds a slot to its repository in v2: it
// carries the repository ID and the chunker parameters, each as a distinct
// byte range, so tampering with the plaintext config fails the unwrap
// instead of silently breaking deduplication.
func TestMasterAADEncodesRepoIDAndChunkerParams(t *testing.T) {
	const min, avg, max = 512 << 10, 2 << 20, 8 << 20

	aad := MasterAAD(goldenRepoID, min, avg, max)
	if got, want := len(aad), len(AADMasterKey)+RepoIDSize+12; got != want {
		t.Fatalf("aad length = %d, want %d", got, want)
	}
	if !bytes.HasPrefix(aad, []byte(AADMasterKey)) {
		t.Errorf("aad does not start with the domain: %x", aad)
	}
	rest := aad[len(AADMasterKey):]
	if !bytes.Equal(rest[:RepoIDSize], goldenRepoID[:]) {
		t.Errorf("repository ID is not carried verbatim: %x", rest[:RepoIDSize])
	}
	fields := []uint32{min, avg, max}
	for i, want := range fields {
		off := RepoIDSize + 4*i
		if got := binary.LittleEndian.Uint32(rest[off:]); got != want {
			t.Errorf("field %d = %d, want %d (little endian)", i, got, want)
		}
	}
}

// One master key, two repositories: the AADs differ, so a slot wrapped
// under one cannot open under the other (exercised end to end below).
func TestMasterAADDiffersPerRepositoryAndParams(t *testing.T) {
	base := MasterAAD(goldenRepoID, 1, 2, 3)

	otherRepo := goldenRepoID
	otherRepo[0] ^= 0xff
	if bytes.Equal(base, MasterAAD(otherRepo, 1, 2, 3)) {
		t.Error("the AAD ignores the repository ID")
	}
	if bytes.Equal(base, MasterAAD(goldenRepoID, 1, 2, 4)) {
		t.Error("the AAD ignores the chunker max size")
	}
	if bytes.Equal(MasterAAD(goldenRepoID, 1, 2, 3), MasterAAD(goldenRepoID, 3, 2, 1)) {
		t.Error("the chunker parameters are not in fixed order")
	}
}

func goldenAAD() []byte {
	return MasterAAD(goldenRepoID, 512<<10, 2<<20, 8<<20)
}

func cheapParams() KDFParams {
	p := DefaultKDFParams()
	p.Time, p.MemoryKiB, p.Threads = 1, 8, 1
	return p
}

func TestKeySlotRoundTrip(t *testing.T) {
	password := []byte("hunter2")

	slot, err := NewKeySlot(password, goldenAAD(), goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
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

	master, err := slot.Unwrap(password, goldenAAD())
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if master != goldenMaster {
		t.Error("unwrapped master key does not match")
	}
}

func TestKeySlotRejectsWrongPassword(t *testing.T) {
	slot, err := NewKeySlot([]byte("hunter2"), goldenAAD(), goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}

	if _, err := slot.Unwrap([]byte("hunter3"), goldenAAD()); !errors.Is(err, ErrWrongPassword) {
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
	slot, err := NewKeySlot(password, goldenAAD(), goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}

	otherRepo := goldenRepoID
	otherRepo[15] ^= 0x01
	if _, err := slot.Unwrap(password, MasterAAD(otherRepo, 512<<10, 2<<20, 8<<20)); !errors.Is(err, ErrWrongPassword) {
		t.Fatalf("unwrap in another repository: err = %v, want failure", err)
	}
	// A repository whose plaintext config lies about its chunker
	// parameters is the more interesting transplant: the unwrap must fail,
	// not silently return a key that deduplicates against nothing.
	if _, err := slot.Unwrap(password, MasterAAD(goldenRepoID, 512<<10, 2<<20, 4<<20)); !errors.Is(err, ErrWrongPassword) {
		t.Fatalf("unwrap with tampered chunker params: err = %v, want failure", err)
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
			slot, err := NewKeySlot([]byte("pw"), goldenAAD(), goldenMaster, base, goldenTime, DeterministicReader("slot"))
			if err != nil {
				t.Fatalf("new key slot: %v", err)
			}
			tc.mutte(&slot.KDF)

			_, err = slot.Unwrap([]byte("pw"), goldenAAD())
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
	slot, err := NewKeySlot([]byte("pw"), goldenAAD(), goldenMaster, cheapParams(), goldenTime, DeterministicReader("slot"))
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}
	slot.Version = KeySlotVersion + 1

	if _, err := slot.Unwrap([]byte("pw"), goldenAAD()); err == nil || !strings.Contains(err.Error(), "version") {
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

	slot, err := NewKeySlot([]byte("pw"), goldenAAD(), goldenMaster, DefaultKDFParams(), goldenTime, nil)
	if err != nil {
		t.Fatalf("new key slot: %v", err)
	}
	master, err := slot.Unwrap([]byte("pw"), goldenAAD())
	if err != nil {
		t.Fatalf("unwrap: %v", err)
	}
	if master != goldenMaster {
		t.Error("unwrapped master key does not match")
	}
	t.Logf("argon2id t3 m64MiB p4, two derivations: %v", time.Since(start))
}
