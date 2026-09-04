package backend

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net"
	"net/url"
	"os"
	"os/user"
	"path"
	"strconv"
	"strings"
	"time"

	"github.com/pkg/sftp"
	"golang.org/x/crypto/ssh"
	"golang.org/x/crypto/ssh/agent"
	"golang.org/x/crypto/ssh/knownhosts"
)

// SFTPConfig says how to reach a repository over SFTP.
type SFTPConfig struct {
	User string
	Host string
	Port int

	// Path is the repository directory on the server. A path that does
	// not start with "/" is relative to the directory the server starts
	// the session in, normally the user's home.
	Path string

	// KnownHostsFile is where the server's host key must already be
	// listed. Empty means ~/.ssh/known_hosts. There is no option to skip
	// the check: a backup that can be redirected to a server of an
	// attacker's choosing is a backup of the attacker's choosing.
	KnownHostsFile string

	// KeyFile is a private key to offer, with Passphrase if it has one.
	// Password is offered last. An SSH agent at $SSH_AUTH_SOCK is tried
	// first whenever one is there.
	KeyFile    string
	Passphrase []byte
	Password   []byte

	// Timeout bounds the TCP connect and the handshake. Zero means 30s.
	Timeout time.Duration
}

// SFTP stores objects as files on an SFTP server, one file per key.
//
// It is the local backend over a network: every visible name is made by
// hard-linking a fully written, synced scratch file into place, so a
// crash can leave a stray ".tmp-<random>" file but never a truncated
// object under a real key. That needs the hardlink@openssh.com
// extension; a server without it is refused, because the fallback --
// rename -- replaces on some servers and refuses on others, and a
// conditional write whose condition depends on the server is not one.
type SFTP struct {
	conn     *ssh.Client
	client   *sftp.Client
	root     string
	location string
	fsync    bool
}

// SFTP implements Backend.
var _ Backend = (*SFTP)(nil)

// ParseSFTPLocation parses sftp://[user@]host[:port]/path. The path is
// absolute; write sftp://host/~/path for one relative to the session's
// starting directory.
func ParseSFTPLocation(location string) (SFTPConfig, error) {
	u, err := url.Parse(location)
	if err != nil {
		return SFTPConfig{}, fmt.Errorf("parse %q: %w", location, err)
	}
	if u.Scheme != "sftp" {
		return SFTPConfig{}, fmt.Errorf("parse %q: scheme is %q, want sftp", location, u.Scheme)
	}
	if u.Hostname() == "" {
		return SFTPConfig{}, fmt.Errorf("parse %q: no host", location)
	}
	cfg := SFTPConfig{Host: u.Hostname(), Port: 22, Path: u.Path}
	if u.User != nil {
		cfg.User = u.User.Username()
		if _, hasPassword := u.User.Password(); hasPassword {
			return SFTPConfig{}, fmt.Errorf("parse %q: a password in the URL ends up in shell history and process listings; use KIST_SFTP_PASSWORD", location)
		}
	}
	if p := u.Port(); p != "" {
		if cfg.Port, err = strconv.Atoi(p); err != nil || cfg.Port <= 0 || cfg.Port > 65535 {
			return SFTPConfig{}, fmt.Errorf("parse %q: bad port %q", location, p)
		}
	}
	if rest, ok := strings.CutPrefix(cfg.Path, "/~/"); ok {
		cfg.Path = rest
	} else if cfg.Path == "/~" {
		cfg.Path = "."
	}
	if cfg.Path == "" {
		return SFTPConfig{}, fmt.Errorf("parse %q: no path", location)
	}
	cfg.Path = path.Clean(cfg.Path)
	return cfg, nil
}

// OpenSFTP connects and returns a backend over an existing directory.
func OpenSFTP(ctx context.Context, cfg SFTPConfig) (*SFTP, error) {
	s, err := dialSFTP(ctx, cfg)
	if err != nil {
		return nil, err
	}
	info, err := s.client.Stat(s.root)
	if err != nil {
		s.discard()
		return nil, fmt.Errorf("open sftp backend at %s: %w", s.location, s.classify(err))
	}
	if !info.IsDir() {
		s.discard()
		return nil, fmt.Errorf("open sftp backend at %s: not a directory", s.location)
	}
	return s, nil
}

