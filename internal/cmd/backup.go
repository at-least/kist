package cmd

import (
	"context"
	"fmt"
	"time"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

func newBackupCommand() *cobra.Command {
	var (
		flags    repoFlags
		host     string
		spoolDir string
		parity   int
		gcGrace  time.Duration
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
			ev := event("backup")
			return finish(cmd, ev, flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				snap, handle, err := r.Backup(ctx, args, repo.BackupOptions{
					Host:     host,
					SpoolDir: spoolDir,
					Parity:   parity,
					GCGrace:  gcGrace,
					Warnf:    warnInto(cmd, ev),
				})
				if err != nil {
					return err
				}
				ev.Backup = report.FromBackup(snap, handle)
				if jsonMode(cmd) {
					return nil
				}

				out := cmd.OutOrStdout()
				fmt.Fprintf(out, "snapshot %s\n", handle.Key)
				fmt.Fprintf(out, "  %d files, %d directories, %d symlinks\n", snap.Stats.Files, snap.Stats.Dirs, snap.Stats.Symlinks)
				fmt.Fprintf(out, "  %s read, %s stored in %d new packs\n",
					humanBytes(snap.Stats.Bytes), humanBytes(snap.Stats.BytesStored), snap.Stats.PacksAdded)
				return nil
			}))
		},
	}

	cmd.Flags().DurationVar(&gcGrace, "gc-grace", 0, "grace period prune uses; the backup refuses to commit past it (0: 72h)")
	flags.register(cmd)
	cmd.Flags().StringVar(&host, "host", "", "host name to record in the snapshot (default: this machine's)")
	cmd.Flags().StringVar(&spoolDir, "spool-dir", "", "where to stage packs before upload (default: the system temporary directory)")
	cmd.Flags().IntVar(&parity, "parity", 0, "Reed-Solomon parity shards per pack, out of 16 data shards (0 = none; 2 = 12.5% overhead, repairs up to 2 damaged sixteenths)")
	return cmd
}
