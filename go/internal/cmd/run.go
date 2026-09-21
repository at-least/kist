package cmd

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/spf13/cobra"

	"errors"
	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/config"
	kistrun "github.com/at-least/kist/internal/run"
)

func newRunCommand() *cobra.Command {
	var (
		configPath string
		once       bool
	)

	cmd := &cobra.Command{
		Use:   "run --config kist.toml",
		Short: "Run the jobs in a configuration file on their schedules",
		Long: "Read a configuration file and run its [[backup]] jobs and its [prune]\n" +
			"job when their cron schedules say so, until interrupted. With --once,\n" +
			"run every job immediately, in order, and exit.\n\n" +
			"Put the [[backup]] jobs in a configuration on the machine being backed\n" +
			"up, with backup credentials, and the [prune] job in another on a\n" +
			"maintenance host, with credentials that can delete. One configuration\n" +
			"holding both works, and means the backed-up machine can delete its\n" +
			"own backups.",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			cfg, err := config.Load(configPath)
			if err != nil {
				return err
			}
			password, err := cfg.Repository.Password(PasswordEnv)
			if err != nil {
				return err
			}
			if cfg.MixesRoles() {
				warnTo(cmd)("this configuration runs both backups and prune: the machine holds credentials that can delete its own backups")
			}

			logf := func(format string, args ...any) {
				fmt.Fprintf(cmd.ErrOrStderr(), time.Now().Format("2006-01-02T15:04:05Z07:00")+" "+format+"\n", args...)
			}
			r := &kistrun.Runner{
				Config:   cfg,
				Password: password,
				Logf:     logf,
				OpenBackend: func(ctx context.Context, location string) (backend.Backend, error) {
					return openBackend(ctx, location, false)
				},
			}

			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()
			if once {
				err := r.Once(ctx)
				// Same contract as serve below: an interrupt is a clean
				// stop, not a failed unit -- a supervisor must not see a
				// plain Ctrl-C as a crash just because --once was set.
				// Keyed on the classified error, not ctx.Err(): a job
				// that failed for real before the signal must still exit
				// non-zero.
				if err == nil || errors.Is(err, context.Canceled) {
					if ctx.Err() != nil {
						logf("stopping")
						return nil
					}
				}
				return err
			}
			err = r.Serve(ctx)
			if ctx.Err() != nil {
				logf("stopping")
				return nil
			}
			return err
		},
	}

	cmd.Flags().StringVar(&configPath, "config", "", "configuration file (TOML)")
	if err := cmd.MarkFlagRequired("config"); err != nil {
		panic(err) // the flag was defined one line up; this cannot fail
	}
	cmd.Flags().BoolVar(&once, "once", false, "run every job now, once, and exit")
	return cmd
}
