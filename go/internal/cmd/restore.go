package cmd

import (
	"context"
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

func newRestoreCommand() *cobra.Command {
	var flags repoFlags

	cmd := &cobra.Command{
		Use:   "restore <snapshot> <target>",
		Short: "Restore a snapshot into an empty directory",
		Long: "Restore a snapshot, named by the key that `kist-go snapshots` prints.\n\n" +
			"The target must not exist or must be empty. Restoring over live data\n" +
			"is not something a backup tool should do by inference.",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ev := event("restore")
			return finish(cmd, ev, flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				stats, err := r.Restore(ctx, args[0], args[1], repo.RestoreOptions{Warnf: warnInto(cmd, ev)})
				if err != nil {
					return err
				}
				ev.Restore = &report.RestoreResult{
					Snapshot: args[0], Target: args[1],
					Files: stats.Files, Dirs: stats.Dirs, Symlinks: stats.Symlinks, HardLinks: stats.Links, Bytes: stats.Bytes,
				}
				if jsonMode(cmd) {
					return nil
				}
				fmt.Fprintf(cmd.OutOrStdout(), "restored %d files, %d directories, %d symlinks, %d hard links (%s)\n",
					stats.Files, stats.Dirs, stats.Symlinks, stats.Links, humanBytes(stats.Bytes))
				return nil
			}))
		},
	}
	flags.register(cmd)
	return cmd
}