// CreateSFTP connects, creates the directory along with any parents, and
// returns a backend over it. An existing empty directory is accepted; a
// non-empty one is not.
func CreateSFTP(ctx context.Context, cfg SFTPConfig) (*SFTP, error) {
	s, err := dialSFTP(ctx, cfg)
	if err != nil {
		return nil, err
	}
	switch entries, err := s.client.ReadDir(s.root); {
	case err == nil && len(entries) > 0:
		s.discard()
		return nil, fmt.Errorf("create sftp backend at %s: directory is not empty", s.location)
	case err != nil && !errors.Is(err, fs.ErrNotExist):
		s.discard()
		return nil, fmt.Errorf("create sftp backend at %s: %w", s.location, s.classify(err))
	}
	if err := s.client.MkdirAll(s.root); err != nil {
		s.discard()
		return nil, fmt.Errorf("create sftp backend at %s: %w", s.location, s.classify(err))
	}
	return s, nil
}

func dialSFTP(ctx context.Context, cfg SFTPConfig) (*SFTP, error) {
	if cfg.Host == "" {
		return nil, errors.New("open sftp backend: no host")
	}
	if cfg.Path == "" {
		return nil, errors.New("open sftp backend: no path")
	}
	port := cfg.Port
	if port == 0 {
		port = 22
	}
	timeout := cfg.Timeout
	if timeout == 0 {
		timeout = 30 * time.Second
	}
	userName := cfg.User
	if userName == "" {
		u, err := user.Current()
		if err != nil {
			return nil, fmt.Errorf("open sftp backend: no user given and none found: %w", err)
		}
		userName = u.Username
	}
	location := fmt.Sprintf("sftp://%s@%s/%s", userName, net.JoinHostPort(cfg.Host, strconv.Itoa(port)), strings.TrimPrefix(cfg.Path, "/"))

	hostKeys, err := hostKeyCallback(cfg.KnownHostsFile)
	if err != nil {
		return nil, fmt.Errorf("open sftp backend at %s: %w", location, err)
	}
	auth, closeAgent, err := authMethods(cfg)
	if err != nil {
		return nil, fmt.Errorf("open sftp backend at %s: %w", location, err)
	}
	defer closeAgent()

	sshCfg := &ssh.ClientConfig{
		User:            userName,
		Auth:            auth,
		HostKeyCallback: hostKeys,
		Timeout:         timeout,
	}
	addr := net.JoinHostPort(cfg.Host, strconv.Itoa(port))
	conn, err := sshDial(ctx, addr, sshCfg, timeout)
	if err != nil {
		return nil, fmt.Errorf("open sftp backend at %s: %w", location, err)
	}

	// Concurrent writes pipeline the packets of one upload; without them
	// a 64 MiB pack goes at 61 MiB/s on loopback, with them several
	// times that. Safe here because nothing links the scratch file into
	// place until it is closed and synced.
	client, err := sftp.NewClient(conn, sftp.UseConcurrentWrites(true))
	if err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("open sftp backend at %s: start sftp: %w", location, err)
	}
	// Cleaned here as well as in ParseSFTPLocation, for a config built
	// by hand: List derives keys by trimming the root off each path, and
	// a root with a trailing slash would trim nothing and list nothing.
	s := &SFTP{conn: conn, client: client, root: path.Clean(cfg.Path), location: location}

	if v, ok := client.HasExtension("hardlink@openssh.com"); !ok || v != "1" {
		s.discard()
		return nil, fmt.Errorf("open sftp backend at %s: the server does not offer hardlink@openssh.com, which kist needs for conditional writes; OpenSSH's sftp-server does", location)
	}
	if v, ok := client.HasExtension("fsync@openssh.com"); ok && v == "1" {
		s.fsync = true
	}
	return s, nil
}

