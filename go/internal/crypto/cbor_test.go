package crypto

import (
	"bytes"
	"encoding/hex"
	"testing"
)

type goldenDoc struct {
	Version uint64 `cbor:"v"`
	Zebra   string `cbor:"zebra"`
	Alpha   string `cbor:"alpha"`
	Num     int64  `cbor:"n"`
}

// Content addressing rests on this: one value, one encoding, whatever
// order the struct happens to declare its fields in.
func TestMarshalIsCanonical(t *testing.T) {
	doc := goldenDoc{Version: 1, Zebra: "z", Alpha: "a", Num: -3}

	first, err := Marshal(doc)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	for range 16 {
		again, err := Marshal(doc)
		if err != nil {
			t.Fatalf("marshal: %v", err)
		}
		if !bytes.Equal(first, again) {
			t.Fatal("two encodings of one value differ")
		}
	}

	// v2 canonical rule: struct fields are emitted in the SPEC-PINNED
	// order, which is the declaration order of these structs (docs/
	// format.md §4). Determinism comes from the field tables, not from
	// sorting -- the sorted form (n, v, alpha, zebra) is exactly what we
	// must NOT emit anymore.
	const want = "a4617601657a65627261617a65616c7068616161616e22"
	if got := hex.EncodeToString(first); got != want {
		t.Fatalf("encoding = %s, want %s", got, want)
	}
}

func TestUnmarshalRoundTrip(t *testing.T) {
	doc := goldenDoc{Version: 1, Zebra: "z", Alpha: "a", Num: -3}

	encoded, err := Marshal(doc)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got goldenDoc
	if err := Unmarshal(encoded, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if got != doc {
		t.Errorf("round trip = %+v, want %+v", got, doc)
	}
}

// A field this build does not know about means the object was written by
// a newer format within the same major version. v2's forward-compatibility
// rule is that unknown FIELDS are ignored (docs/format.md §4): readers
// never re-encode what they decoded, so a dropped field cannot smuggle two
// readings of one document past anyone. The version field is what makes a
// newer MAJOR format a loud failure, and each loader checks it.
func TestUnmarshalIgnoresUnknownFields(t *testing.T) {
	type extended struct {
		Version uint64 `cbor:"v"`
		Zebra   string `cbor:"zebra"`
		Alpha   string `cbor:"alpha"`
		Num     int64  `cbor:"n"`
		Future  string `cbor:"future"`
	}

	encoded, err := Marshal(extended{Version: 2, Zebra: "z", Alpha: "a", Num: -3, Future: "surprise"})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got goldenDoc
	if err := Unmarshal(encoded, &got); err != nil {
		t.Fatalf("unmarshal of a document with an unknown field: %v", err)
	}
	if got.Version != 2 || got.Zebra != "z" || got.Alpha != "a" || got.Num != -3 {
		t.Errorf("decoded = %+v, want the known fields populated and nothing else", got)
	}
}

func TestUnmarshalRejectsDuplicateMapKeys(t *testing.T) {
	// {"v": 1, "v": 2} -- hand-built, because the encoder will not emit it.
	dup := []byte{0xa2, 0x61, 'v', 0x01, 0x61, 'v', 0x02}

	var got map[string]uint64
	if err := Unmarshal(dup, &got); err == nil {
		t.Fatalf("unmarshal of a duplicate key: got nil error, decoded %v", got)
	}
}

func TestUnmarshalRejectsIndefiniteLength(t *testing.T) {
	// Indefinite-length text string "ab", broken into two chunks.
	indef := []byte{0x7f, 0x61, 'a', 0x61, 'b', 0xff}

	var got string
	if err := Unmarshal(indef, &got); err == nil {
		t.Fatalf("unmarshal of an indefinite-length string: got nil error, decoded %q", got)
	}
}

func TestUnmarshalRejectsTrailingGarbage(t *testing.T) {
	encoded, err := Marshal(goldenDoc{Version: 1})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got goldenDoc
	if err := Unmarshal(append(encoded, 0x00), &got); err == nil {
		t.Fatal("unmarshal with trailing bytes: got nil error")
	}
}

// format.md §4 rule 4: CBOR tags are forbidden -- a tagged encoding and
// its untagged equivalent are two readings of one object, exactly what
// the canonical-form rules exist to rule out. The decoder must reject
// major type 6 rather than silently unwrap it.
func TestDecodeRejectsTags(t *testing.T) {
	// tag(1) wrapped around the integer 5, then trailing text is
	// irrelevant: Unmarshal into an int must fail on the tag alone.
	var n int
	if err := Unmarshal([]byte{0xc1, 0x05}, &n); err == nil {
		t.Fatal("a tagged integer decoded without error")
	}
	var s struct {
		A uint64 `cbor:"a"`
	}
	// map(1){ "a": tag(1)(5) } -- the tag hides inside a field.
	if err := Unmarshal([]byte{0xa1, 0x61, 'a', 0xc1, 0x05}, &s); err == nil {
		t.Fatal("a tagged field value decoded without error")
	}
}
