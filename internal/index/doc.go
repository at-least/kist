// Package index maps chunk IDs to their location, (packID, offset, length).
//
// Index blobs are a cache, never a source of truth: every entry can be
// recovered by reading the trailer of each pack. That property is what
// keeps the format repairable and lets rebuild-index be a safe operation.
package index
