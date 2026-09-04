// Package chunker splits file streams into content-defined chunks so that
// an edit only rewrites the chunks it touches.
//
// The splitter is FastCDC with a 512 KiB minimum, 2 MiB average and 8 MiB
// maximum chunk size. Boundaries depend only on content, never on offset,
// which is what makes deduplication survive insertions and deletions.
package chunker
