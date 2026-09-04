package crypto

import (
	"crypto/hkdf"
	"crypto/rand"
	"crypto/sha256"
	"fmt"
	"io"
	"time"

	"golang.org/x/crypto/argon2"
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
	AADPackTrailer = "kist/v1/pack-trailer"
	// AADIndexBlob binds an index blob to its role.
	AADIndexBlob = "kist/v1/index"
	// AADMasterKey prefixes the AAD wrapping the master key; the
	// repository ID is appended, so a key slot cannot be transplanted
	// into another repository.
	AADMasterKey = "kist/v1/master"
)

// HKDF info strings, one per subkey. Changing one of these strings
// changes the key it derives, which would make an existing repository
// unreadable; they are part of the frozen format.
const (
	infoChunkKey = "kist/v1/chunk"
	infoHashKey  = "kist/v1/hash"
	infoIndexKey = "kist/v1/index"
	infoMetaKey  = "kist/v1/meta"
)

// Keys is the unwrapped key material for an open repository.
//
// Each subkey has exactly one job, so compromise of one context does not
// spread: Chunk encrypts chunk payloads, Hash names content, Index
// protects pack trailers and index blobs, Meta protects trees, snapshots
// and gc markers.
type Keys struct {
	Master Key
	Chunk  Key
	Hash   Key
	Index  Key
	Meta   Key
}

// DeriveKeys expands a master key into the per-purpose subkeys, salted
// with the repository ID.
func DeriveKeys(master Key, repoID RepoID) (*Keys, error) {
	keys := &Keys{Master: master}

	for _, sub := range []struct {
		info string
		dst  *Key
	}{
		{infoChunkKey, &keys.Chunk},
		{infoHashKey, &keys.Hash},
		{infoIndexKey, &keys.Index},
		{infoMetaKey, &keys.Meta},
	} {
		derived, err := hkdf.Key(sha256.New, master[:], repoID[:], sub.info, KeySize)
		if err != nil {
			return nil, fmt.Errorf("derive %s: %w", sub.info, err)
		}
		copy(sub.dst[:], derived)
	}

	return keys, nil
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
type KeySlot struct {
	Version       uint64    `cbor:"v"`
	KDF           KDFParams `cbor:"kdf"`
	WrappedMaster []byte    `cbor:"wrapped"`
	CreatedUnixNs int64     `cbor:"created"`
}

// KeySlotVersion is the schema version written by this implementation.
const KeySlotVersion = 1

// NewKeySlot wraps master under a key derived from password.
//
// randSource supplies the salt and the envelope nonce; production callers
// pass nil for crypto/rand. now is the creation timestamp, injected so
// that golden files are reproducible.
func NewKeySlot(password []byte, repoID RepoID, master Key, params KDFParams, now time.Time, randSource io.Reader) (*KeySlot, error) {
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

	wrapped, err := Seal(&kek, masterAAD(repoID), master[:], randSource)
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
func (s *KeySlot) Unwrap(password []byte, repoID RepoID) (Key, error) {
	var master Key

	if s.Version != KeySlotVersion {
		return master, fmt.Errorf("key slot: version %d is not supported, want %d", s.Version, KeySlotVersion)
	}

	kek, err := deriveKEK(password, s.KDF)
	if err != nil {
		return master, err
	}

	plain, err := Open(&kek, masterAAD(repoID), s.WrappedMaster)
	if err != nil {
		return master, ErrWrongPassword
	}
	if len(plain) != KeySize {
		return master, fmt.Errorf("key slot: wrapped master key is %d bytes, want %d", len(plain), KeySize)
	}

	copy(master[:], plain)
	return master, nil
}

func masterAAD(repoID RepoID) []byte {
	return append([]byte(AADMasterKey), repoID[:]...)
}

func deriveKEK(password []byte, params KDFParams) (Key, error) {
	var kek Key

	switch {
	case params.Alg != KDFAlgArgon2id:
		return kek, fmt.Errorf("key slot: unsupported kdf %q, want %q", params.Alg, KDFAlgArgon2id)
	case params.Time == 0:
		return kek, fmt.Errorf("key slot: argon2id time cost is 0")
	case params.MemoryKiB == 0:
		return kek, fmt.Errorf("key slot: argon2id memory cost is 0")
	case params.Threads == 0:
		return kek, fmt.Errorf("key slot: argon2id parallelism is 0")
	case len(params.Salt) != SaltSize:
		return kek, fmt.Errorf("key slot: salt is %d bytes, want %d", len(params.Salt), SaltSize)
	}

	copy(kek[:], argon2.IDKey(password, params.Salt, params.Time, params.MemoryKiB, params.Threads, KeySize))
	return kek, nil
}
