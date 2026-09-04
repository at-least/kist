// Package tree implements directory objects: content-addressed listings of
// names, metadata and child references.
//
// Because a tree is named by the hash of its contents, an unchanged
// subtree is reused whole across snapshots, so an incremental backup
// writes only the trees on the path from a changed file to the root.
package tree
