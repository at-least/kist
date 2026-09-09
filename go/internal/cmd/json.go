package cmd

import (
	"encoding/json"
	"fmt"
	"time"

	"github.com/spf13/cobra"

	"github.com/at-least/kist/internal/report"
)

// JSONFlag is the persistent flag that switches every command's output
// to one JSON object on stdout. Warnings and progress stay on stderr,
// and a failing command still exits non-zero: the object says what
// happened, the exit code says whether it was good.
const JSONFlag = "json"

func jsonMode(cmd *cobra.Command) bool {
	v, err := cmd.Flags().GetBool(JSONFlag)
	return err == nil && v
}

// event starts a report for a command.
func event(kind string) *report.Event {
	return &report.Event{Kind: kind, Started: time.Now()}
}

// finish completes the event with the command's error and, in JSON
// mode, prints it. Text mode prints nothing here; the command already
// did. Either way the error is returned for the exit code.
func finish(cmd *cobra.Command, ev *report.Event, err error) error {
	ev.Finished = time.Now()
	ev.OK = err == nil
	if err != nil {
		ev.Error = err.Error()
	}
	if !jsonMode(cmd) {
		return err
	}
	enc := json.NewEncoder(cmd.OutOrStdout())
	if encErr := enc.Encode(ev); encErr != nil {
		return fmt.Errorf("write JSON: %w", encErr)
	}
	return err
}

// warnInto returns a warning sink that writes to stderr and records
// into the event, so that JSON output carries what text output said.
func warnInto(cmd *cobra.Command, ev *report.Event) func(string, ...any) {
	stderr := warnTo(cmd)
	return func(format string, args ...any) {
		ev.Warnings = append(ev.Warnings, fmt.Sprintf(format, args...))
		stderr(format, args...)
	}
}

// emitList prints a JSON array, for commands whose output is a list.
func emitList(cmd *cobra.Command, v any) error {
	if err := json.NewEncoder(cmd.OutOrStdout()).Encode(v); err != nil {
		return fmt.Errorf("write JSON: %w", err)
	}
	return nil
}
