// Package chunker splits file streams into content-defined chunks, so
// that an edit rewrites only the chunks it touches.
//
// The splitter is FastCDC with a 512 KiB minimum, 2 MiB average and 8 MiB
// maximum. Boundaries depend only on content, never on offset, which is
// what makes deduplication survive an insertion at the front of a file.
//
// The parameters are part of the frozen format: change them and every
// chunk of every existing file gets a new boundary, so a repository
// written by one build would deduplicate against nothing written by
// another. TestGoldenBoundaries pins them.
package chunker
