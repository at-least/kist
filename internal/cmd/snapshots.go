package cmd

import (
	"context"
	"fmt"
	"strings"
	"text/tabwriter"
	"time"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
)

func newSnapshotsCommand() *cobra.Command {
	var (
		flags  repoFlags
		client string
	)

	cmd := &cobra.Command{
		Use:   "snapshots",
		Short: "List snapshots, oldest first",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			return flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				handles, err := r.Snapshots(ctx, client)
				if err != nil {
					return err
				}
				if len(handles) == 0 {
					fmt.Fprintln(cmd.OutOrStdout(), "no snapshots")
					return nil
				}

				w := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 0, 2, ' ', 0)
				fmt.Fprintln(w, "TIME\tHOST\tFILES\tSIZE\tPATHS\tSNAPSHOT")
				for _, handle := range handles {
					snap, err := r.LoadSnapshot(ctx, handle.Key)
					if err != nil {
						fmt.Fprintf(w, "%s\t?\t?\t?\t?\t%s\n", handle.Time.Format(time.RFC3339), handle.Key)
						warnTo(cmd)("%v", err)
						continue
					}
					fmt.Fprintf(w, "%s\t%s\t%d\t%s\t%s\t%s\n",
						handle.Time.Format(time.RFC3339),
						snap.Host,
						snap.Stats.Files,
						humanBytes(snap.Stats.Bytes),
						strings.Join(snap.Paths, ","),
						handle.Key)
				}
				return w.Flush()
			})
		},
	}

	flags.register(cmd)
	cmd.Flags().StringVar(&client, "client", "", "list only this client's snapshots")
	return cmd
}