// sshDial connects and handshakes, once more if the first attempt failed
// only because the server offered a key of a type known_hosts does not
// hold for it. A server with ed25519 and ECDSA keys, listed in
// known_hosts under ed25519 alone, presents whichever the client's
// algorithm preference picks; ssh(1) handles this by preferring the
// types it knows, and so does this.
func sshDial(ctx context.Context, addr string, cfg *ssh.ClientConfig, timeout time.Duration) (*ssh.Client, error) {
	attempt := func(algorithms []string) (*ssh.Client, error) {
		c := *cfg
		c.HostKeyAlgorithms = algorithms
		dialer := net.Dialer{Timeout: timeout}
		tcp, err := dialer.DialContext(ctx, "tcp", addr) //nolint:gosec // the host is the user's repository location; connecting to it is the feature
		if err != nil {
			return nil, err
		}
		sshConn, chans, reqs, err := ssh.NewClientConn(tcp, addr, &c)
		if err != nil {
			_ = tcp.Close()
			return nil, err
		}
		return ssh.NewClient(sshConn, chans, reqs), nil
	}

	conn, err := attempt(nil)
	var keyErr *knownhosts.KeyError
	if errors.As(err, &keyErr) && len(keyErr.Want) > 0 {
		algorithms := make([]string, 0, len(keyErr.Want))
		for _, k := range keyErr.Want {
			algorithms = append(algorithms, k.Key.Type())
		}
		if conn, retryErr := attempt(algorithms); retryErr == nil {
			return conn, nil
		}
	}
	return conn, err
}

// hostKeyCallback verifies the server against a known_hosts file, and
// says which file when the server is not in it.
func hostKeyCallback(file string) (ssh.HostKeyCallback, error) {
	if file == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return nil, fmt.Errorf("no known_hosts file given and no home directory: %w", err)
		}
		file = home + "/.ssh/known_hosts"
	}
	check, err := knownhosts.New(file)
	if err != nil {
		return nil, fmt.Errorf("known_hosts %s: %w", file, err)
	}
	return func(hostname string, remote net.Addr, key ssh.PublicKey) error {
		if err := check(hostname, remote, key); err != nil {
			return fmt.Errorf("host key check against %s: %w", file, err)
		}
		return nil
	}, nil
}

func authMethods(cfg SFTPConfig) (methods []ssh.AuthMethod, cleanup func(), err error) {
	cleanup = func() {}
	if sock := os.Getenv("SSH_AUTH_SOCK"); sock != "" {
		if conn, err := net.Dial("unix", sock); err == nil { //nolint:gosec // $SSH_AUTH_SOCK is the user's own agent
			methods = append(methods, ssh.PublicKeysCallback(agent.NewClient(conn).Signers))
			cleanup = func() { _ = conn.Close() }
		}
	}
	if cfg.KeyFile != "" {
		pem, err := os.ReadFile(cfg.KeyFile)
		if err != nil {
			cleanup()
			return nil, nil, fmt.Errorf("read key %s: %w", cfg.KeyFile, err)
		}
		var signer ssh.Signer
		if len(cfg.Passphrase) > 0 {
			signer, err = ssh.ParsePrivateKeyWithPassphrase(pem, cfg.Passphrase)
		} else {
			signer, err = ssh.ParsePrivateKey(pem)
		}
		if err != nil {
			cleanup()
			return nil, nil, fmt.Errorf("parse key %s: %w", cfg.KeyFile, err)
		}
		methods = append(methods, ssh.PublicKeys(signer))
	}
	if len(cfg.Password) > 0 {
		methods = append(methods, ssh.Password(string(cfg.Password)))
	}
	if len(methods) == 0 {
		cleanup()
		return nil, nil, errors.New("no way to authenticate: no SSH agent, no key file, no password")
	}
	return methods, cleanup, nil
}

// discard closes everything on an error path that already has an error
// to report.
func (s *SFTP) discard() {
	_ = s.client.Close() //nolint:errcheck // cleanup on an error path; the caller's error is the one that matters
	_ = s.conn.Close()
}

