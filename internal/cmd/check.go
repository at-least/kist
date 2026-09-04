package cmd

import (
	"context"
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
)

func newCheckCommand() *cobra.Command {
	var (
		flags    repoFlags
		readData bool
	)

	cmd := &cobra.Command{
		Use:   "check",
		Short: "Verify the repository",
		Long: "Verify that every snapshot resolves to chunks that are actually stored.\n\n" +
			"Without --read-data this reads pack trailers and metadata only, which\n" +
			"catches a missing or truncated pack and any dangling reference. It\n" +
			"cannot catch a bit flipped inside chunk data, because nothing\n" +
			"decrypts that data; --read-data does, at the cost of reading the\n" +
			"whole repository.",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			return flags.withRepository(cmd, func(ctx context.Context, r *repo.Repository) error {
				report, err := r.Check(ctx, repo.CheckOptions{
					ReadData:  readData,
					Progressf: func(format string, args ...any) { fmt.Fprintf(cmd.ErrOrStderr(), format+"\n", args...) },
				})
				if err != nil {
					return err
				}

				out := cmd.OutOrStdout()
				fmt.Fprintf(out, "%d snapshots, %d trees, %d chunks in %d packs\n",
					report.Snapshots, report.Trees, report.Chunks, report.Packs)
				if report.OK() {
					fmt.Fprintln(out, "no problems found")
					return nil
				}
				for _, p := range report.Problems {
					fmt.Fprintf(out, "problem: %s\n", p)
				}
				return fmt.Errorf("%w: %d problems", repo.ErrCheckFailed, len(report.Problems))
			})
		},
	}

	flags.register(cmd)
	cmd.Flags().BoolVar(&readData, "read-data", false, "read and verify every chunk, not just metadata")
	return cmd
}
