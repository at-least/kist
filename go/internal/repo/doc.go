// Package repo ties the layers together: it opens a repository over a
// backend, unwraps the key hierarchy, and drives backup, restore and
// integrity checks.
//
// This is the package the CLI talks to. Command implementations in
// internal/cmd hold no repository logic of their own.
//
// The ordering rule every operation here obeys: a snapshot is written
// only after every pack, tree and index blob it refers to is durably
// stored. Nothing else in a repository is ordered, which is what lets
// several clients back up to one repository with no lock. A client that
// dies mid-backup leaves objects nobody refers to; those cost space until
// prune collects them, and cost correctness nothing.
package repo
