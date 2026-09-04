package cmd

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

// NewRootCommand builds the kist command tree. Tests use it to execute a
// command with buffers attached instead of the process streams.
func NewRootCommand() *cobra.Command {
	root := &cobra.Command{
		Use:   "kist",
		Short: "Deduplicating, encrypted, multi-client backups",
		Long: "kist backs up to object storage with client-side encryption,\n" +
			"content-defined deduplication and lock-free maintenance.",
		SilenceUsage:  true,
		SilenceErrors: true,
	}

	root.AddCommand(newVersionCommand())

	return root
}

// Execute runs the command tree against the process streams. It is the
// only place in the binary that terminates the process.
func Execute() {
	if err := NewRootCommand().Execute(); err != nil {
		fmt.Fprintf(os.Stderr, "kist: %v\n", err)
		os.Exit(1)
	}
}
