package backend

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"golang.org/x/crypto/ssh"
	"golang.org/x/crypto/ssh/knownhosts"
)

const (
	sftpTestEnv = "KIST_SFTP_TEST"
	sftpImage   = "atmoz/sftp:debian"
	sftpUser    = "kist"
	sftpPass    = "kist-test-password"
)

type sftpServer struct {
	container  string
	host       string
	port       int
	knownHosts string
}

var (
	sftpOnce sync.Once
	sftpEnv  *sftpServer
	sftpErr  error
)

func startSFTP(t *testing.T) *sftpServer {
	t.Helper()
	if os.Getenv(sftpTestEnv) != "1" {
		t.Skipf("set %s=1 to run the SFTP tests against OpenSSH in Docker", sftpTestEnv)
	}
	sftpOnce.Do(func() { sftpEnv, sftpErr = launchSFTP() })
	if sftpErr != nil {
		t.Fatalf("start sftp: %v", sftpErr)
	}
	return sftpEnv
}

// launchSFTP runs OpenSSH's sftp-server in a container and writes a
// known_hosts file holding its host key -- the backend refuses to talk
// to a server it cannot verify, so the test has to do what a person
// would do with ssh-keyscan.
func launchSFTP(dockerArgs ...string) (*sftpServer, error) {
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	args := append([]string{"run", "-d", "--rm", "-p", fmt.Sprintf("127.0.0.1:%d:22", port)}, dockerArgs...)
	args = append(args, sftpImage, fmt.Sprintf("%s:%s:1001::repo", sftpUser, sftpPass))
	out, err := exec.Command("docker", args...).CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("docker run: %w: %s", err, out)
	}
	// A first run pulls the image and prints about it before the ID.
	lines := strings.Fields(string(out))
	srv := &sftpServer{container: lines[len(lines)-1], host: "127.0.0.1", port: port}

	deadline := time.Now().Add(60 * time.Second)
	var hostKey []byte
	for {
		hostKey, err = exec.Command("docker", "exec", srv.container, "cat", "/etc/ssh/ssh_host_ed25519_key.pub").Output()
		if err == nil && len(hostKey) > 0 {
			if conn, err := net.DialTimeout("tcp", fmt.Sprintf("%s:%d", srv.host, srv.port), time.Second); err == nil {
				_ = conn.Close()
				break
			}
		}
		if time.Now().After(deadline) {
			srv.stop()
			return nil, fmt.Errorf("sftp server did not become ready: %w", err)
		}
		time.Sleep(250 * time.Millisecond)
	}
	dir, err := os.MkdirTemp("", "kist-sftp-known-hosts")
	if err != nil {
		srv.stop()
		return nil, err
	}
	srv.knownHosts = filepath.Join(dir, "known_hosts")
	line := fmt.Sprintf("[%s]:%d %s", srv.host, srv.port, strings.TrimSpace(string(hostKey)))
	if err := os.WriteFile(srv.knownHosts, []byte(line+"\n"), 0o600); err != nil {
		srv.stop()
		return nil, err
	}
	// sshd accepts TCP before it can complete a handshake, and the
	// container's entrypoint may still be generating keys. Ready means
	// a real session opens.
	for {
		cfg := srv.config("readiness")
		cfg.Timeout = 2 * time.Second
		b, err := CreateSFTP(context.Background(), cfg)
		if err == nil {
			b.discard()
			return srv, nil
		}
		if time.Now().After(deadline) {
			srv.stop()
			return nil, fmt.Errorf("sftp server never completed a handshake: %w", err)
		}
		time.Sleep(250 * time.Millisecond)
	}
}

func (s *sftpServer) stop() {
	if s.container == "" {
		return
	}
	if out, err := exec.Command("docker", "stop", "-t", "1", s.container).CombinedOutput(); err != nil {
		fmt.Fprintf(os.Stderr, "stop sftp container %s: %v: %s\n", s.container, err, out)
	}
	if s.knownHosts != "" {
		_ = os.RemoveAll(filepath.Dir(s.knownHosts))
	}
}

func (s *sftpServer) config(dir string) SFTPConfig {
	return SFTPConfig{
		User: sftpUser, Host: s.host, Port: s.port, Path: "repo/" + dir,
		KnownHostsFile: s.knownHosts, Password: []byte(sftpPass), Timeout: 10 * time.Second,
	}
}

var sftpDirCounter int

