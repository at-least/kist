package source

import (
	"context"
	"net"
	"os"
	"path/filepath"
	"testing"

	"github.com/pkg/sftp"
)

// inProcessSFTP serves dir over a net.Pipe with pkg/sftp's own server:
// enough protocol for the source's ReadDir, no ssh, no container.
func inProcessSFTP(t *testing.T, dir string) *sftp.Client {
	t.Helper()
	serverConn, clientConn := net.Pipe()
	srv, err := sftp.NewServer(serverConn, sftp.WithServerWorkingDirectory(dir))
	if err != nil {
		t.Fatal(err)
	}
	go func() { _ = srv.Serve() }() //nolint:errcheck // best-effort server loop for the test
	t.Cleanup(func() {
		_ = clientConn.Close() //nolint:errcheck // teardown
		_ = srv.Close() //nolint:errcheck // teardown
	})
	client, err := sftp.NewClientPipe(clientConn, clientConn)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = client.Close() }) //nolint:errcheck // teardown
	return client
}

// A source lists THE USER'S data: dotfiles are content, not noise. The
// blanket dot-skip this listing inherited from the repository backend's
// namespace silently dropped .bashrc and friends from every sftp-source
// backup -- the local and s3 sources include them, and a backup that
// skips something must say so.
func TestSFTPSourceListsDotfiles(t *testing.T) {
	dir := t.TempDir()
	for _, f := range []string{".bashrc", ".profile", "readme.txt"} {
		if err := os.WriteFile(filepath.Join(dir, f), []byte(f), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.MkdirAll(filepath.Join(dir, ".config"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, ".config", "settings"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}

	src := &SFTPSource{root: dir, client: inProcessSFTP(t, dir)}
	items, err := src.List(context.Background(), nil)
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	got := map[string]bool{}
	for _, it := range items {
		got[string(it.Name)] = true
	}
	for _, want := range []string{".bashrc", ".profile", ".config", "readme.txt"} {
		if !got[want] {
			t.Errorf("listing is missing %q (got %v): a source must list the user's dotfiles", want, got)
		}
	}
}
