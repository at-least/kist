package crypto

import (
	"bytes"
	"encoding/hex"
	"strings"
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

	// Core-deterministic ordering sorts map keys by their encoded bytes:
	// shortest first, then lexicographic. For these keys that is n, v,
	// alpha, zebra -- struct declaration order does not appear anywhere.
	const want = "a4616e2261760165616c7068616161657a65627261617a"
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
// a newer format. Silently dropping it would let a reader act on half a
// document; the version field exists so that this can be a loud failure.
func TestUnmarshalRejectsUnknownFields(t *testing.T) {
	type extended struct {
		Version uint64 `cbor:"v"`
		Zebra   string `cbor:"zebra"`
		Alpha   string `cbor:"alpha"`
		Num     int64  `cbor:"n"`
		Future  string `cbor:"future"`
	}

	encoded, err := Marshal(extended{Version: 2, Future: "surprise"})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got goldenDoc
	err = Unmarshal(encoded, &got)
	if err == nil {
		t.Fatal("unmarshal of an unknown field: got nil error")
	}
	if !strings.Contains(err.Error(), "unknown field") {
		t.Errorf("error = %q, want it to report an unknown field", err)
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
