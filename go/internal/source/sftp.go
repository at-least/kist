package source

import (
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path"
	"strings"

	"github.com/pkg/sftp"
	"golang.org/x/crypto/ssh"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/tree"
)

// Environment variables the sftp SOURCE reads for its credentials, the
// same names the CLI wires for an sftp repository. They are re-declared
// here rather than imported from the command layer, which is the wrong
// direction for a library package.
const (
	// SFTPKnownHostsFileEnv overrides the known_hosts file the server's
	// host key must already be listed in.
	SFTPKnownHostsFileEnv = "KIST_SFTP_KNOWN_HOSTS"

	// SFTPKeyEnv names a private key to offer.
	SFTPKeyEnv = "KIST_SFTP_KEY"

	// SFTPKeyPassphraseEnv is that key's passphrase, if it has one.
	SFTPKeyPassphraseEnv = "KIST_SFTP_KEY_PASSPHRASE" //nolint:gosec // an env var name, not a credential

	// SFTPPasswordEnv offers a password, last.
	SFTPPasswordEnv = "KIST_SFTP_PASSWORD" //nolint:gosec // an env var name, not a credential
)

// An SFTPSource is a directory on an SFTP server.
//
// It has NO symlink support: a server listing reports a link's own
// attributes (which is what a plain file looks like to this source), and
// opening one follows it. There is consequently no posix fast path
// either -- an sftp-kind source's mtime is the server's claim, and the
// format records only what the source can prove.
type SFTPSource struct {
	locator []byte
	root    string

	conn   *ssh.Client
	client *sftp.Client
}

// OpenSFTPSource connects to sftp://[user@]host[:port]/path, serving
// paths under path. Credentials come from the same KIST_SFTP_*
// environment the CLI uses for an sftp repository; the host key must
// already be in known_hosts.
func OpenSFTPSource(ctx context.Context, spec string) (*SFTPSource, error) {
	cfg, err := backend.ParseSFTPLocation(spec)
	if err != nil {
		return nil, fmt.Errorf("open sftp source %s: %w", spec, err)
	}
	cfg.KnownHostsFile = os.Getenv(SFTPKnownHostsFileEnv)
	cfg.KeyFile = os.Getenv(SFTPKeyEnv)
	cfg.Passphrase = []byte(os.Getenv(SFTPKeyPassphraseEnv))
	cfg.Password = []byte(os.Getenv(SFTPPasswordEnv))

	conn, client, err := backend.DialSFTPConn(ctx, cfg)
	if err != nil {
		return nil, fmt.Errorf("open sftp source %s: %w", spec, err)
	}
	return &SFTPSource{
		locator: []byte(spec),
		root:    path.Clean(cfg.Path),
		conn:    conn,
		client:  client,
	}, nil
}

// Locator is the URL the source was opened with, as given.
func (s *SFTPSource) Locator() []byte { return s.locator }

// MetaKind is sftp: the server maintains the modification time, and
// nothing else a tree entry records is provable.
func (s *SFTPSource) MetaKind() uint8 { return uint8(tree.MetaSFTP) }

// Close ends the session and the connection.
func (s *SFTPSource) Close() error {
	cerr := s.client.Close()
	if err := s.conn.Close(); err != nil && cerr == nil {
		cerr = err
	}
	if cerr != nil {
		return fmt.Errorf("close sftp source %s: %w", s.locator, cerr)
	}
	return nil
}

// join builds the server path for a relative source path.
func (s *SFTPSource) join(rel []byte) string {
	p := string(rel)
	if p == "" {
		return s.root
	}
	return path.Join(s.root, p)
}

// List reads one directory level. A directory that does not exist lists
// empty, the way an object store's listing does; any other failure is
// returned. Listing a FILE path fails: the SFTP protocol has no
// directory read for one, which is what makes a file-root backup fail
// loudly here instead of quietly storing an empty tree.
func (s *SFTPSource) List(ctx context.Context, dir []byte) ([]SourceItem, error) {
	p := s.join(dir)
	entries, err := s.client.ReadDirContext(ctx, p)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, nil
		}
		return nil, fmt.Errorf("list %s: %w", p, err)
	}

	out := make([]SourceItem, 0, len(entries))
	for _, e := range entries {
		name := e.Name()
		if strings.HasPrefix(name, ".") {
			continue // scratch files, the server's business
		}
		if e.IsDir() {
			out = append(out, SourceItem{Name: []byte(name), Kind: SourceItemKind{Kind: KindDir}})
			continue
		}
		// A symlink lands here too, with its own attributes: this source
		// has no readlink, and the entry says what the server said.
		out = append(out, SourceItem{
			Name: []byte(name),
			Kind: SourceItemKind{
				Kind:    KindFile,
				Size:    uint64(max(e.Size(), 0)), //nolint:gosec // a length is never negative
				MTimeNs: e.ModTime().UnixNano(),
			},
		})
	}
	return out, nil
}

// Read opens a file. A symlink is followed, the way an SFTP open reads
// one.
func (s *SFTPSource) Read(_ context.Context, file []byte) (io.ReadCloser, error) {
	p := s.join(file)
	f, err := s.client.Open(p)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", p, err)
	}
	return f, nil
}
