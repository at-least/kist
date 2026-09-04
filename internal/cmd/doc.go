// Package cmd builds the kist command tree.
//
// Commands parse flags, call into internal/repo and render output. They
// hold no repository logic themselves, which keeps the CLI surface
// testable by executing a command with a buffer attached.
package cmd
