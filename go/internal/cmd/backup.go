package cmd

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

// sourceSpecOf classifies the path arguments: an sftp:// or s3:// URL
// names a remote source, anything else is local paths. A URL mixed with
// other paths is rejected by the repository layer with an error that
// says what a remote backup accepts.
func sourceSpecOf(args []string) repo.SourceSpec {
	for _, arg := range args {
		if strings.HasPrefix(arg, "sftp://") || strings.HasPrefix(arg, "s3://") {
			return repo.SourceSpec{URL: arg}
		}
	}
	return repo.SourceSpec{}
}

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
			"reclaims them.\n\n" +
			"A single remote source can be backed up by passing one sftp:// or\n" +
			"s3:// URL as the path. The client reads the source, chunks and\n" +
			"encrypts locally, and uploads: no key material leaves this machine.",
		Args: cobra.MinimumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ev := event("backup")
			return finish(cmd, ev, flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				summary, err := r.Backup(ctx, args, repo.BackupOptions{
					Host:     host,
					SpoolDir: spoolDir,
					Parity:   parity,
					GCGrace:  gcGrace,
					Warnf:    warnInto(cmd, ev),
					Source:   sourceSpecOf(args),
				})
				if err != nil {
					return err
				}
				ev.Backup = report.FromBackup(summary)
				if jsonMode(cmd) {
					return nil
				}

				snap, handle := summary.Snapshot, summary.Handle
				out := cmd.OutOrStdout()
				fmt.Fprintf(out, "snapshot %s\n", handle.Key)
				fmt.Fprintf(out, "  %d files, %d directories, %d symlinks", snap.Stats.Files, snap.Stats.Dirs, snap.Stats.Symlinks)
				if summary.Report.FilesReused > 0 {
					fmt.Fprintf(out, " (%d reused from the previous snapshot)", summary.Report.FilesReused)
				}
				fmt.Fprintf(out, "\n")
				fmt.Fprintf(out, "  %s stored in %d new chunks, %d new packs\n",
					humanBytes(summary.Report.BytesStored), summary.Report.ChunksNew, summary.Report.PacksNew)
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
