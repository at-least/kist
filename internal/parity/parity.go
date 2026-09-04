// Package parity adds Reed-Solomon redundancy to packs, as sidecar
// objects, so that a damaged pack can be repaired in place.
//
// Parity is computed over the sealed pack bytes -- ciphertext, trailer
// and tail alike -- split into DataShards equal shards. A damaged shard
// is found by its hash, reconstructed from the others, and the result is
// accepted only if the whole pack then hashes to its own name. The AEAD
// tags say a chunk is bad; parity says how to fix it; the name says the
// fix is right.
//
// The object is plaintext on purpose. Reed-Solomon over ciphertext is a
// linear combination of pseudo-random bytes and leaks nothing; the shard
// hashes are hashes of ciphertext; and a forged or damaged parity object
// can only make a repair fail, never succeed wrongly, because the proof
// of a repair is the pack's name. That is what lets a scrub run without
// the repository password.
package parity

import (
	"bytes"
	"errors"
	"fmt"

	"github.com/klauspost/reedsolomon"

	"github.com/at-least/kist/internal/crypto"
)

// Prefix is the repository prefix parity objects live under.
const Prefix = "parity/"

// Version is the parity object schema version.
const Version = 1

// DataShards is the fixed number of data shards a pack is split into.
// Fixed, so that the overhead of M parity shards is exactly M/16 and a
// reader needs no negotiation.
const DataShards = 16

// MaxParityShards bounds M: half the data is as much redundancy as a
// backup format should offer before the answer is a second repository.
const MaxParityShards = 8

// maxShardLen bounds the allocation a parsed object can ask for. Packs
// are at most TargetSize + MaxSize, far under a gibibyte.
const maxShardLen = 64 << 20

// Sentinel errors.
var (
	// ErrCorrupt means a parity object is structurally invalid.
	ErrCorrupt = errors.New("parity object is corrupt")

	// ErrUnrepairable means the damage exceeds what the parity can
	// reconstruct, or the reconstruction did not hash to the pack's name.
	ErrUnrepairable = errors.New("pack cannot be repaired")
)

// Key returns the parity object's key for a pack.
func Key(packID crypto.ID) string { return Prefix + packID.String() }

// Object is the decoded parity sidecar.
type Object struct {
	Version  uint64 `cbor:"v"`
	K        uint8  `cbor:"k"`
	M        uint8  `cbor:"m"`
	PackSize uint64 `cbor:"pack_size"`
	ShardLen uint32 `cbor:"shard_len"`

	// Hashes are the unkeyed BLAKE3-256 of every shard, data shards
	// first, then parity. They are what finds the damaged ones.
	Hashes []crypto.ID `cbor:"hashes"`

	// Parity holds the M parity shards.
	Parity [][]byte `cbor:"parity"`
}

// Encode computes the parity object for a pack with m parity shards.
func Encode(packID crypto.ID, pack []byte, m int) ([]byte, error) {
	if m < 1 || m > MaxParityShards {
		return nil, fmt.Errorf("encode parity: %d parity shards, want 1..%d", m, MaxParityShards)
	}
	if len(pack) == 0 {
		return nil, errors.New("encode parity: empty pack")
	}
	if got := crypto.CiphertextID(pack); got != packID {
		return nil, fmt.Errorf("encode parity: bytes hash to %s, not %s", got, packID)
	}
	shardLen := (len(pack) + DataShards - 1) / DataShards
	shards := shard(pack, shardLen, m)

	enc, err := reedsolomon.New(DataShards, m)
	if err != nil {
		return nil, fmt.Errorf("encode parity: %w", err)
	}
	if err := enc.Encode(shards); err != nil {
		return nil, fmt.Errorf("encode parity: %w", err)
	}

	obj := Object{
		Version: Version, K: DataShards, M: uint8(m), //nolint:gosec // m <= MaxParityShards
		PackSize: uint64(len(pack)), ShardLen: uint32(shardLen), //nolint:gosec // bounded by the pack size
		Hashes: make([]crypto.ID, 0, DataShards+m), Parity: shards[DataShards:],
	}
	for _, s := range shards {
		obj.Hashes = append(obj.Hashes, crypto.CiphertextID(s))
	}
	encoded, err := crypto.Marshal(obj)
	if err != nil {
		return nil, fmt.Errorf("encode parity: %w", err)
	}
	return encoded, nil
}

// shard splits pack into DataShards data shards of shardLen bytes, the
// last zero-padded, followed by m empty parity shards for the encoder to
// fill.
func shard(pack []byte, shardLen, m int) [][]byte {
	shards := make([][]byte, DataShards+m)
	padded := make([]byte, DataShards*shardLen)
	copy(padded, pack)
	for i := range DataShards {
		shards[i] = padded[i*shardLen : (i+1)*shardLen]
	}
	for i := DataShards; i < DataShards+m; i++ {
		shards[i] = make([]byte, shardLen)
	}
	return shards
}

