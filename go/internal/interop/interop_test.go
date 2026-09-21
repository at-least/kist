package interop

// Cross-language conformance tests: the same vectors are consumed by the
// Rust implementation (crates/kist-format/tests/interop.rs and
// crates/kist-chunker/tests/interop.rs). A change that breaks one side's
// expectations breaks the other's repository.

import (
	"bytes"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"strconv"
	"strings"
	"testing"

	"golang.org/x/crypto/argon2"
	"lukechampine.com/blake3"

	"github.com/at-least/kist/internal/chunker"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/tree"
)

// corpusBytes is the shared test input: xorshift64* bytes, generated
// identically by the Rust implementation of the same function. The
// generator IS the corpus; nothing is checked in but the boundaries.
func corpusBytes(seed uint64, n int) []byte {
	out := make([]byte, n)
	state := seed
	for i := range out {
		state ^= state >> 12
		state ^= state << 25
		state ^= state >> 27
		state *= 0x2545F4914F6CDD1D
		out[i] = byte(state >> 33)
	}
	return out
}

type sliceReader struct{ b []byte }

func (r *sliceReader) Read(p []byte) (int, error) {
	if len(r.b) == 0 {
		return 0, io.EOF
	}
	n := copy(p, r.b)
	r.b = r.b[n:]
	return n, nil
}

func TestInteropChunkerBoundaries(t *testing.T) {
	cases := []struct {
		name   string
		params chunker.Params
		length int
	}{
		{"small", chunker.Params{Min: 1 << 10, Avg: 4 << 10, Max: 16 << 10}, 4 << 20},
		{"default", chunker.DefaultParams(), 16 << 20},
	}
	for _, tc := range cases {
		want, err := loadBoundaries("testdata/chunker-boundaries-" + tc.name + ".txt")
		if err != nil {
			t.Fatalf("%s: %v", tc.name, err)
		}
		c, err := chunker.NewParams(&sliceReader{b: corpusBytes(0x5eed1234, tc.length)}, tc.params)
		if err != nil {
			t.Fatal(err)
		}
		var got []int
		for {
			chunk, err := c.Next()
			if errors.Is(err, io.EOF) {
				break
			}
			if err != nil {
				t.Fatal(err)
			}
			got = append(got, len(chunk.Data))
		}
		if len(got) != len(want) {
			t.Fatalf("%s: got %d chunks, want %d", tc.name, len(got), len(want))
		}
		for i := range got {
			if got[i] != want[i] {
				t.Fatalf("%s: chunk %d is %d bytes, want %d", tc.name, i, got[i], want[i])
			}
		}
	}
}

func loadBoundaries(path string) ([]int, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out []int
	for _, line := range strings.Split(strings.TrimSpace(string(raw)), "\n") {
		if line == "" {
			continue
		}
		n, err := strconv.Atoi(line)
		if err != nil {
			return nil, err
		}
		out = append(out, n)
	}
	return out, nil
}

func TestInteropKeyDerivation(t *testing.T) {
	// The vectors are pinned by BOTH implementations; see
	// crates/kist-crypto/tests/poc_keys.rs (kept permanently).
	password := []byte("correct horse battery staple")
	salt := bytes.Repeat([]byte{0x11}, 16)
	master := bytes.Repeat([]byte{0x42}, 32)

	kek := argon2.IDKey(password, salt, 3, 64*1024, 4, 32)
	if got := hex.EncodeToString(kek); got != "daa225443258b3c27130b9872378dd616724c8072613fb9483425b824634bc30" {
		t.Fatalf("KEK mismatch: %s", got)
	}
	for _, tc := range []struct{ ctx, want string }{
		{"kist/v2/hash", "1953dd93ebf5b2e60606cb54e9b5a01debcb64a51c186fcaed7a698e776c8a24"},
		{"kist/v2/chunk", "cd800501750684a7f2de3982090396759351e2ba153a47c0cabd7a307c844d59"},
		{"kist/v2/meta", "b9bb8e689db093d3b7969ebd013efbcf04bb0f49c59d8934484fbda5bff91581"},
		{"kist/v2/index", "7db4ac7f2de7ff9f7ea22282af0bd5963a2a955d773db866210dd3055ecd8d7b"},
	} {
		sub := make([]byte, 32)
		blake3.DeriveKey(sub, tc.ctx, master)
		if got := hex.EncodeToString(sub); got != tc.want {
			t.Fatalf("subkey %s mismatch: %s", tc.ctx, got)
		}
	}
}

