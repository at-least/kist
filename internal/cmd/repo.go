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

// openBackend resolves a location to a backend. Only local paths are
// supported in M1; S3 and SFTP arrive with their milestones, and an
// unknown scheme says so rather than being treated as a directory name.
func openBackend(location string, create bool) (backend.Backend, error) {
	if scheme, _, ok := strings.Cut(location, "://"); ok {
		return nil, fmt.Errorf("repository scheme %q is not supported yet; this build stores repositories on the local filesystem", scheme)
	}
	if create {
		return backend.CreateLocal(location)
	}
	return backend.OpenLocal(location)
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

	b, err := openBackend(location, false)
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
