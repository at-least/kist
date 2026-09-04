package cmd

import (
	"context"
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
)

func newForgetCommand() *cobra.Command {
	var (
		flags  repoFlags
		policy repo.RetentionPolicy
		client string
		dryRun bool
	)

	cmd := &cobra.Command{
		Use:   "forget [snapshot...]",
		Short: "Remove snapshots by retention policy or by name",
		Long: "Remove snapshots. The --keep-* rules are applied per client, and a\n" +
			"snapshot kept by any rule is kept. Snapshots named on the command\n" +
			"line are removed whatever the rules say.\n\n" +
			"Only the snapshot objects are removed; the data they referenced\n" +
			"stays until prune finds it unreferenced. With no rule and no\n" +
			"snapshot named, forget refuses to run rather than forget everything.",
		RunE: func(cmd *cobra.Command, args []string) error {
			return flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				result, err := r.Forget(ctx, repo.ForgetOptions{
					Policy:   policy,
					ClientID: client,
					Keys:     args,
					DryRun:   dryRun,
				})
				if err != nil {
					return err
				}
				verb := "removed"
				if dryRun {
					verb = "would remove"
				}
				out := cmd.OutOrStdout()
				for _, h := range result.Removed {
					fmt.Fprintf(out, "%s %s\n", verb, h.Key)
				}
				fmt.Fprintf(out, "%s %d snapshot(s), kept %d\n", verb, len(result.Removed), len(result.Kept))
				return nil
			})
		},
	}

	flags.register(cmd)
	f := cmd.Flags()
	f.IntVar(&policy.Last, "keep-last", 0, "keep the N most recent snapshots")
	f.IntVar(&policy.Hourly, "keep-hourly", 0, "keep the newest snapshot of each of the last N hours that have one")
	f.IntVar(&policy.Daily, "keep-daily", 0, "keep the newest snapshot of each of the last N days that have one")
	f.IntVar(&policy.Weekly, "keep-weekly", 0, "keep the newest snapshot of each of the last N ISO weeks that have one")
	f.IntVar(&policy.Monthly, "keep-monthly", 0, "keep the newest snapshot of each of the last N months that have one")
	f.IntVar(&policy.Yearly, "keep-yearly", 0, "keep the newest snapshot of each of the last N years that have one")
	f.DurationVar(&policy.Within, "keep-within", 0, "keep every snapshot newer than this age (e.g. 72h)")
	f.StringVar(&client, "client", "", "apply the rules to this client's snapshots only")
	f.BoolVar(&dryRun, "dry-run", false, "report what would be removed and remove nothing")
	return cmd
}