// Location reports where the backend stores objects.
func (s *SFTP) Location() string { return s.location }

// Close ends the session and the connection.
func (s *SFTP) Close() error {
	cerr := s.client.Close()
	if err := s.conn.Close(); err != nil && cerr == nil {
		cerr = err
	}
	if cerr != nil {
		return fmt.Errorf("close sftp backend at %s: %w", s.location, cerr)
	}
	return nil
}

func (s *SFTP) path(key string) (string, error) {
	if err := ValidateKey(key); err != nil {
		return "", err
	}
	return path.Join(s.root, key), nil
}

// Get opens a ranged reader over the object.
func (s *SFTP) Get(_ context.Context, key string, off, length int64) (io.ReadCloser, error) {
	p, err := s.path(key)
	if err != nil {
		return nil, err
	}
	if off < 0 {
		return nil, fmt.Errorf("get %s: offset %d is negative", key, off)
	}
	if length < 0 && length != ReadToEnd {
		return nil, fmt.Errorf("get %s: length %d is negative", key, length)
	}

	f, err := s.client.Open(p)
	if err != nil {
		return nil, s.wrap("get", key, err)
	}
	if off > 0 {
		if _, err := f.Seek(off, io.SeekStart); err != nil {
			_ = f.Close()
			return nil, fmt.Errorf("get %s: seek to %d: %w", key, off, err)
		}
	}
	if length == ReadToEnd {
		return f, nil
	}
	return sectionReader{Reader: io.LimitReader(f, length), closer: f}, nil
}

// Put writes the object, replacing whatever was there.
func (s *SFTP) Put(_ context.Context, key string, r io.Reader, size int64) error {
	p, err := s.path(key)
	if err != nil {
		return err
	}
	tmp, written, err := s.spool(path.Dir(p), r)
	if err != nil {
		return fmt.Errorf("put %s: %w", key, err)
	}
	defer func() { _ = s.client.Remove(tmp) }()

	if size >= 0 && written != size {
		return fmt.Errorf("put %s: read %d bytes, expected %d", key, written, size)
	}
	if err := s.client.PosixRename(tmp, p); err != nil {
		return fmt.Errorf("put %s: %w", key, s.classify(err))
	}
	return nil
}

// PutIfAbsent stores data unless the key is taken, in which case it
// returns ErrExists.
//
// The conditional step is a hard link of the scratch file onto the final
// name, which the server refuses if the name exists. SFTP has no status
// code for "exists" -- OpenSSH reports the generic SSH_FX_FAILURE -- so a
// refused link is followed by a Stat: the name exists, and a link is
// atomic, so whatever is there is complete, and that is ErrExists;
// otherwise the failure was something else and is returned as such.
//
// The Stat before the write is an optimisation for the common case: a
// tree that has not changed collides every night, and skipping the
// upload matters. The link remains the guard.
func (s *SFTP) PutIfAbsent(_ context.Context, key string, r io.Reader, size int64) error {
	p, err := s.path(key)
	if err != nil {
		return err
	}
	if _, err := s.client.Stat(p); err == nil {
		return fmt.Errorf("put %s: %w", key, ErrExists)
	}

	tmp, written, err := s.spool(path.Dir(p), r)
	if err != nil {
		return fmt.Errorf("put %s: %w", key, err)
	}
	defer func() { _ = s.client.Remove(tmp) }()

	if size >= 0 && written != size {
		return fmt.Errorf("put %s: read %d bytes, expected %d", key, written, size)
	}
	if err := s.client.Link(tmp, p); err != nil {
		if _, statErr := s.client.Stat(p); statErr == nil {
			return fmt.Errorf("put %s: %w", key, ErrExists)
		}
		return fmt.Errorf("put %s: %w", key, s.classify(err))
	}
	return nil
}

