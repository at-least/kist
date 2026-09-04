//go:build linux || darwin

package cmd

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/mount"
	"github.com/at-least/kist/internal/repo"
)

func init() {
	platformCommands = append(platformCommands, newMountCommand)
}

func newMountCommand() *cobra.Command {
	var (
		flags repoFlags
		debug bool
	)

	cmd := &cobra.Command{
		Use:   "mount <mountpoint>",
		Short: "Browse snapshots as a read-only filesystem",
		Long: "Mount the repository at an empty directory, laid out as\n" +
			"<client>/<timestamp>/<backed-up tree>, and serve it until interrupted.\n\n" +
			"Files are decrypted as they are read. Reading the end of a large file\n" +
			"decodes what comes before it once, because the format does not\n" +
			"record where each chunk's plaintext starts; sequential reads pay\n" +
			"nothing extra.",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				srv, err := mount.Mount(ctx, r, args[0], mount.Options{Warnf: warnTo(cmd), Debug: debug})
				if err != nil {
					return err
				}
				fmt.Fprintf(cmd.ErrOrStderr(), "mounted at %s; interrupt to unmount\n", args[0])

				ctx, stop := signal.NotifyContext(ctx, os.Interrupt, syscall.SIGTERM)
				defer stop()
				done := make(chan struct{})
				go func() { srv.Wait(); close(done) }()
				select {
				case <-ctx.Done():
					if err := srv.Unmount(); err != nil {
						return fmt.Errorf("%w; is something still open under %s?", err, args[0])
					}
					<-done
				case <-done:
				}
				return nil
			})
		},
	}
	flags.register(cmd)
	cmd.Flags().BoolVar(&debug, "debug", false, "log every filesystem request to stderr")
	return cmd
}
