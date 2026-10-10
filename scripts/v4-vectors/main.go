// 獨立計算 kist v4（docs/format-v4.md）§3 的金鑰推導與槽、§5 的密封向量，給
// crates/kist-crypto/tests/v4_keys.rs 與 crates/kist-format/tests/golden-v4/ 當預期值。
// 刻意不用 Rust：lukechampine.com/blake3、x/crypto 的 argon2 與 chacha20poly1305，config 的 CBOR
// 以手寫的最短編碼產生。執行：go run .（離線時 GOPROXY=off GOSUMDB=off go run .，用本機的模組快取）。
package main

import (
	"encoding/hex"
	"fmt"
	"sort"

	"golang.org/x/crypto/argon2"
	"golang.org/x/crypto/chacha20poly1305"
	"lukechampine.com/blake3"
)

// CBOR 的 head：major type 與最短長度。
func head(major byte, n uint64) []byte {
	m := major << 5
	switch {
	case n < 24:
		return []byte{m | byte(n)}
	case n < 1<<8:
		return []byte{m | 24, byte(n)}
	case n < 1<<16:
		return []byte{m | 25, byte(n >> 8), byte(n)}
	case n < 1<<32:
		return []byte{m | 26, byte(n >> 24), byte(n >> 16), byte(n >> 8), byte(n)}
	default:
		b := []byte{m | 27}
		for i := 7; i >= 0; i-- {
			b = append(b, byte(n>>(8*i)))
		}
		return b
	}
}

func text(s string) []byte  { return append(head(3, uint64(len(s))), s...) }
func bytes(b []byte) []byte { return append(head(2, uint64(len(b))), b...) }
func uint(n uint64) []byte  { return head(0, n) }
func cat(parts ...[]byte) []byte {
	var out []byte
	for _, p := range parts {
		out = append(out, p...)
	}
	return out
}

func rep(b byte, n int) []byte {
	out := make([]byte, n)
	for i := range out {
		out[i] = b
	}
	return out
}

func derive(ctx string, key []byte) []byte {
	out := make([]byte, 32)
	blake3.DeriveKey(out, ctx, key)
	return out
}

func keyed(key, msg []byte) []byte {
	h := blake3.New(32, key)
	h.Write(msg)
	return h.Sum(nil)
}

