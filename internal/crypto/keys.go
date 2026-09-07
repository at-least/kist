package crypto

import (
	"crypto/rand"
	"encoding/binary"
	"fmt"
	"io"
	"time"

	"golang.org/x/crypto/argon2"
	"lukechampine.com/blake3"
)

// RepoIDSize is the length of the random identifier minted by repo init.
// It salts subkey derivation, so two repositories never share a subkey
// even when they share a master key.
const RepoIDSize = 16

// A RepoID identifies one repository. It is public: it lives in the
// plaintext part of the config object.
type RepoID [RepoIDSize]byte

// Additional-authenticated-data domains. Every sealed object names the
// role it plays, so a ciphertext lifted from one place in the repository
// cannot be pasted into another.
//
// Objects whose name is derived from their own ciphertext (packs, index
// blobs) cannot use their name as AAD -- the name does not exist yet when
// the object is sealed -- so they use a constant domain string. Objects
// named by the hash of their plaintext (chunks, trees) use that name, and
// objects at a fixed location (snapshots, gc markers) use their full key
// path.
const (
	// AADPackTrailer binds a pack trailer to its role.
	AADPackTrailer = "kist/v2/pack-trailer"
	// AADIndexBlob binds an index blob to its role.
	AADIndexBlob = "kist/v2/index"
	// AADMasterKey prefixes the AAD wrapping the master key; the
	// repository ID and the chunker parameters follow, so a key slot
	// cannot be transplanted into another repository and a tampered
	// plaintext config cannot silently break deduplication.
	AADMasterKey = "kist/v2/master\x00"
)

// HKDF is gone in v2: subkeys are BLAKE3 DeriveKey outputs, one context
// per purpose. The strings are frozen format (docs/format.md §3).
const (
	infoHashKey  = "kist/v2/hash"
	infoChunkKey = "kist/v2/chunk"
	infoMetaKey  = "kist/v2/meta"
	infoIndexKey = "kist/v2/index"
)

// Keys is the unwrapped key material for an open repository.
//
// Each subkey has exactly one job, so compromise of one context does not
// spread: Chunk encrypts chunk payloads, Hash names content (chunks and
// trees), Index protects pack trailers and index blobs, Meta protects
// trees and snapshots. There is no nonce key in v2: nothing in the format
// uses a deterministic nonce.
type Keys struct {
	Master Key
	Chunk  Key
	Hash   Key
	Index  Key
	Meta   Key
}

// DeriveKeys expands a master key into the per-purpose subkeys via BLAKE3
// DeriveKey. The master-key AAD (not the derivation) is what binds the
// repository ID: two repositories never share a master key by construction.
func DeriveKeys(master Key) *Keys {
	keys := &Keys{Master: master}
	blake3.DeriveKey(keys.Hash[:], infoHashKey, master[:])
	blake3.DeriveKey(keys.Chunk[:], infoChunkKey, master[:])
	blake3.DeriveKey(keys.Meta[:], infoMetaKey, master[:])
	blake3.DeriveKey(keys.Index[:], infoIndexKey, master[:])
	return keys
}

// ContentIDv2 is the keyed content address of chunk plaintext under the hash
// key (crypto.ID keyed mode). Trees use the same function over their
// encoded bytes: see tree.Encode.
func ContentIDv2(keys *Keys, plaintext []byte) ID {
	return ContentID(&keys.Hash, plaintext)
}

// KDFAlgArgon2id is the only password KDF this format defines.
const KDFAlgArgon2id = "argon2id"

// KDFParams records how a password was stretched into a key-encryption
// key. It is stored in plaintext: without it the slot cannot be opened
// even with the right password, and it reveals nothing a repository
// holder does not already have.
type KDFParams struct {
	Alg       string `cbor:"alg"`
	Time      uint32 `cbor:"t"`
	MemoryKiB uint32 `cbor:"m"`
	Threads   uint8  `cbor:"p"`
	Salt      []byte `cbor:"salt"`
}

// SaltSize is the length of the Argon2id salt in a key slot.
const SaltSize = 16

// Upper bounds enforced before deriving: the config is plaintext, so its
// parameters are untrusted, and the bound is what keeps a tampered slot
// from turning "open the repository" into "allocate 4 GiB".
const (
	MaxKDFMemoryKiB = 1024 * 1024 // 1 GiB
	MaxKDFTCost     = 64
	MaxKDFThreads   = 64
)

// DefaultKDFParams is RFC 9106's second recommended option: 64 MiB of
// memory, three passes, four lanes. The first option (2 GiB) is not
// something a backup client can assume it may allocate on a machine that
// is also doing the backup.
func DefaultKDFParams() KDFParams {
	return KDFParams{
		Alg:       KDFAlgArgon2id,
		Time:      3,
		MemoryKiB: 64 * 1024,
		Threads:   4,
	}
}

