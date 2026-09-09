package crypto

import (
	"fmt"

	"github.com/fxamacker/cbor/v2"
)

// Every metadata object in a repository is CBOR, and every one of them
// carries a version field so the format can evolve without guessing.
//
// Encoding follows the v2 canonical rules (docs/format.md §4): struct
// fields are emitted in each struct's SPEC-PINNED ORDER (declaration
// order in these packages -- the field tables in the spec match them
// field for field), integers in shortest form (CBOR's default), and the
// one real map type (tree xattrs) is sorted by its custom marshaler.
// The earlier Core-Deterministic sorted-keys rule was dropped: it made
// the shipping Rust implementation 20x slower on encode for bytes
// nothing depends on. Determinism is not cosmetic -- a tree is named by
// ContentID over its encoding -- but it comes from the spec's field
// order, not from sorting.
//
// NEVER hand a Go map to Marshal: plain EncMode does not sort map keys.
// The metadata structs contain none; keep it that way.
var (
	encMode cbor.EncMode
	decMode cbor.DecMode
)

func init() {
	var err error
	if encMode, err = new(cbor.EncOptions).EncMode(); err != nil {
		panic(fmt.Sprintf("kist/crypto: build canonical CBOR encoder: %v", err))
	}

	// Decoding is strict on the things that let an attacker smuggle two
	// readings of one document past each other, and bounded on the things
	// that let a small object allocate a large amount of memory. Unknown
	// FIELDS are ignored: that is the forward-compatibility rule of the
	// v2 format (docs/format.md §4), and it is safe because no reader
	// ever re-encodes what it decoded ("never round-trip").
	opts := cbor.DecOptions{
		DupMapKey:        cbor.DupMapKeyEnforcedAPF,
		IndefLength:      cbor.IndefLengthForbidden,
		MaxArrayElements: 8 << 20,
		MaxMapPairs:      1 << 20,
	}
	if decMode, err = opts.DecMode(); err != nil {
		panic(fmt.Sprintf("kist/crypto: build CBOR decoder: %v", err))
	}
}

// Marshal encodes v as canonical CBOR.
func Marshal(v any) ([]byte, error) {
	b, err := encMode.Marshal(v)
	if err != nil {
		return nil, fmt.Errorf("encode cbor: %w", err)
	}
	return b, nil
}

// Unmarshal decodes canonical CBOR into v, rejecting duplicate map keys
// and indefinite-length items; unknown fields are ignored.
func Unmarshal(data []byte, v any) error {
	if err := decMode.Unmarshal(data, v); err != nil {
		return fmt.Errorf("decode cbor: %w", err)
	}
	return nil
}
