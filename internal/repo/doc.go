// Package repo ties the layers together: it opens a repository over a
// backend, unwraps the key hierarchy, and drives backup, restore and
// integrity checks.
//
// This is the package the CLI talks to. Command implementations in
// internal/cmd hold no repository logic of their own.
package repo