// Parse decodes and validates a parity object. Every bound is checked
// before anything sized by the object is allocated.
func Parse(data []byte) (*Object, error) {
	var o Object
	if err := crypto.Unmarshal(data, &o); err != nil {
		return nil, fmt.Errorf("%w: %w", ErrCorrupt, err)
	}
	if err := o.validate(); err != nil {
		return nil, err
	}
	return &o, nil
}

func (o *Object) validate() error {
	fail := func(format string, args ...any) error {
		return fmt.Errorf("%w: %s", ErrCorrupt, fmt.Sprintf(format, args...))
	}
	switch {
	case o.Version != Version:
		return fail("version %d, this build reads %d", o.Version, Version)
	case o.K != DataShards:
		return fail("%d data shards, want %d", o.K, DataShards)
	case o.M < 1 || o.M > MaxParityShards:
		return fail("%d parity shards, want 1..%d", o.M, MaxParityShards)
	case o.ShardLen == 0 || o.ShardLen > maxShardLen:
		return fail("shard length %d", o.ShardLen)
	case o.PackSize == 0 || (o.PackSize+DataShards-1)/DataShards != uint64(o.ShardLen):
		// shard_len must be exactly ceil(pack_size / 16): anything else
		// means the header disagrees with itself.
		return fail("pack size %d does not fit %d shards of %d bytes", o.PackSize, DataShards, o.ShardLen)
	case len(o.Hashes) != DataShards+int(o.M):
		return fail("%d hashes, want %d", len(o.Hashes), DataShards+int(o.M))
	case len(o.Parity) != int(o.M):
		return fail("%d parity shards, header says %d", len(o.Parity), o.M)
	}
	for i, p := range o.Parity {
		if len(p) != int(o.ShardLen) {
			return fail("parity shard %d is %d bytes, want %d", i, len(p), o.ShardLen)
		}
	}
	return nil
}

// Repair reconstructs a pack from its damaged bytes and this parity.
//
// A shard whose hash does not match is an erasure; a pack shorter or
// longer than recorded is treated as damaged where it differs. The
// result is returned only if it hashes to packID -- nothing in the
// parity object is trusted further than that.
func (o *Object) Repair(packID crypto.ID, damaged []byte) ([]byte, error) {
	m := int(o.M)
	shardLen := int(o.ShardLen)
	shards := shard(padOrTrim(damaged, o.PackSize), shardLen, m)
	for i := range m {
		copy(shards[DataShards+i], o.Parity[i])
	}

	erasures := 0
	for i, s := range shards {
		if crypto.CiphertextID(s) != o.Hashes[i] {
			shards[i] = nil
			erasures++
		}
	}
	if erasures == 0 {
		// Nothing the shards can see is wrong, yet the caller says the
		// pack is damaged: either the parity is for another pack or the
		// hashes are forged. Either way, no repair.
		if crypto.CiphertextID(padOrTrim(damaged, o.PackSize)[:o.PackSize]) == packID {
			return damaged[:o.PackSize], nil
		}
		return nil, fmt.Errorf("%w: every shard matches the parity's hashes, but the pack does not hash to its name; the parity is not for this pack", ErrUnrepairable)
	}
	if erasures > m {
		return nil, fmt.Errorf("%w: %d shards are damaged, parity can rebuild %d", ErrUnrepairable, erasures, m)
	}

	enc, err := reedsolomon.New(DataShards, m)
	if err != nil {
		return nil, fmt.Errorf("repair: %w", err)
	}
	if err := enc.Reconstruct(shards); err != nil {
		return nil, fmt.Errorf("%w: %w", ErrUnrepairable, err)
	}
	var out bytes.Buffer
	out.Grow(int(o.PackSize))                                       //nolint:gosec // bounded by maxShardLen * DataShards
	if err := enc.Join(&out, shards, int(o.PackSize)); err != nil { //nolint:gosec // same bound
		return nil, fmt.Errorf("%w: %w", ErrUnrepairable, err)
	}
	repaired := out.Bytes()
	if got := crypto.CiphertextID(repaired); got != packID {
		return nil, fmt.Errorf("%w: reconstruction hashes to %s, not %s; the parity is wrong or forged", ErrUnrepairable, got, packID)
	}
	return repaired, nil
}

// padOrTrim makes damaged exactly size bytes long, so that a truncated
// or overlong pack shards like the original with the difference showing
// up as damaged shards.
func padOrTrim(damaged []byte, size uint64) []byte {
	if uint64(len(damaged)) == size {
		return damaged
	}
	out := make([]byte, size)
	copy(out, damaged)
	return out
}
