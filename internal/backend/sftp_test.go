package backend

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"errors"
	"fmt"
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
func launchSFTP() (*sftpServer, error) {
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	out, err := exec.Command("docker", "run", "-d", "--rm",
		"-p", fmt.Sprintf("127.0.0.1:%d:22", port),
		sftpImage, fmt.Sprintf("%s:%s:1001::repo", sftpUser, sftpPass)).CombinedOutput()
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
	// sshd accepts TCP before it is ready to complete a handshake.
	time.Sleep(time.Second)
	return srv, nil
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
