package cmd

import (
	"context"
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
)

func newBackupCommand() *cobra.Command {
	var (
		flags    repoFlags
		host     string
		spoolDir string
	)

	cmd := &cobra.Command{
		Use:   "backup <path>...",
		Short: "Back up paths into the repository",
		Long: "Walk each path and commit a snapshot.\n\n" +
			"The snapshot is written last, after every pack, tree and index it\n" +
			"refers to. An interrupted backup therefore leaves no snapshot and no\n" +
			"damage: the objects it did upload are simply unreferenced, and prune\n" +
			"reclaims them.",
		Args: cobra.MinimumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			return flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				snap, handle, err := r.Backup(ctx, args, repo.BackupOptions{
					Host:     host,
					SpoolDir: spoolDir,
					Warnf:    warnTo(cmd),
				})
				if err != nil {
					return err
				}

				out := cmd.OutOrStdout()
				fmt.Fprintf(out, "snapshot %s\n", handle.Key)
				fmt.Fprintf(out, "  %d files, %d directories, %d symlinks\n", snap.Stats.Files, snap.Stats.Dirs, snap.Stats.Symlinks)
				fmt.Fprintf(out, "  %s read, %s stored in %d new packs\n",
					humanBytes(snap.Stats.Bytes), humanBytes(snap.Stats.BytesStored), snap.Stats.PacksAdded)
				return nil
			})
		},
	}

	flags.register(cmd)
	cmd.Flags().StringVar(&host, "host", "", "host name to record in the snapshot (default: this machine's)")
	cmd.Flags().StringVar(&spoolDir, "spool-dir", "", "where to stage packs before upload (default: the system temporary directory)")
	return cmd
}