func newTestSFTP(t *testing.T) Backend {
	t.Helper()
	srv := startSFTP(t)
	sftpDirCounter++
	b, err := CreateSFTP(context.Background(), srv.config(fmt.Sprintf("t%d-%d", os.Getpid(), sftpDirCounter)))
	if err != nil {
		t.Fatalf("create sftp backend: %v", err)
	}
	t.Cleanup(func() {
		if err := b.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return b
}

func TestSFTPConformance(t *testing.T) {
	startSFTP(t)
	runConformance(t, newTestSFTP)
}

func TestSFTPPutIfAbsentHasOneWinner(t *testing.T) {
	ctx := context.Background()
	b := newTestSFTP(t)

	const writers = 8
	var (
		wg      sync.WaitGroup
		mu      sync.Mutex
		wins    int
		existed int
	)
	start := make(chan struct{})
	for i := range writers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			payload := []byte("shared content")
			err := b.PutIfAbsent(ctx, "packs/contended", bytes.NewReader(payload), int64(len(payload)))
			mu.Lock()
			defer mu.Unlock()
			switch {
			case err == nil:
				wins++
			case errors.Is(err, ErrExists):
				existed++
			default:
				t.Errorf("writer %d: %v", i, err)
			}
		}()
	}
	close(start)
	wg.Wait()
	if wins != 1 || existed != writers-1 {
		t.Errorf("%d winners and %d ErrExists, want 1 and %d", wins, existed, writers-1)
	}
}

func TestSFTPFailedPutLeavesNothing(t *testing.T) {
	ctx := context.Background()
	b := newTestSFTP(t)

	err := b.PutIfAbsent(ctx, "packs/broken", failingReader{err: errors.New("disk on fire")}, 10)
	if err == nil {
		t.Fatal("put from a failing reader succeeded")
	}
	if _, err := b.Stat(ctx, "packs/broken"); !errors.Is(err, ErrNotFound) {
		t.Errorf("object exists after a failed put: %v", err)
	}
	// A short reader is a failure too: the size was a promise.
	err = b.PutIfAbsent(ctx, "packs/short", strings.NewReader("abc"), 10)
	if err == nil || errors.Is(err, ErrExists) {
		t.Fatalf("short put: err = %v", err)
	}
	n := 0
	if err := b.List(ctx, "packs/", func(FileInfo) error { n++; return nil }); err != nil {
		t.Fatal(err)
	}
	if n != 0 {
		t.Errorf("%d objects listed under packs/ after two failed puts, want 0 (scratch files must be hidden or gone)", n)
	}
	s, ok := b.(*SFTP)
	if !ok {
		t.Fatalf("backend is %T", b)
	}
	entries, err := s.client.ReadDir(s.root + "/packs")
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range entries {
		t.Errorf("scratch file left behind: %s", e.Name())
	}
}

// A trailing slash in the path is a natural thing to type. It must not
// make the repository look empty: List builds keys by trimming the root,
// and an uncleaned root trims nothing.
func TestSFTPListSurvivesAnUncleanRootPath(t *testing.T) {
	ctx := context.Background()
	srv := startSFTP(t)
	sftpDirCounter++
	cfg := srv.config(fmt.Sprintf("t%d-%d/", os.Getpid(), sftpDirCounter))
	cfg.Path = "/" + cfg.Path // absolute with a trailing slash: /repo/tN/
	b, err := CreateSFTP(ctx, cfg)
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	defer func() {
		if err := b.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	}()
	if err := PutBytesIfAbsent(ctx, b, "packs/one", []byte("x")); err != nil {
		t.Fatal(err)
	}
	var keys []string
	if err := b.List(ctx, "packs/", func(fi FileInfo) error { keys = append(keys, fi.Key); return nil }); err != nil {
		t.Fatal(err)
	}
	if len(keys) != 1 || keys[0] != "packs/one" {
		t.Fatalf("List under a root with a trailing slash returned %v, want [packs/one]", keys)
	}
}

