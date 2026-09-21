package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

func newInitCommand() *cobra.Command {
	var (
		flags      repoFlags
		chunkerMin uint32
		chunkerAvg uint32
		chunkerMax uint32
	)

	cmd := &cobra.Command{
		Use:   "init",
		Short: "Create a repository",
		Long: "Create a repository at --repo.\n\n" +
			"The password is asked for twice and cannot be recovered: it is the\n" +
			"only thing standing between the repository and whoever can read it,\n" +
			"and kist keeps no copy of it anywhere.",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			// The event exists from the start so a failing command still
			// emits the object (json.go's contract): the object says what
			// happened, the exit code says whether it was good.
			ev := event("init")
			fail := func(err error) error { return finish(cmd, ev, err) }
			location, err := flags.location()
			if err != nil {
				return fail(err)
			}
			opts, err := flags.options(cmd, true)
			if err != nil {
				return fail(err)
			}
			opts.Chunker = &repo.ChunkerParams{MinSize: chunkerMin, AvgSize: chunkerAvg, MaxSize: chunkerMax}

			b, err := openBackend(cmd.Context(), location, true)
			if err != nil {
				return fail(err)
			}

			r, err := repo.Init(cmd.Context(), b, opts)
			if err != nil {
				if cerr := b.Close(); cerr != nil {
					warnTo(cmd)("closing the backend: %v", cerr)
				}
				return fail(err)
			}
			defer func() {
				if cerr := r.Close(); cerr != nil {
					warnTo(cmd)("closing the repository: %v", cerr)
				}
			}()

			if jsonMode(cmd) {
				ev.Init = &report.InitResult{Location: b.Location(), ClientID: r.ClientID()}
				return finish(cmd, ev, nil)
			}
			fmt.Fprintf(cmd.OutOrStdout(), "created repository at %s\n", b.Location())
			fmt.Fprintf(cmd.OutOrStdout(), "client %s\n", r.ClientID())
			return nil
		},
	}
	cmd.Flags().Uint32Var(&chunkerMin, "chunker-min", 512<<10, "FastCDC minimum chunk size in bytes")
	cmd.Flags().Uint32Var(&chunkerAvg, "chunker-avg", 2<<20, "FastCDC average chunk size in bytes")
	cmd.Flags().Uint32Var(&chunkerMax, "chunker-max", 8<<20, "FastCDC maximum chunk size in bytes")
	flags.register(cmd)
	return cmd
}
