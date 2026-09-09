//go:build !race

package backend

import (
	"bytes"
	"context"
	"crypto/rand"
	"fmt"
	"io"
	"testing"
	"time"
)

// TestSFTPThroughput reports what one 64 MiB pack costs to put and to
// get over SFTP to the local container. A number, not a bar: PLAN sets
// none, and the network here is loopback.
func TestSFTPThroughput(t *testing.T) {
	ctx := context.Background()
	b := newTestSFTP(t)
	const size = 64 << 20
	payload := make([]byte, size)
	if _, err := rand.Read(payload); err != nil {
		t.Fatal(err)
	}

	start := time.Now()
	if err := b.PutIfAbsent(ctx, "packs/big", bytes.NewReader(payload), size); err != nil {
		t.Fatal(err)
	}
	put := time.Since(start)

	start = time.Now()
	rc, err := b.Get(ctx, "packs/big", 0, ReadToEnd)
	if err != nil {
		t.Fatal(err)
	}
	n, err := io.Copy(io.Discard, rc)
	_ = rc.Close()
	if err != nil || n != size {
		t.Fatalf("get: %d bytes, %v", n, err)
	}
	get := time.Since(start)

	start = time.Now()
	rc, err = b.Get(ctx, "packs/big", size-4096, 4096)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.Copy(io.Discard, rc); err != nil {
		t.Fatal(err)
	}
	_ = rc.Close()
	tail := time.Since(start)

	mbps := func(d time.Duration) string { return fmt.Sprintf("%.0f MiB/s", float64(size>>20)/d.Seconds()) }
	t.Logf("put 64 MiB: %v (%s); get 64 MiB: %v (%s); ranged 4 KiB tail: %v", put.Round(time.Millisecond), mbps(put), get.Round(time.Millisecond), mbps(get), tail.Round(time.Millisecond))
}
