//go:build unix

package repo

import (
	"os"
	"path/filepath"
	"syscall"
	"testing"

	"github.com/at-least/kist/internal/crypto"
	"time"
)

// The client id file is read-mint-write; without the lock two concurrent
// first runs each mint a different id and the last writer wins, leaving
// the loser with a snapshot no later client recognizes. ClientID must
// hold an exclusive flock on <id file>.lock across that sequence (the
// Rust peer's client_id::lock does the same).
func TestClientIDLockSerializesTheMintRace(t *testing.T) {
	var rid crypto.RepoID
	rid[0] = 0xAB
	stateDir := t.TempDir()
	lockPath := filepath.Join(stateDir, "clients", hexRepoID(rid)) + ".lock"
	if err := os.MkdirAll(filepath.Dir(lockPath), 0o700); err != nil {
		t.Fatal(err)
	}
	// Hold the lock the way a concurrent first run would.
	holder, err := os.OpenFile(lockPath, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	defer holder.Close()
	if err := syscall.Flock(int(holder.Fd()), syscall.LOCK_EX); err != nil {
		t.Fatal(err)
	}

	done := make(chan string, 1)
	go func() {
		id, err := ClientID(stateDir, hexRepoID(rid))
		if err != nil {
			done <- "error: " + err.Error()
			return
		}
		done <- id
	}()

	select {
	case id := <-done:
		t.Fatalf("ClientID must block while another process holds the lock (got %q)", id)
	case <-time.After(300 * time.Millisecond):
		// Still blocked: the lock is held, as required.
	}
	if err := syscall.Flock(int(holder.Fd()), syscall.LOCK_UN); err != nil {
		t.Fatal(err)
	}
	select {
	case id := <-done:
		if len(id) != ClientIDSize*2 {
			t.Fatalf("ClientID must complete after release, got %q", id)
		}
		if err := validateClientID(id); err != nil {
			t.Fatalf("minted id invalid: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("ClientID never completed after the lock was released")
	}
}