// A KeySlot wraps the master key under a key-encryption key derived from
// one password. A repository has one slot in its config object and may
// have more under keys/<id>, so several passwords can open one repository
// without any of them being able to reach another's.
// Field order is the spec table (docs/format.md §4): v, name, created,
// kdf, wrapped. Reordering changes every byte it feeds.
type KeySlot struct {
	Version       uint64    `cbor:"v"`
	Name          string    `cbor:"name,omitempty"`
	CreatedUnixNs int64     `cbor:"created"`
	KDF           KDFParams `cbor:"kdf"`
	WrappedMaster []byte    `cbor:"wrapped"`
}

// KeySlotVersion is the schema version written by this implementation.
const KeySlotVersion = 2

// NewKeySlot wraps master under a key derived from password.
//
// randSource supplies the salt and the envelope nonce; production callers
// pass nil for crypto/rand. now is the creation timestamp, injected so
// that golden files are reproducible.
func NewKeySlot(password []byte, aad []byte, master Key, params KDFParams, now time.Time, randSource io.Reader) (*KeySlot, error) {
	if params.Alg != KDFAlgArgon2id {
		return nil, fmt.Errorf("key slot: unsupported kdf %q, want %q", params.Alg, KDFAlgArgon2id)
	}
	if randSource == nil {
		randSource = rand.Reader
	}

	params.Salt = make([]byte, SaltSize)
	if _, err := io.ReadFull(randSource, params.Salt); err != nil {
		return nil, fmt.Errorf("key slot: read salt: %w", err)
	}

	kek, err := deriveKEK(password, params)
	if err != nil {
		return nil, err
	}

	wrapped, err := Seal(&kek, aad, master[:], randSource)
	if err != nil {
		return nil, fmt.Errorf("key slot: wrap master key: %w", err)
	}

	return &KeySlot{
		Version:       KeySlotVersion,
		KDF:           params,
		WrappedMaster: wrapped,
		CreatedUnixNs: now.UTC().UnixNano(),
	}, nil
}

// ErrWrongPassword is returned when a slot will not open under the given
// password. It wraps ErrDecrypt: a wrong password and a corrupted slot
// are indistinguishable by construction, and this is the friendlier of
// the two explanations to lead with.
var ErrWrongPassword = fmt.Errorf("wrong password or damaged key slot: %w", ErrDecrypt)

// Unwrap recovers the master key from the slot.
func (s *KeySlot) Unwrap(password []byte, aad []byte) (Key, error) {
	var master Key

	if s.Version != KeySlotVersion {
		return master, fmt.Errorf("key slot: version %d is not supported, want %d", s.Version, KeySlotVersion)
	}

	kek, err := deriveKEK(password, s.KDF)
	if err != nil {
		return master, err
	}

	plain, err := Open(&kek, aad, s.WrappedMaster)
	if err != nil {
		return master, ErrWrongPassword
	}
	if len(plain) != KeySize {
		return master, fmt.Errorf("key slot: wrapped master key is %d bytes, want %d", len(plain), KeySize)
	}

	copy(master[:], plain)
	return master, nil
}

// MasterAAD builds the AAD that binds a wrapped master key to its
// repository: prefix ‖ repo_id(16) ‖ chunker min/avg/max (u32 LE).
// The chunker parameters are part of it because the config that carries
// them is plaintext: tampering must fail the unwrap, not silently break
// deduplication.
func MasterAAD(repoID RepoID, minSize, avgSize, maxSize uint32) []byte {
	aad := make([]byte, 0, len(AADMasterKey)+16+12)
	aad = append(aad, AADMasterKey...)
	aad = append(aad, repoID[:]...)
	aad = binary.LittleEndian.AppendUint32(aad, minSize)
	aad = binary.LittleEndian.AppendUint32(aad, avgSize)
	aad = binary.LittleEndian.AppendUint32(aad, maxSize)
	return aad
}

func deriveKEK(password []byte, params KDFParams) (Key, error) {
	var kek Key

	switch {
	case params.Alg != KDFAlgArgon2id:
		return kek, fmt.Errorf("key slot: unsupported kdf %q, want %q", params.Alg, KDFAlgArgon2id)
	case params.Time == 0 || params.Time > MaxKDFTCost:
		return kek, fmt.Errorf("key slot: argon2id time cost %d is outside 1..%d", params.Time, MaxKDFTCost)
	case params.MemoryKiB == 0 || params.MemoryKiB > MaxKDFMemoryKiB:
		return kek, fmt.Errorf("key slot: argon2id memory cost %d KiB is outside 1..%d", params.MemoryKiB, MaxKDFMemoryKiB)
	case params.Threads == 0 || params.Threads > MaxKDFThreads:
		return kek, fmt.Errorf("key slot: argon2id parallelism %d is outside 1..%d", params.Threads, MaxKDFThreads)
	case len(params.Salt) != SaltSize:
		return kek, fmt.Errorf("key slot: salt is %d bytes, want %d", len(params.Salt), SaltSize)
	}

	copy(kek[:], argon2.IDKey(password, params.Salt, params.Time, params.MemoryKiB, params.Threads, KeySize))
	return kek, nil
}
