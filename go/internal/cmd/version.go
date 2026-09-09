package cmd

import (
	"fmt"
	"runtime"

	"github.com/spf13/cobra"
)

// version is stamped at link time by the build (see Makefile). The default
// is what an unstamped `go build ./...` or `go run` produces.
var version = "dev"

// Version reports the build version of this binary.
func Version() string { return version }

func newVersionCommand() *cobra.Command {
	return &cobra.Command{
		Use:   "version",
		Short: "Print the kist-go version",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			_, err := fmt.Fprintf(cmd.OutOrStdout(), "kist-go %s %s/%s %s\n",
				version, runtime.GOOS, runtime.GOARCH, runtime.Version())
			return err
		},
	}
}
