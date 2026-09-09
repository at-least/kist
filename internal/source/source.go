package source

import (
	"context"
	"io"
	"slices"
)

// Kind says what a SourceItem is.
type Kind uint8

// The item kinds a source may report.
const (
	// KindDir is a subdirectory. It carries no metadata: a listing's
	// directories are names, and only S3 directories even have a
	// discoverable mtime (none of the three source families records one).
	KindDir Kind = iota
	// KindFile is a regular file -- or, for the remote sources, anything
	// that opens and reads like one.
	KindFile
	// KindSymlink is a symbolic link. Only the local source reports it:
	// SFTP's listing has no readlink, and S3 has no links at all.
	KindSymlink
)

// SourceItemKind is what a listed item is and what it can say about
// itself. Which fields mean something depends on Kind, and which of those
// are trustworthy depends on the source's MetaKind -- the format's
// metadata union starts here, at the source.
//
// The name mirrors the Rust implementation's SourceItemKind on purpose:
// the two are kept field-for-field equivalent.
//
//nolint:revive // source.SourceItemKind reads better than source.ItemKind here
type SourceItemKind struct {
	// Kind discriminates the fields below.
	Kind Kind

	// Size and MTimeNs describe a file. MTimeNs is Unix nanoseconds;
	// remote sources only have second precision (or coarser), and the
	// value is the source's claim, not a proof.
	Size    uint64
	MTimeNs int64

	// Etag is a content fingerprint the source computed and vouches for
	// (an S3 ETag, for instance), and Vern is the source object's version
	// ID where the storage has versioning. Both are optional, and only an
	// s3-kind source provides them.
	Etag []byte
	Vern []byte

	// Target is a symlink's destination, as the link spells it.
	Target []byte
}

// PosixMeta is the full local metadata a posix entry records. It travels
// separately from SourceItemKind because it exists only for the local
// source: a remote item has no inode, and a format entry must not pretend
// one exists (an absent field is "the source did not say", never 0).
type PosixMeta struct {
	// Mode is the Go fs.FileMode bit layout, the same encoding a tree
	// entry stores.
	Mode uint32

	// UID and GID are the owning user and group; 0 is root, a real value.
	UID uint32
	GID uint32

	// MTimeNs and CTimeNs are Unix nanoseconds. CTimeNs is 0 when the
	// platform has none, which is also how a tree entry records that.
	MTimeNs int64
	CTimeNs int64

	// Inode and Dev identify the file for hard-link detection, and NLink
	// is the link count. All three are 0 where the platform does not
	// expose them.
	Inode uint64
	Dev   uint64
	NLink uint64
}

// A SourceItem is one listed directory entry: a single path component,
// by the source's own bytes, plus what the source can say about it.
//
// Items are deliberately lean -- a name and a kind, with the full stat
// only where the source is local. A listing of a million entries is
// resident for the whole walk (the large-repository memory gate), and
// per-item metadata the walker will not use would be paid for nothing;
// remote items carry no Posix pointer at all.
//
//nolint:revive // source.SourceItem reads better than source.Item here
type SourceItem struct {
	// Name is one path component, as raw bytes.
	Name []byte

	// Kind is the item's type and its self-reported metadata.
	Kind SourceItemKind

	// Posix is the full local metadata, captured when the item was
	// listed. Only the local source sets it.
	Posix *PosixMeta
}

// A Source is one backup origin: something that can list directories and
// stream files. Names are byte strings; dir and file arguments are
// slash-separated relative paths with no leading or trailing slash, and
// an empty dir means the source's root.
type Source interface {
	// Locator is the opaque string a snapshot root records for this
	// source: a local absolute path, an sftp://host/path or an
	// s3://bucket/prefix URL.
	Locator() []byte

	// MetaKind is the tree.MetaKind value naming the metadata family this
	// source can prove. It decides which entry fields the walker records
	// and which fast path (if any) the next backup may take.
	MetaKind() uint8

	// List returns one directory level's entries, sorted by name bytes.
	// A source whose root names a single file lists that file itself,
	// which is what makes the walker's file-root detection work; a
	// directory lists its children. Entries that vanish mid-listing are
	// skipped, the same tolerance the walk itself applies.
	List(ctx context.Context, dir []byte) ([]SourceItem, error)

	// Read opens a file for streaming. The caller closes it.
	Read(ctx context.Context, file []byte) (io.ReadCloser, error)

	// Close releases the source's connection, if it holds one.
	Close() error
}

// A LocalPather is a Source whose items also exist on the local
// filesystem, reachable at the path the locator names. The walker reads
// xattrs and per-entry stat details through it; remote sources do not
// implement it.
type LocalPather interface {
	// LocalPath maps a relative path back onto the local filesystem. The
	// bool reports whether the source is local at all.
	LocalPath(rel []byte) (string, bool)
}

// LocalPathOf returns the local path of rel when src is a local source.
func LocalPathOf(src Source, rel []byte) (string, bool) {
	p, ok := src.(LocalPather)
	if !ok {
		return "", false
	}
	return p.LocalPath(rel)
}

// JoinRel joins a directory's relative path and one entry name with a
// slash. Source paths are always slash-separated bytes, whatever the
// platform's separator is.
func JoinRel(dir, name []byte) []byte {
	out := make([]byte, 0, len(dir)+1+len(name))
	out = append(out, dir...)
	if len(dir) > 0 {
		out = append(out, '/')
	}
	out = append(out, name...)
	return out
}

// makeFileItem builds a listed file item. Etag and vern are copied, so
// callers may pass slices that alias their own storage.
func makeFileItem(name string, size uint64, mtimeNs int64, etag, vern []byte) SourceItem {
	item := SourceItem{
		Name: []byte(name),
		Kind: SourceItemKind{
			Kind:    KindFile,
			Size:    size,
			MTimeNs: mtimeNs,
		},
	}
	if len(etag) > 0 {
		item.Kind.Etag = slices.Clone(etag)
	}
	if len(vern) > 0 {
		item.Kind.Vern = slices.Clone(vern)
	}
	return item
}
