package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

func newInitCommand() *cobra.Command {
	var flags repoFlags

	cmd := &cobra.Command{
		Use:   "init",
		Short: "Create a repository",
		Long: "Create a repository at --repo.\n\n" +
			"The password is asked for twice and cannot be recovered: it is the\n" +
			"only thing standing between the repository and whoever can read it,\n" +
			"and kist keeps no copy of it anywhere.",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			location, err := flags.location()
			if err != nil {
				return err
			}
			opts, err := flags.options(cmd, true)
			if err != nil {
				return err
			}

			b, err := openBackend(cmd.Context(), location, true)
			if err != nil {
				return err
			}

			r, err := repo.Init(cmd.Context(), b, opts)
			if err != nil {
				if cerr := b.Close(); cerr != nil {
					warnTo(cmd)("closing the backend: %v", cerr)
				}
				return err
			}
			defer func() {
				if cerr := r.Close(); cerr != nil {
					warnTo(cmd)("closing the repository: %v", cerr)
				}
			}()

			if jsonMode(cmd) {
				ev := event("init")
				ev.Init = &report.InitResult{Location: b.Location(), ClientID: r.ClientID()}
				return finish(cmd, ev, nil)
			}
			fmt.Fprintf(cmd.OutOrStdout(), "created repository at %s\n", b.Location())
			fmt.Fprintf(cmd.OutOrStdout(), "client %s\n", r.ClientID())
			return nil
		},
	}
	flags.register(cmd)
	return cmd
}
