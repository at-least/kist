package parity

import (
	"bytes"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/crypto"
)

// A pack-shaped payload: pseudo-random bytes, as ciphertext is.
func fakePack(seed string, n int) []byte {
	out := make([]byte, n)
	if _, err := io.ReadFull(crypto.DeterministicReader(seed), out); err != nil {
		panic(err)
	}
	return out
}

func encoded(t *testing.T, pack []byte, m int) (crypto.ID, *Object, []byte) {
	t.Helper()
	id := crypto.CiphertextID(pack)
	raw, err := Encode(id, pack, m)
	if err != nil {
		t.Fatal(err)
	}
	obj, err := Parse(raw)
	if err != nil {
		t.Fatal(err)
	}
	return id, obj, raw
}

func TestEncodeIsDeterministicAndParses(t *testing.T) {
	pack := fakePack("pack", 1000)
	id, obj, raw := encoded(t, pack, 2)
	raw2, err := Encode(id, pack, 2)
	if err != nil || !bytes.Equal(raw, raw2) {
		t.Fatal("encoding is not deterministic")
	}
	if obj.M != 2 || obj.K != DataShards || obj.PackSize != 1000 || obj.ShardLen != 63 || len(obj.Hashes) != 18 {
		t.Errorf("object = %+v", obj)
	}
	if _, err := Encode(crypto.ID{1}, pack, 2); err == nil {
		t.Error("encode accepted a pack that does not hash to the given ID")
	}
	for _, m := range []int{0, 9} {
		if _, err := Encode(id, pack, m); err == nil {
			t.Errorf("encode accepted m=%d", m)
		}
	}
}

// Sizes around the shard boundary. 178 bytes was the one that broke: a
// lower bound written as shard_len*15 instead of ceil(size/16) rejected
// a valid object, and check --repair reported "parity object is corrupt".
func TestEncodeAndParseAtAwkwardSizes(t *testing.T) {
	for _, n := range []int{1, 15, 16, 17, 178, 179, 180, 191, 192, 193, 255, 256, 4095, 4097} {
		pack := fakePack(fmt.Sprint("size", n), n)
		id := crypto.CiphertextID(pack)
		raw, err := Encode(id, pack, 2)
		if err != nil {
			t.Fatalf("%d bytes: encode: %v", n, err)
		}
		obj, err := Parse(raw)
		if err != nil {
			t.Fatalf("%d bytes: parse: %v", n, err)
		}
		bad := bytes.Clone(pack)
		bad[n/2] ^= 1
		if got, err := obj.Repair(id, bad); err != nil || !bytes.Equal(got, pack) {
			t.Fatalf("%d bytes: repair: %v", n, err)
		}
	}
}

func TestGoldenParity(t *testing.T) {
	pack := fakePack("golden", 4321)
	id, _, raw := encoded(t, pack, 2)
	got := fmt.Sprintf("pack %s\nparity %s\n", id, hex.EncodeToString(raw))
	const path = "testdata/parity.txt"
	want, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		if err := os.WriteFile(path, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
		t.Logf("wrote %s", path)
		return
	}
	if err != nil {
		t.Fatal(err)
	}
	if string(want) != got {
		t.Fatalf("parity object changed; if that was intended, delete %s and re-run", path)
	}
}

// damage flips one byte at each offset.
func damage(pack []byte, offsets ...int) []byte {
	out := bytes.Clone(pack)
	for _, o := range offsets {
		out[o] ^= 0x5a
	}
	return out
}

func TestRepairsUpToMShards(t *testing.T) {
	pack := fakePack("repair", 100_000)
	id, obj, _ := encoded(t, pack, 2)
	shardLen := int(obj.ShardLen)

	cases := map[string][]int{
		"one byte in the body":                  {shardLen * 3},
		"two flips in one shard":                {shardLen*5 + 1, shardLen*5 + 40},
		"two shards, one in the last (trailer)": {shardLen * 2, len(pack) - 3},
		"the very first and last bytes":         {0, len(pack) - 1},
	}
	for name, offsets := range cases {
		t.Run(name, func(t *testing.T) {
			repaired, err := obj.Repair(id, damage(pack, offsets...))
			if err != nil {
				t.Fatalf("repair: %v", err)
			}
			if !bytes.Equal(repaired, pack) {
				t.Fatal("repaired pack differs from the original")
			}
		})
	}

	t.Run("truncated pack", func(t *testing.T) {
		repaired, err := obj.Repair(id, pack[:len(pack)-shardLen/2])
		if err != nil || !bytes.Equal(repaired, pack) {
			t.Fatalf("repair of a truncated pack: %v", err)
		}
	})
	t.Run("overlong pack", func(t *testing.T) {
		repaired, err := obj.Repair(id, append(bytes.Clone(pack), 1, 2, 3))
		if err != nil || !bytes.Equal(repaired, pack) {
			t.Fatalf("repair of an overlong pack: %v", err)
		}
	})
	t.Run("undamaged pack", func(t *testing.T) {
		repaired, err := obj.Repair(id, pack)
		if err != nil || !bytes.Equal(repaired, pack) {
			t.Fatalf("repair of an intact pack: %v", err)
		}
	})
}

