package repo

// v2 的間接清單：tree entry 的 Chunks 指向 encoded ChunkList 的資料塊。
// prune 的活躍走訪必須解開清單、把**資料** chunk 所在的 pack 也算活——
// 只看 entry.Chunks 會把大檔的全部資料當垃圾刪掉（reviewer 發現的
// 資料遺失路徑；docs/format.md §13.1）。

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"slices"
	"testing"
	"time"

	"github.com/at-least/kist/internal/chunker"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/pack"
	"github.com/at-least/kist/internal/snapshot"
	"github.com/at-least/kist/internal/tree"
)

func TestPruneKeepsDataOfIndirectChunkLists(t *testing.T) {
	ctx := context.Background()
	r, _ := initRepo(t, "indirect-prune")

	payload := randomBytes(t, "indirect-prune-payload", 3<<20)
	var ids []crypto.ID
	var put [][]byte
	c, err := chunker.New(bytes.NewReader(payload))
	if err != nil {
		t.Fatal(err)
	}
	for {
		chunk, err := c.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		id := crypto.ContentID(&r.keys.Hash, chunk.Data)
		ids = append(ids, id)
		put = append(put, append([]byte(nil), chunk.Data...))
	}

	store := func(chunks map[crypto.ID][]byte) (crypto.ID, []pack.Entry) {
		w, err := pack.NewWriter(r.keys, t.TempDir(), crypto.DeterministicReader("indirect-prune"))
		if err != nil {
			t.Fatal(err)
		}
		for id, data := range chunks {
			if err := w.Add(id, data); err != nil {
				t.Fatal(err)
			}
		}
		packID, entries, _, err := w.Finish(ctx, r.Backend())
		if err != nil {
			t.Fatal(err)
		}
		r.index.AddPack(packID, entries)
		return packID, entries
	}

	contentChunks := make(map[crypto.ID][]byte, len(ids))
	for i, id := range ids {
		contentChunks[id] = put[i]
	}
	dataPack, _ := store(contentChunks)

	encoded, err := crypto.Marshal(tree.NewChunkList(ids))
	if err != nil {
		t.Fatal(err)
	}
	listID := crypto.ContentID(&r.keys.Hash, encoded)
	listPack, listEntries := store(map[crypto.ID][]byte{listID: encoded})

	dirTree := tree.New([]tree.Entry{{
		Name: []byte("big.bin"), Type: uint8(tree.TypeFile), Mode: 0o644,
		Size: uint64(len(payload)), MTimeNs: 1767225845000000000,
		Chunks: listEntriesIDs(listEntries), ContentType: uint8(tree.ContentIndirect),
	}})
	dirID, err := dirTree.Save(ctx, r.Backend(), r.keys, crypto.DeterministicReader("indirect-prune-dir"))
	if err != nil {
		t.Fatal(err)
	}
	root := tree.New([]tree.Entry{{
		Name: []byte("/virtual"), Type: uint8(tree.TypeDir), Mode: 0o755 | uint32(os.ModeDir),
		MTimeNs: 1767225845000000000, Subtree: &dirID,
	}})
	rootID, err := root.Save(ctx, r.Backend(), r.keys, crypto.DeterministicReader("indirect-prune-root"))
	if err != nil {
		t.Fatal(err)
	}
	// 真實 backup 會寫 index blob；prune 從 blobs 載入 index。
	if _, err := r.RebuildIndex(ctx); err != nil {
		t.Fatalf("rebuild index: %v", err)
	}

	snap := &snapshot.Snapshot{
		Version: snapshot.Version, Root: rootID, TimeNs: 1767225845000000001,
		Host: "t", Paths: [][]byte{[]byte("/virtual")}, ClientID: r.clientID,
	}
	_, err = snap.Save(ctx, r.Backend(), r.keys, crypto.DeterministicReader("indirect-prune-snap"))
	if err != nil {
		t.Fatal(err)
	}

	// 沒有 chunk list 之外的引用：資料 pack 只被間接清單「解開之後」引用。
	// 修復前：liveness 只看 entry.Chunks（= 清單塊），資料 pack 被標記、
	// grace 後刪除 → snapshot 引用的資料消失。
	report, err := r.Prune(ctx, PruneOptions{Grace: time.Nanosecond, Progressf: func(string, ...any) {}})
	if err != nil {
		t.Fatalf("prune: %v", err)
	}
	if len(report.Marked) != 0 {
		t.Errorf("prune marked %d objects; the data packs of an indirect chunk list are live: %v", len(report.Marked), report.Marked)
	}
	for _, id := range ids {
		if _, ok := r.index.Lookup(id); !ok {
			t.Errorf("data chunk %s is not in the index", id)
		}
	}
	for name, want := range map[string]crypto.ID{"data": dataPack, "chunk-list": listPack} {
		if _, err := r.Backend().Stat(ctx, pack.Key(want)); err != nil {
			t.Errorf("%s pack: %v", name, err)
		}
	}
}

func listEntriesIDs(entries []pack.Entry) []crypto.ID {
	out := make([]crypto.ID, 0, len(entries))
	for _, e := range entries {
		out = append(out, e.ID)
	}
	slices.SortFunc(out, func(a, b crypto.ID) int { return bytes.Compare(a[:], b[:]) })
	return out
}