func u32p(v uint32) *uint32 { return &v }

func u64p(v uint64) *uint64 { return &v }

func i64p(v int64) *int64 { return &v }

func TestInteropTreeCanonicalCBOR(t *testing.T) {
	// A tree exercising every field kind: bytes name, all optional
	// fields, indirect content, xattrs. Both implementations must
	// produce the identical canonical encoding (the tree ID depends on
	// it).
	id1 := crypto.ID{}
	for i := range id1 {
		id1[i] = byte(i)
	}
	sub := crypto.ID{}
	for i := range sub {
		sub[i] = byte(0xA0 + i)
	}
	// Same tree as the Rust recorder (kist-format/tests/interop.rs): every
	// field family AND every metadata kind, byte-identical canonical CBOR.
	t1 := tree.New([]tree.Entry{
		{
			Name: []byte("a.txt"), Type: 0, MetaKind: 0, Mode: u32p(0o100644),
			UID: u32p(1000), GID: u32p(100), MTimeNs: i64p(1788605504101452995), CTimeNs: i64p(1788605504000000001),
			Size: 4096, Chunks: []crypto.ID{id1}, ContentType: 0,
		},
		{
			Name: []byte("big.bin"), Type: 0, MetaKind: 0, Mode: u32p(0o100600),
			UID: u32p(0), GID: u32p(0), MTimeNs: i64p(1788605500000000000),
			Chunks: []crypto.ID{id1, sub}, ContentType: 1,
			Device: u64p(8), Inode: u64p(999), Links: u64p(3),
			Xattrs: tree.Xattrs{{Name: []byte("user.k"), Value: []byte("v")}},
		},
		{
			Name: []byte("dir"), Type: 1, MetaKind: 0, Mode: u32p(0o040755),
			UID: u32p(0), GID: u32p(0), MTimeNs: i64p(1788605500000000000),
			Subtree: &sub,
		},
		{
			Name: []byte("link"), Type: 2, MetaKind: 0, Mode: u32p(0o120777),
			UID: u32p(0), GID: u32p(0), MTimeNs: i64p(1788605500000000000),
			Target: []byte("../a.txt"),
		},
		{
			Name: []byte("z-s3.bin"), Type: 0, MetaKind: 2, MTimeNs: i64p(1788605500000000000),
			Size: 777, Chunks: []crypto.ID{sub},
			Etag: []byte(`"5e6f80a1c9de4c2b95e6f81a03cf8f4d"`),
			Vern: []byte("3sL4k1JtX9qZ7wR2.noS3vId8UuM5pQ0"),
		},
	})
	var keys crypto.Key
	_, encoded, err := t1.Encode(&keys)
	if err != nil {
		t.Fatal(err)
	}
	want, err := os.ReadFile("testdata/tree-canonical.hex")
	if err != nil {
		// First run: record. The Rust side must produce the same bytes.
		if err := os.WriteFile("testdata/tree-canonical.hex", []byte(hex.EncodeToString(encoded)+"\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		t.Skipf("recorded tree-canonical.hex (%d bytes)", len(encoded))
	}
	if got := hex.EncodeToString(encoded); got != strings.TrimSpace(string(want)) {
		t.Fatalf("tree encoding diverged from the recorded vector (got %d bytes, want %d)", len(encoded), len(want)/2)
	}
}

// The two format specs must stay byte-identical (README: the local
// interop gate covers "conformance vectors 與兩份 format.md 逐 byte
// 一致"). ci-rust.yml enforces it with cmp, but that workflow is
// manual-dispatch-only; this test is the local enforcement the README
// promises, so a spec edit that skips the Go copy fails `go test
// ./internal/interop/...` on the spot.
func TestFormatSpecCopiesAreByteIdentical(t *testing.T) {
	authoritative, err := os.ReadFile("../../../docs/format.md")
	if err != nil {
		t.Fatalf("read docs/format.md: %v", err)
	}
	goCopy, err := os.ReadFile("../../docs/format.md")
	if err != nil {
		t.Fatalf("read go/docs/format.md: %v", err)
	}
	if !bytes.Equal(authoritative, goCopy) {
		t.Fatal("go/docs/format.md differs from the authoritative docs/format.md; update both or run: cp docs/format.md go/docs/format.md")
	}
}
