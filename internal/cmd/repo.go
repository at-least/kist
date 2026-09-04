package cmd

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
	"golang.org/x/term"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/repo"
)

// PasswordEnv is the environment variable a password may be passed in.
// It is the least bad of the non-interactive options: a command line is
// visible in the process table, and a file has to be created and removed.
const PasswordEnv = "KIST_PASSWORD"

// repoFlags are the flags every command that touches a repository takes.
type repoFlags struct {
	repository   string
	passwordFile string
	clientID     string
}

func (f *repoFlags) register(cmd *cobra.Command) {
	cmd.Flags().StringVarP(&f.repository, "repo", "r", "", "repository location (default $KIST_REPOSITORY)")
	cmd.Flags().StringVar(&f.passwordFile, "password-file", "", "read the password from this file instead of prompting")
	cmd.Flags().StringVar(&f.clientID, "client-id", "", "override this machine's client identifier")
}

// SFTP settings that have no place in a URL. The agent at $SSH_AUTH_SOCK
// is always tried first.
const (
	SFTPKnownHostsEnv    = "KIST_SFTP_KNOWN_HOSTS"    // default ~/.ssh/known_hosts
	SFTPKeyEnv           = "KIST_SFTP_KEY"            // private key file
	SFTPKeyPassphraseEnv = "KIST_SFTP_KEY_PASSPHRASE" //nolint:gosec // an env var name, not a credential
	SFTPPasswordEnv      = "KIST_SFTP_PASSWORD"       //nolint:gosec // an env var name, not a credential
)

// RepositoryEnv names the repository when --repo is not given.
const RepositoryEnv = "KIST_REPOSITORY"

func (f *repoFlags) location() (string, error) {
	if f.repository != "" {
		return f.repository, nil
	}
	if env := os.Getenv(RepositoryEnv); env != "" {
		return env, nil
	}
	return "", fmt.Errorf("no repository given: pass --repo or set %s", RepositoryEnv)
}

// options assembles repo.Options, reading the password from the file, the
// environment or the terminal, in that order.
func (f *repoFlags) options(cmd *cobra.Command, confirm bool) (repo.Options, error) {
	password, err := f.password(cmd, confirm)
	if err != nil {
		return repo.Options{}, err
	}
	return repo.Options{Password: password, ClientID: f.clientID, Warnf: warnTo(cmd)}, nil
}

func (f *repoFlags) password(cmd *cobra.Command, confirm bool) ([]byte, error) {
	if f.passwordFile != "" {
		data, err := os.ReadFile(f.passwordFile) //nolint:gosec // the path is the user's own argument
		if err != nil {
			return nil, fmt.Errorf("read password file: %w", err)
		}
		return []byte(strings.TrimRight(string(data), "\r\n")), nil
	}
	if env, ok := os.LookupEnv(PasswordEnv); ok {
		return []byte(env), nil
	}

	fd := int(os.Stdin.Fd())
	if !term.IsTerminal(fd) {
		return nil, fmt.Errorf("no password available: pass --password-file or set %s", PasswordEnv)
	}

	fmt.Fprint(cmd.ErrOrStderr(), "password: ")
	first, err := term.ReadPassword(fd)
	fmt.Fprintln(cmd.ErrOrStderr())
	if err != nil {
		return nil, fmt.Errorf("read password: %w", err)
	}
	if !confirm {
		return first, nil
	}

	fmt.Fprint(cmd.ErrOrStderr(), "password (again): ")
	second, err := term.ReadPassword(fd)
	fmt.Fprintln(cmd.ErrOrStderr())
	if err != nil {
		return nil, fmt.Errorf("read password: %w", err)
	}
	if string(first) != string(second) {
		return nil, errors.New("the two passwords do not match")
	}
	return first, nil
}

// openBackend resolves a location to a backend.
//
//	/path/to/dir            local filesystem
//	s3://bucket[/prefix]    S3-compatible object storage; credentials
//	                        from the AWS_* environment, endpoint from
//	                        $KIST_S3_ENDPOINT, path-style addressing
//	                        from $KIST_S3_PATH_STYLE=1
//
// An unknown scheme says so rather than being treated as a directory.
func openBackend(ctx context.Context, location string, create bool) (backend.Backend, error) {
	scheme, _, hasScheme := strings.Cut(location, "://")
	switch {
	case !hasScheme:
		if create {
			return backend.CreateLocal(location)
		}
		return backend.OpenLocal(location)
	case scheme == "s3":
		cfg, err := backend.ParseS3Location(location)
		if err != nil {
			return nil, err
		}
		// Create is a no-op for S3: the bucket must already exist, and
		// Init's own check refuses a prefix that already holds a config.
		return backend.OpenS3(ctx, cfg)
	case scheme == "sftp":
		cfg, err := backend.ParseSFTPLocation(location)
		if err != nil {
			return nil, err
		}
		cfg.KnownHostsFile = os.Getenv(SFTPKnownHostsEnv)
		cfg.KeyFile = os.Getenv(SFTPKeyEnv)
		cfg.Passphrase = []byte(os.Getenv(SFTPKeyPassphraseEnv))
		cfg.Password = []byte(os.Getenv(SFTPPasswordEnv))
		if create {
			return backend.CreateSFTP(ctx, cfg)
		}
		return backend.OpenSFTP(ctx, cfg)
	default:
		return nil, fmt.Errorf("repository scheme %q is not supported; this build knows local paths, s3:// and sftp://", scheme)
	}
}

// withRepository opens a repository, runs fn, and closes it.
func (f *repoFlags) withRepository(cmd *cobra.Command, fn func(context.Context, *repo.Repository) error) error {
	location, err := f.location()
	if err != nil {
		return err
	}
	opts, err := f.options(cmd, false)
	if err != nil {
		return err
	}

	b, err := openBackend(cmd.Context(), location, false)
	if err != nil {
		return err
	}

	r, err := repo.Open(cmd.Context(), b, opts)
	if err != nil {
		if cerr := b.Close(); cerr != nil {
			warnTo(cmd)("closing the backend: %v", cerr)
		}
		return err
	}
	defer func() {
		if cerr := r.Close(); cerr != nil && err == nil {
			err = cerr
		}
	}()

	return fn(cmd.Context(), r)
}

func warnTo(cmd *cobra.Command) func(string, ...any) {
	return func(format string, args ...any) {
		fmt.Fprintf(cmd.ErrOrStderr(), "kist: warning: "+format+"\n", args...)
	}
}