func main() {
	password := []byte("correct horse battery staple")
	salt := rep(0x11, 16)
	master := rep(0x42, 32)
	nonce := rep(0x77, 24)

	// Config { v:4, repo_id:[0xab;16], created:1788523200000000000, chunker{min,avg,max}, kdf{alg,t,m,p,salt} }
	config := cat(head(5, 5),
		text("v"), uint(4),
		text("repo_id"), bytes(rep(0xab, 16)),
		text("created"), uint(1788523200000000000),
		text("chunker"), head(5, 3), text("min"), uint(512*1024), text("avg"), uint(2*1024*1024), text("max"), uint(8*1024*1024),
		text("kdf"), head(5, 5), text("alg"), text("argon2id"), text("t"), uint(3), text("m"), uint(64*1024), text("p"), uint(4), text("salt"), bytes(salt),
	)
	fmt.Println("CONFIG_CBOR", hex.EncodeToString(config))

	kek := argon2.IDKey(password, salt, 3, 64*1024, 4, 32)
	fmt.Println("KEK", hex.EncodeToString(kek))
	name := derive("kist/v4/slot-name", kek)[:16]
	fmt.Println("SLOT_NAME", hex.EncodeToString(name))
	slotKey := derive("kist/v4/slot-key", kek)
	fmt.Println("SLOT_KEY", hex.EncodeToString(slotKey))
	cfgHash := blake3.Sum256(config)
	aad := append([]byte("kist/v4/slot"), cfgHash[:]...)
	fmt.Println("SLOT_AAD", hex.EncodeToString(aad))
	aead, err := chacha20poly1305.NewX(slotKey)
	if err != nil {
		panic(err)
	}
	wrapped := append(append([]byte{}, nonce...), aead.Seal(nil, nonce, master, aad)...)
	fmt.Println("SLOT_WRAPPED", hex.EncodeToString(wrapped))
	slot := cat(head(5, 2), text("v"), uint(4), text("wrapped"), bytes(wrapped))
	fmt.Println("SLOT_CBOR", hex.EncodeToString(slot))

	subs := map[string][]byte{}
	for _, ctx := range []string{"chunk-id", "meta-id", "cut", "data", "meta", "snapshot", "index", "cache"} {
		k := derive("kist/v4/"+ctx, master)
		subs[ctx] = k
		fmt.Println("SUB", ctx, hex.EncodeToString(k))
	}
	chunkID := keyed(subs["chunk-id"], []byte("hello"))
	fmt.Println("CHUNK_ID_hello", hex.EncodeToString(chunkID))
	metaID := keyed(subs["meta-id"], []byte{0xa0})
	seal := func(label string, key, aad, plain []byte) {
		a, err := chacha20poly1305.NewX(key)
		if err != nil {
			panic(err)
		}
		out := append(append([]byte{}, nonce...), a.Seal(nil, nonce, plain, aad)...)
		fmt.Println("SEALED", label, hex.EncodeToString(out))
	}
	client := rep(0x1f, 16)
	seal("chunk", subs["data"], chunkID, append([]byte{0}, []byte("hello")...))
	seal("meta", subs["meta"], metaID, []byte{0, 0xa0})
	seal("trailer", subs["index"], []byte("kist/v4/pack-trailer"), []byte("trailer"))
	seal("snapshot", subs["snapshot"], []byte("snapshots/"+hex.EncodeToString(client)+"/20260904T120000123456789Z"), []byte("snap"))
	seal("delta", subs["index"], append([]byte("kist/v4/delta"), client...), []byte("delta"))
	seal("head", subs["index"], []byte("maint/gen/0000000000000003"), []byte("head"))
	seal("shard", subs["index"], []byte("kist/v4/shard"), []byte("shard"))
	fmt.Println("META_ID_a0", hex.EncodeToString(keyed(subs["meta-id"], []byte{0xa0})))
	// 本地快取的目錄名稱：keyed BLAKE3(K_cache, repo 位置) 的前 16 bytes。換位置就換目錄（同 v3，ADR 004）。
	for _, loc := range []string{"s3://bucket/prefix", "/srv/repo"} {
		fmt.Println("CACHE_DIR", loc, hex.EncodeToString(keyed(subs["cache"], []byte(loc))[:16]))
	}
	// §7 的切段規則：一段在加入某一項之前已有至少 1024 項、而且那一項的名稱讓切段條件成立，就在它之後切。
	isCut := func(name []byte) bool {
		h := keyed(subs["cut"], name)
		return (uint16(h[0])|uint16(h[1])<<8)&0x0fff == 0
	}
	// 目錄：名稱 n0..n19999 依 bytes 排序。
	var names []string
	for i := 0; i < 20000; i++ {
		names = append(names, fmt.Sprintf("n%d", i))
	}
	sort.Strings(names)
	count, first := 0, ""
	for _, n := range names {
		if count == 0 {
			first = n
		}
		cut := count >= 1024 && isCut([]byte(n))
		count++
		if cut {
			fmt.Println("DIR_SEG", first, count)
			count = 0
		}
	}
	if count > 0 {
		fmt.Println("DIR_SEG", first, count)
	}
	// chunk 清單：ID = BLAKE3(i 的 u32 little-endian)，明文長度 1000+i，名稱是 ID 的 32 bytes。
	count, sum := 0, uint64(0)
	for i := 0; i < 20000; i++ {
		id := blake3.Sum256([]byte{byte(i), byte(i >> 8), byte(i >> 16), byte(i >> 24)})
		cut := count >= 1024 && isCut(id[:])
		count++
		sum += uint64(1000 + i)
		if cut {
			fmt.Println("CHUNK_SEG", count, sum)
			count, sum = 0, 0
		}
	}
	if count > 0 {
		fmt.Println("CHUNK_SEG", count, sum)
	}
	// 切段：前兩個 byte little-endian 的低 12 位元為 0 的名稱，從 "n0" 起找前三個。
	found := 0
	for i := 0; found < 3; i++ {
		n := []byte(fmt.Sprintf("n%d", i))
		h := keyed(subs["cut"], n)
		if (uint16(h[0])|uint16(h[1])<<8)&0x0fff == 0 {
			fmt.Println("CUT_NAME", string(n))
			found++
		}
	}
}
