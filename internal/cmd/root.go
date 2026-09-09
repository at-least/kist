package cmd

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

// platformCommands are registered by files with build constraints:
// mount needs FUSE, which Windows does not have.
var platformCommands []func() *cobra.Command

// NewRootCommand builds the kist-go command tree. Tests use it to execute a
// command with buffers attached instead of the process streams.
func NewRootCommand() *cobra.Command {
	root := &cobra.Command{
		Use:   "kist-go",
		Short: "Deduplicating, encrypted, multi-client backups",
		Long: "kist-go backs up to object storage with client-side encryption,\n" +
			"content-defined deduplication and lock-free maintenance.",
		SilenceUsage:  true,
		SilenceErrors: true,
	}

	root.PersistentFlags().Bool(JSONFlag, false, "print one JSON object (or array) on stdout instead of text")

	root.AddCommand(
		newInitCommand(),
		newBackupCommand(),
		newSnapshotsCommand(),
		newRestoreCommand(),
		newCheckCommand(),
		newForgetCommand(),
		newPruneCommand(),
		newRunCommand(),
		newRebuildIndexCommand(),
		newVersionCommand(),
	)
	for _, newCommand := range platformCommands {
		root.AddCommand(newCommand())
	}

	return root
}

// Execute runs the command tree against the process streams. It is the
// only place in the binary that terminates the process.
func Execute() {
	if err := NewRootCommand().Execute(); err != nil {
		fmt.Fprintf(os.Stderr, "kist-go: %v\n", err)
		os.Exit(1)
	}
}