func TestRefusesMoreThanMErasures(t *testing.T) {
	pack := fakePack("toomuch", 50_000)
	id, obj, _ := encoded(t, pack, 2)
	shardLen := int(obj.ShardLen)
	_, err := obj.Repair(id, damage(pack, 0, shardLen, 2*shardLen))
	if !errors.Is(err, ErrUnrepairable) {
		t.Fatalf("three damaged shards with m=2: err = %v, want ErrUnrepairable", err)
	}
}

// A parity object that is valid but not for this pack -- forged, or a
// mix-up -- cannot produce a "repair" that passes.
func TestForgedParityCannotRepairWrongly(t *testing.T) {
	pack := fakePack("real", 20_000)
	id := crypto.CiphertextID(pack)
	other := fakePack("other", 20_000)
	_, forged, _ := encoded(t, other, 2)

	if _, err := forged.Repair(id, damage(pack, 5)); !errors.Is(err, ErrUnrepairable) {
		t.Fatalf("forged parity repaired a pack: err = %v", err)
	}
	// Forged hashes that happen to match the damaged shards: every
	// shard "verifies", and the name check is the only thing left.
	_, obj, _ := encoded(t, pack, 2)
	bad := damage(pack, 7)
	lying := *obj
	lying.Hashes = make([]crypto.ID, len(obj.Hashes))
	copy(lying.Hashes, obj.Hashes)
	shards := shard(bad, int(obj.ShardLen), 2)
	lying.Hashes[0] = crypto.CiphertextID(shards[0])
	if _, err := lying.Repair(id, bad); !errors.Is(err, ErrUnrepairable) {
		t.Fatalf("parity with hashes matching the damage repaired nothing yet returned: %v", err)
	}
}

func TestParseRejectsInconsistentHeaders(t *testing.T) {
	pack := fakePack("hdr", 3000)
	_, obj, _ := encoded(t, pack, 1)
	mutate := func(f func(o *Object)) []byte {
		o := *obj
		o.Hashes = append([]crypto.ID(nil), obj.Hashes...)
		o.Parity = append([][]byte(nil), obj.Parity...)
		f(&o)
		raw, err := crypto.Marshal(o)
		if err != nil {
			t.Fatal(err)
		}
		return raw
	}
	cases := map[string][]byte{
		"version":          mutate(func(o *Object) { o.Version = 3 }),
		"k":                mutate(func(o *Object) { o.K = 8 }),
		"m zero":           mutate(func(o *Object) { o.M = 0 }),
		"m too big":        mutate(func(o *Object) { o.M = 9 }),
		"shard len zero":   mutate(func(o *Object) { o.ShardLen = 0 }),
		"shard len huge":   mutate(func(o *Object) { o.ShardLen = 1 << 30 }),
		"pack size small":  mutate(func(o *Object) { o.PackSize = 1 }),
		"pack size large":  mutate(func(o *Object) { o.PackSize = 1 << 40 }),
		"hash count":       mutate(func(o *Object) { o.Hashes = o.Hashes[:3] }),
		"parity count":     mutate(func(o *Object) { o.Parity = nil }),
		"parity shard len": mutate(func(o *Object) { o.Parity[0] = o.Parity[0][:5] }),
	}
	cases["garbage"] = []byte("not cbor")
	cases["unknown field"] = []byte{0xa1, 0x61, 0x7a, 0x01}
	for name, raw := range cases {
		if _, err := Parse(raw); !errors.Is(err, ErrCorrupt) {
			t.Errorf("%s: err = %v, want ErrCorrupt", name, err)
		}
	}
	if !strings.HasPrefix(Key(crypto.ID{}), Prefix) {
		t.Error("key prefix")
	}
}

// FuzzParse: arbitrary bytes into the parity decoder, no panics, and
// nothing decoded is allowed to ask for shards its header does not
// justify.
func FuzzParse(f *testing.F) {
	pack := fakePack("fuzz", 777)
	raw, err := Encode(crypto.CiphertextID(pack), pack, 2)
	if err != nil {
		f.Fatal(err)
	}
	f.Add(raw)
	f.Add([]byte{0xa0})
	f.Add([]byte{})
	f.Fuzz(func(t *testing.T, data []byte) {
		obj, err := Parse(data)
		if err != nil {
			return
		}
		if uint64(len(obj.Parity))*uint64(obj.ShardLen) > MaxParityShards*maxShardLen {
			t.Fatalf("parsed object holds %d parity bytes", len(obj.Parity)*int(obj.ShardLen))
		}
		_, _ = obj.Repair(crypto.ID{}, pack) //nolint:errcheck // must not panic; an error is the expected answer
	})
}