// spool writes r into a scratch file in dir, synced when the server can,
// and returns its path.
func (s *SFTP) spool(dir string, r io.Reader) (_ string, _ int64, err error) {
	if err := s.client.MkdirAll(dir); err != nil {
		return "", 0, fmt.Errorf("create directory %s: %w", dir, s.classify(err))
	}
	var suffix [8]byte
	if _, err := rand.Read(suffix[:]); err != nil {
		return "", 0, fmt.Errorf("name scratch file: %w", err)
	}
	scratch := path.Join(dir, ".tmp-"+hex.EncodeToString(suffix[:]))

	f, err := s.client.OpenFile(scratch, os.O_WRONLY|os.O_CREATE|os.O_EXCL)
	if err != nil {
		return "", 0, fmt.Errorf("create scratch file: %w", s.classify(err))
	}
	defer func() {
		if err != nil {
			_ = f.Close()
			_ = s.client.Remove(scratch)
		}
	}()

	written, err := f.ReadFrom(r)
	if err != nil {
		return "", 0, fmt.Errorf("write scratch file: %w", err)
	}
	if s.fsync {
		if err = f.Sync(); err != nil {
			return "", 0, fmt.Errorf("sync scratch file: %w", err)
		}
	}
	if err = f.Close(); err != nil {
		return "", 0, fmt.Errorf("close scratch file: %w", err)
	}
	return scratch, written, nil
}

// List walks every object whose key starts with prefix.
//
// SFTP lists one directory per round trip and holds each listing in
// memory; a packs/ directory of 160k entries is a few megabytes and a
// few seconds. That is the cost of a protocol with no pagination.
func (s *SFTP) List(ctx context.Context, prefix string, fn func(FileInfo) error) error {
	if err := ValidatePrefix(prefix); err != nil {
		return err
	}
	base, _ := path.Split(prefix)
	root := path.Join(s.root, base)
	if err := s.walk(ctx, root, prefix, fn); err != nil {
		return fmt.Errorf("list %q in %s: %w", prefix, s.location, err)
	}
	return nil
}

func (s *SFTP) walk(ctx context.Context, dir, prefix string, fn func(FileInfo) error) error {
	entries, err := s.client.ReadDirContext(ctx, dir)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil // nothing stored under this prefix yet
		}
		return s.classify(err)
	}
	for _, e := range entries {
		if err := ctx.Err(); err != nil {
			return err
		}
		name := e.Name()
		if strings.HasPrefix(name, ".") {
			continue // scratch file from an in-flight Put
		}
		full := path.Join(dir, name)
		if e.IsDir() {
			if err := s.walk(ctx, full, prefix, fn); err != nil {
				return err
			}
			continue
		}
		key := strings.TrimPrefix(full, s.root+"/")
		if s.root == "." {
			key = full
		}
		if !strings.HasPrefix(key, prefix) {
			continue
		}
		if err := fn(FileInfo{Key: key, Size: e.Size()}); err != nil {
			return err
		}
	}
	return nil
}

// Stat reports on one object.
func (s *SFTP) Stat(_ context.Context, key string) (FileInfo, error) {
	p, err := s.path(key)
	if err != nil {
		return FileInfo{}, err
	}
	info, err := s.client.Stat(p)
	if err != nil {
		return FileInfo{}, s.wrap("stat", key, err)
	}
	if info.IsDir() {
		return FileInfo{}, fmt.Errorf("stat %s: %w", key, ErrNotFound)
	}
	return FileInfo{Key: key, Size: info.Size()}, nil
}

// Delete removes an object, treating an absent object as already deleted.
func (s *SFTP) Delete(_ context.Context, key string) error {
	p, err := s.path(key)
	if err != nil {
		return err
	}
	if err := s.client.Remove(p); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("delete %s: %w", key, s.classify(err))
	}
	return nil
}

func (s *SFTP) wrap(op, key string, err error) error {
	return fmt.Errorf("%s %s: %w", op, key, s.classify(err))
}

// classify maps the library's errors onto the backend's sentinels.
func (s *SFTP) classify(err error) error {
	switch {
	case errors.Is(err, fs.ErrNotExist):
		return ErrNotFound
	case errors.Is(err, fs.ErrPermission):
		return ErrDenied
	default:
		return err
	}
}
