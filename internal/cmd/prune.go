package cmd

import (
	"context"
	"fmt"
	"time"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
)

func newPruneCommand() *cobra.Command {
	var (
		flags              repoFlags
		grace              time.Duration
		forgetClientsAfter time.Duration
		dryRun             bool
	)

	cmd := &cobra.Command{
		Use:   "prune",
		Short: "Reclaim packs no snapshot needs",
		Long: "Mark packs no snapshot refers to, and delete packs that have been\n" +
			"marked for longer than the grace period.\n\n" +
			"A pack is deleted only when it has been marked for the whole grace\n" +
			"period, is still unreferenced, and every client that has backed up\n" +
			"recently has started a backup since the mark. Run prune regularly:\n" +
			"the first run marks, a run after the grace period deletes.",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			return flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				report, err := r.Prune(ctx, repo.PruneOptions{
					Grace:              grace,
					ForgetClientsAfter: forgetClientsAfter,
					DryRun:             dryRun,
					Progressf:          warnTo(cmd),
				})
				if err != nil {
					return err
				}
				out := cmd.OutOrStdout()
				would := ""
				if dryRun {
					would = "would have "
				}
				fmt.Fprintf(out, "%d packs stored, %d live\n", report.Stored, report.Live)
				for _, id := range report.Marked {
					fmt.Fprintf(out, "%smarked %s\n", would, id)
				}
				for _, id := range report.Unmarked {
					fmt.Fprintf(out, "%sunmarked %s\n", would, id)
				}
				for _, h := range report.Held {
					fmt.Fprintf(out, "held %s: %s\n", h.Pack, h.Reason)
				}
				for _, id := range report.Locked {
					fmt.Fprintf(out, "locked %s: retained by the storage, not reclaimed\n", id)
				}
				for _, id := range report.Deleted {
					fmt.Fprintf(out, "%sdeleted %s\n", would, id)
				}
				fmt.Fprintf(out, "%smarked %d, unmarked %d, held %d, deleted %d (%s reclaimed)\n",
					would, len(report.Marked), len(report.Unmarked), len(report.Held), len(report.Deleted), humanBytes(report.BytesReclaimed))
				return nil
			})
		},
	}

	flags.register(cmd)
	f := cmd.Flags()
	f.DurationVar(&grace, "grace", repo.DefaultGrace, "how long a pack stays marked before it can be deleted")
	f.DurationVar(&forgetClientsAfter, "forget-clients-after", 0, "stop waiting for a client that has not backed up in this long (default 10x grace)")
	f.BoolVar(&dryRun, "dry-run", false, "report what would happen and change nothing")
	return cmd
}
