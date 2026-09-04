package crypto

import (
	"fmt"

	"github.com/fxamacker/cbor/v2"
)

// Every metadata object in a repository is CBOR, and every one of them
// carries a version field so the format can evolve without guessing.
//
// Encoding is Core Deterministic (RFC 8949 §4.2.1): shortest-form
// arguments, map keys sorted bytewise. Determinism is not cosmetic here.
// A tree is named by ContentID over its encoding, so two encoders that
// disagree by one byte would produce two names for one directory and
// silently defeat deduplication.
var (
	encMode cbor.EncMode
	decMode cbor.DecMode
)

func init() {
	var err error
	if encMode, err = cbor.CoreDetEncOptions().EncMode(); err != nil {
		panic(fmt.Sprintf("kist/crypto: build canonical CBOR encoder: %v", err))
	}

	// Decoding is strict on the things that let an attacker smuggle two
	// readings of one document past each other, and bounded on the things
	// that let a small object allocate a large amount of memory.
	opts := cbor.DecOptions{
		DupMapKey:         cbor.DupMapKeyEnforcedAPF,
		IndefLength:       cbor.IndefLengthForbidden,
		ExtraReturnErrors: cbor.ExtraDecErrorUnknownField,
		MaxArrayElements:  8 << 20,
		MaxMapPairs:       1 << 20,
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

// Unmarshal decodes canonical CBOR into v, rejecting duplicate map keys,
// indefinite-length items and unknown fields.
func Unmarshal(data []byte, v any) error {
	if err := decMode.Unmarshal(data, v); err != nil {
		return fmt.Errorf("decode cbor: %w", err)
	}
	return nil
}
