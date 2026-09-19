package source

import (
	"context"
	"io"
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"

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
		_ = srv.Close()        //nolint:errcheck // teardown
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

// fakeFileInfo is one canned entry for the hostile listing below.
type fakeFileInfo struct {
	name string
	dir  bool
}

func (f fakeFileInfo) Name() string { return f.name }
func (f fakeFileInfo) Size() int64  { return 0 }
func (f fakeFileInfo) Mode() os.FileMode {
	if f.dir {
		return os.ModeDir | 0o755
	}
	return 0o644
}
func (f fakeFileInfo) ModTime() time.Time { return time.Unix(0, 0) }
func (f fakeFileInfo) IsDir() bool        { return f.dir }
func (f fakeFileInfo) Sys() any           { return nil }

// dotLister serves a listing that CONTAINS "." and "..": the filexfer
// draft lets a server emit them and the client library passes them
// through. "." recursing into itself and ".." walking out of the source
// root are the failures the source must not allow.
type dotLister struct {
	entries []os.FileInfo
	done    bool
}

func (l *dotLister) Filelist(_ *sftp.Request) (sftp.ListerAt, error) {
	return l, nil
}

func (l *dotLister) ListAt(out []os.FileInfo, _ int64) (int, error) {
	if l.done {
		return 0, io.EOF
	}
	n := copy(out, l.entries)
	l.done = true
	return n, io.EOF
}

// A hostile listing may contain "." and ".."; the source must never see
// them ("." recurses into itself, ".." walks out of the source root).
// This PINS the guarantee the safety relies on: pkg/sftp's client
// filters them at protocol-decode time (client.go:414) -- verified
// here against a RequestServer that deliberately emits them. (The Rust
// side's openssh-sftp-client does NOT filter; its skip_in_listing
// carries the guard instead.)
func TestSFTPSourceSkipsSelfAndParentEntries(t *testing.T) {
	serverConn, clientConn := net.Pipe()
	srv := sftp.NewRequestServer(serverConn, sftp.Handlers{
		FileList: &dotLister{entries: []os.FileInfo{
			fakeFileInfo{name: ".", dir: true},
			fakeFileInfo{name: "..", dir: true},
			fakeFileInfo{name: ".bashrc"},
			fakeFileInfo{name: "readme.txt"},
		}},
	})
	go func() { _ = srv.Serve() }() //nolint:errcheck // best-effort server loop
	t.Cleanup(func() {
		_ = clientConn.Close() //nolint:errcheck // teardown
		_ = srv.Close()        //nolint:errcheck // teardown
	})
	client, err := sftp.NewClientPipe(clientConn, clientConn)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = client.Close() }) //nolint:errcheck // teardown

	src := &SFTPSource{root: "/data", client: client}
	items, err := src.List(context.Background(), nil)
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	got := map[string]bool{}
	for _, it := range items {
		got[string(it.Name)] = true
	}
	if got["."] || got[".."] {
		t.Fatalf("listing must skip . and .. (got %v): . recurses into itself, .. walks out of the source root", got)
	}
	if !got[".bashrc"] || !got["readme.txt"] {
		t.Fatalf("listing must keep the user's files (got %v)", got)
	}
}