func TestSFTPRefusesAnUnknownHost(t *testing.T) {
	srv := startSFTP(t)
	empty := filepath.Join(t.TempDir(), "known_hosts")
	if err := os.WriteFile(empty, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	cfg := srv.config("unknown-host")
	cfg.KnownHostsFile = empty
	_, err := OpenSFTP(context.Background(), cfg)
	if err == nil || !strings.Contains(err.Error(), "host key") {
		t.Fatalf("open with an empty known_hosts: err = %v, want a host key error", err)
	}
	// A key of the right type that is not the server's: the mismatch
	// that the algorithm retry must not paper over.
	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	signer, err := ssh.NewSignerFromKey(priv)
	if err != nil {
		t.Fatal(err)
	}
	wrong := filepath.Join(t.TempDir(), "known_hosts")
	line := knownhosts.Line([]string{fmt.Sprintf("[%s]:%d", srv.host, srv.port)}, signer.PublicKey())
	if err := os.WriteFile(wrong, []byte(line+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	cfg.KnownHostsFile = wrong
	_, err = OpenSFTP(context.Background(), cfg)
	if err == nil || !strings.Contains(err.Error(), "key mismatch") {
		t.Fatalf("open with a wrong host key: err = %v, want a key mismatch", err)
	}

	if _, err := OpenSFTP(context.Background(), srv.config("nope")); !errors.Is(err, ErrNotFound) {
		t.Errorf("open of a missing directory: err = %v, want ErrNotFound", err)
	}
}

func TestParseSFTPLocation(t *testing.T) {
	cases := []struct {
		in   string
		want SFTPConfig
		err  bool
	}{
		{in: "sftp://alice@backup.example:2222/srv/kist", want: SFTPConfig{User: "alice", Host: "backup.example", Port: 2222, Path: "/srv/kist"}},
		{in: "sftp://h/srv/kist/", want: SFTPConfig{Host: "h", Port: 22, Path: "/srv/kist"}},
		{in: "sftp://h//srv//kist", want: SFTPConfig{Host: "h", Port: 22, Path: "/srv/kist"}},
		{in: "sftp://h/~/x/", want: SFTPConfig{Host: "h", Port: 22, Path: "x"}},
		{in: "sftp://backup.example/~/kist", want: SFTPConfig{Host: "backup.example", Port: 22, Path: "kist"}},
		{in: "sftp://backup.example/~", want: SFTPConfig{Host: "backup.example", Port: 22, Path: "."}},
		{in: "sftp://[::1]:22/x", want: SFTPConfig{Host: "::1", Port: 22, Path: "/x"}},
		{in: "sftp://alice:secret@h/x", err: true},
		{in: "sftp://h", err: true},
		{in: "sftp:///x", err: true},
		{in: "sftp://h:99999/x", err: true},
		{in: "s3://h/x", err: true},
	}
	for _, tc := range cases {
		got, err := ParseSFTPLocation(tc.in)
		if tc.err {
			if err == nil {
				t.Errorf("%s: no error", tc.in)
			}
			continue
		}
		if err != nil {
			t.Errorf("%s: %v", tc.in, err)
			continue
		}
		if got.User != tc.want.User || got.Host != tc.want.Host || got.Port != tc.want.Port || got.Path != tc.want.Path {
			t.Errorf("%s: got %+v, want %+v", tc.in, got, tc.want)
		}
	}
}

// pkg/sftp has no context-aware Put or Get: the context passed to those
// methods is ignored, and a cancelled one neither stops the transfer nor
// fails it. That is a documented limitation (ADR 008); this test pins the
// observed behaviour so a change in the library is noticed.
func TestSFTPPutAndGetIgnoreACancelledContext(t *testing.T) {
	b := newTestSFTP(t)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	const size = 32 << 20
	payload := bytes.Repeat([]byte("kist"), size/4)
	start := time.Now()
	if err := b.Put(ctx, "packs/cancelled", bytes.NewReader(payload), size); err != nil {
		t.Fatalf("put with a cancelled context: %v (the library started honouring ctx; update ADR 008)", err)
	}
	put := time.Since(start)

	rc, err := b.Get(ctx, "packs/cancelled", 0, ReadToEnd)
	if err != nil {
		t.Fatalf("get with a cancelled context: %v", err)
	}
	n, err := io.Copy(io.Discard, rc)
	_ = rc.Close()
	if err != nil || n != size {
		t.Fatalf("get with a cancelled context: %d bytes, %v", n, err)
	}
	t.Logf("cancelled context ignored: put of %d MiB completed in %v, get returned all %d bytes", size>>20, put.Round(time.Millisecond), n)
}

// format.md §10 says the backup account cannot have `remove` on the
// `internal-sftp -P` blacklist, because every PutIfAbsent ends by
// removing its spool file. This runs a second server with exactly that
// blacklist (atmoz/sftp executes /etc/sftp.d/* before sshd starts) and
// pins the consequence: the put still succeeds, the spool file stays
// behind, and Delete fails.
func TestSFTPRemoveBlacklistLeavesSpoolFilesBehind(t *testing.T) {
	if os.Getenv(sftpTestEnv) != "1" {
		t.Skipf("set %s=1 to run the SFTP tests against OpenSSH in Docker", sftpTestEnv)
	}
	script := filepath.Join(t.TempDir(), "blacklist.sh")
	patch := "#!/bin/sh\nset -e\nsed -i 's/^ForceCommand internal-sftp$/ForceCommand internal-sftp -P remove/' /etc/ssh/sshd_config\ngrep -q 'internal-sftp -P remove' /etc/ssh/sshd_config\n"
	if err := os.WriteFile(script, []byte(patch), 0o755); err != nil { //nolint:gosec // it has to be executable for the container's entrypoint to run it
		t.Fatal(err)
	}
	started := time.Now()
	srv, err := launchSFTP("-v", script+":/etc/sftp.d/blacklist.sh:ro")
	if err != nil {
		t.Fatalf("start sftp with a blacklist: %v", err)
	}
	t.Cleanup(srv.stop)
	t.Logf("second server ready in %v", time.Since(started).Round(time.Millisecond))

	ctx := context.Background()
	b, err := CreateSFTP(ctx, srv.config("blacklist"))
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	t.Cleanup(func() {
		if err := b.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})

	// First prove the blacklist is in force, or the rest passes for the
	// wrong reason.
	if err := b.Put(ctx, "probe", strings.NewReader("x"), 1); err != nil {
		t.Fatalf("put under the blacklist: %v", err)
	}
	if err := b.Delete(ctx, "probe"); err == nil {
		t.Fatal("delete succeeded: the remove blacklist did not take effect")
	} else {
		t.Logf("delete under -P remove: %v", err)
	}

	if err := b.PutIfAbsent(ctx, "packs/one", strings.NewReader("pack"), 4); err != nil {
		t.Fatalf("put-if-absent under the blacklist: %v", err)
	}
	dir, err := b.path("packs")
	if err != nil {
		t.Fatal(err)
	}
	entries, err := b.client.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	var stray []string
	for _, e := range entries {
		if strings.HasPrefix(e.Name(), ".tmp-") {
			stray = append(stray, e.Name())
		}
	}
	if len(stray) == 0 {
		t.Fatalf("no spool file left behind; entries: %v", entries)
	}
	t.Logf("put-if-absent succeeded and left %d spool file(s) behind: %v", len(stray), stray)
}

// The GC protocol feeds Modified into every age comparison; a server
// that omits ACMODTIME (pkg/sftp maps it to the Unix epoch) must fail
// loudly instead of reading as "infinitely old" -- the data-loss
// direction. The Rust side rejects the same case with NoMtime; it can
// tell absent from an explicit zero and respects the latter, but the
// third-party library here cannot, so epoch is refused outright.
func TestSFTPMissingMtimeIsRefused(t *testing.T) {
	if _, err := modifiedAt("packs/x", time.Unix(0, 0)); !errors.Is(err, ErrNoMtime) {
		t.Fatalf("epoch mtime: err = %v, want ErrNoMtime", err)
	}
	if _, err := modifiedAt("packs/x", time.Time{}); !errors.Is(err, ErrNoMtime) {
		t.Fatalf("zero time: err = %v, want ErrNoMtime", err)
	}
	at := time.Unix(1_750_000_123, 456_000_000)
	got, err := modifiedAt("packs/x", at)
	if err != nil {
		t.Fatalf("real mtime: %v", err)
	}
	if want := at.Truncate(time.Second); !got.Equal(want) {
		t.Fatalf("mtime = %v, want %v", got, want)
	}
}

// The repo namespace is shallow (snapshots/<client>/<ts>,
// trees/<2hex>/<id>); a directory chain deeper than the cap is not a
// kist object layout -- or a hostile server fabricating one -- and
// must fail the listing cleanly instead of recursing without bound.
// The Rust peer enforces the same cap.
func TestSFTPListRejectsAbsurdDirectoryDepth(t *testing.T) {
	ctx := context.Background()
	b := newTestSFTP(t)

	deep := strings.Repeat("d/", 20)
	if err := b.Put(ctx, deep+"x", bytes.NewReader([]byte{1}), 1); err != nil {
		t.Fatalf("put deep object: %v", err)
	}
	if err := b.List(ctx, "", func(FileInfo) error { return nil }); err == nil {
		t.Fatal("list of a 20-level directory chain must fail (repo namespace depth cap)")
	}

	// Normal depth keeps working.
	if err := b.Put(ctx, "packs/normal", bytes.NewReader([]byte{2}), 1); err != nil {
		t.Fatalf("put: %v", err)
	}
	n := 0
	if err := b.List(ctx, "packs/", func(FileInfo) error { n++; return nil }); err != nil {
		t.Fatalf("list packs/: %v", err)
	}
	if n != 1 {
		t.Fatalf("expected 1 pack object, got %d", n)
	}
}
