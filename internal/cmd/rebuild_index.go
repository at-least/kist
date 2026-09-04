package cmd

import (
	"context"
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

func newRebuildIndexCommand() *cobra.Command {
	var flags repoFlags

	cmd := &cobra.Command{
		Use:   "rebuild-index",
		Short: "Rebuild the chunk index from pack trailers",
		Long: "Discard the cached index and reconstruct it by reading every pack's\n" +
			"trailer.\n\n" +
			"This is always safe, because the index is only ever a cache: every\n" +
			"entry in it can be recovered from the packs themselves. Run it after\n" +
			"a client died mid-backup, or when check reports the index refers to\n" +
			"packs that are not there.",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			ev := event("rebuild_index")
			return finish(cmd, ev, flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				n, err := r.RebuildIndex(ctx)
				if err != nil {
					return err
				}
				ev.Index = &report.IndexResult{Chunks: n}
				if jsonMode(cmd) {
					return nil
				}
				fmt.Fprintf(cmd.OutOrStdout(), "rebuilt the index: %d chunks\n", n)
				return nil
			}))
		},
	}
	flags.register(cmd)
	return cmd
}
