package cmd

import (
	"context"
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
)

func newRestoreCommand() *cobra.Command {
	var flags repoFlags

	cmd := &cobra.Command{
		Use:   "restore <snapshot> <target>",
		Short: "Restore a snapshot into an empty directory",
		Long: "Restore a snapshot, named by the key that `kist snapshots` prints.\n\n" +
			"The target must not exist or must be empty. Restoring over live data\n" +
			"is not something a backup tool should do by inference.",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			return flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				stats, err := r.Restore(ctx, args[0], args[1], repo.RestoreOptions{Warnf: warnTo(cmd)})
				if err != nil {
					return err
				}

				fmt.Fprintf(cmd.OutOrStdout(), "restored %d files, %d directories, %d symlinks, %d hard links (%s)\n",
					stats.Files, stats.Dirs, stats.Symlinks, stats.Links, humanBytes(stats.Bytes))
				return nil
			})
		},
	}
	flags.register(cmd)
	return cmd
}
