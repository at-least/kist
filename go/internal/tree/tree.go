package tree

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io/fs"
	"slices"

	"github.com/fxamacker/cbor/v2"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

// Version is the tree object schema version.
const Version = 3

// Prefix is the repository prefix tree objects live under.
const Prefix = "trees/"

// ReplicaSuffix is appended to a tree's key for its .r1 replica, stored
// when the repository was created with replicas=1 (docs/format.md
// §13.5). The replica's bytes are identical to the primary's; it is a
// second copy of one object, not a second object.
const ReplicaSuffix = ".r1"

// TouchPrefix is the repository prefix of a tree's revival signal. The
// 8-byte object is written with an OVERWRITING Put on every backup that
// reuses the tree: the refreshed backend mtime IS the signal
// (docs/format.md §13.1). A PutIfAbsent here would silently keep the
// old mtime on the second reuse and open the deletion race the overwrite
// exists to close.
const TouchPrefix = "touch/"

// TouchMagic is the entire content of a touch object: 8 fixed bytes that
// carry no information.
var TouchMagic = []byte("KISTTC3\n")

// MaxNodesPerTree is how many entries one tree object holds before it is
// split into a chain linked by Prev. A directory that has not changed in
// its earlier segments keeps their IDs, so a huge directory does not pay
// a rewrite of everything.
const MaxNodesPerTree = 10_000

// MaxInlineChunks is the chunk-list length at which a file switches to
// indirect storage: the list is encoded as a ChunkList and chunked like
// ordinary data, and the tree entry points at those chunks.
const MaxInlineChunks = 256

// Key returns the repository key a tree is stored under.
func Key(id crypto.ID) string { return Prefix + id.String() }

// ReplicaKey returns the repository key of a tree's .r1 replica.
func ReplicaKey(id crypto.ID) string { return Prefix + id.String() + ReplicaSuffix }

// TouchKey returns the repository key of a tree's revival signal.
func TouchKey(id crypto.ID) string { return TouchPrefix + id.String() }

// ErrCorrupt means a tree object is structurally invalid.
var ErrCorrupt = errors.New("tree object is corrupt")

// NodeType distinguishes what an entry describes. Only these three are
// backed up; sockets, FIFOs and device nodes are skipped with a warning,
// because restoring them faithfully needs privileges a restore should not
// assume and their contents are never what the user wanted saved.
type NodeType uint8

// The node types a tree entry may have.
const (
	TypeFile NodeType = iota
	TypeDir
	TypeSymlink
)

// ContentType says what an entry's Chunks list holds. Direct: the chunks
// of the file's contents. Indirect: the chunks of an encoded ChunkList,
// which names the file's content chunks. Indirect exists so that a tree
// entry for a 100 TiB file is not a gigabyte of IDs.
type ContentType uint8

// Content types.
const (
	ContentDirect ContentType = iota
	ContentIndirect
)

// A MetaKind says which metadata family an entry carries. v3 entries are
// a kind union: a source records what it can prove about a file, and the
// per-kind rules (Validate, docs/format.md §8.1) pin which fields are
// required, optional, or must be absent for each kind.
type MetaKind uint8

// MaxEtagVernBytes caps an s3 entry's etag and vern each (§8.1).
const MaxEtagVernBytes = 1024

// The metadata kinds.
const (
	// MetaPOSIX is a local filesystem source: the kernel maintains mode,
	// ownership and times, so they are required. UID 0 is root -- a real
	// value, not "not recorded".
	MetaPOSIX MetaKind = iota
	// MetaSFTP is an SFTP source: only the modification time is required;
	// mode and ownership are optional (absent means the source did not
	// say).
	MetaSFTP
	// MetaS3 is an object-storage source: mtime, etag and vern are
	// optional; mode/uid/gid have no meaning and are never recorded.
	MetaS3
	// MetaGeneric is the conservative kind for future sources: an mtime
	// and nothing else.
	MetaGeneric
)

// Valid reports whether v is a metadata kind this format defines.
func (k MetaKind) Valid() bool { return k <= MetaGeneric }

// Xattrs are extended attributes: byte-string keys and values, kept in
// canonical (sorted) order on the wire. Go maps cannot key on []byte, so
// this carries cbor.Marshaler/cbor.Unmarshaler itself; the v1 map[string]
// form is gone because xattr names are not required to be UTF-8.
type Xattrs []Xattr

// Xattr is one extended attribute.
type Xattr struct {
	Name  []byte
	Value []byte
}

// MarshalCBOR renders the attributes as a CBOR map with byte-string keys,
// sorted bytewise on the encoded keys (RFC 8949 §4.2.1).
func (x Xattrs) MarshalCBOR() ([]byte, error) {
	pairs := make([][2][]byte, 0, len(x))
	for _, kv := range x {
		pairs = append(pairs, [2][]byte{kv.Name, kv.Value})
	}
	slices.SortFunc(pairs, func(a, b [2][]byte) int { return bytes.Compare(a[0], b[0]) })
	var buf bytes.Buffer
	writeHead(&buf, 5, uint64(len(pairs)))
	for _, kv := range pairs {
		writeHead(&buf, 2, uint64(len(kv[0])))
		buf.Write(kv[0])
		writeHead(&buf, 2, uint64(len(kv[1])))
		buf.Write(kv[1])
	}
	return buf.Bytes(), nil
}

// writeHead emits a definite-length CBOR head with the shortest form.
func writeHead(buf *bytes.Buffer, major byte, length uint64) {
	switch {
	case length < 24:
		buf.WriteByte(major<<5 | byte(length))
	case length <= 0xff:
		buf.WriteByte(major<<5 | 24)
		buf.WriteByte(byte(length))
	case length <= 0xffff:
		buf.WriteByte(major<<5 | 25)
		buf.Write(binary.BigEndian.AppendUint16(nil, uint16(length)))
	case length <= 0xffffffff:
		buf.WriteByte(major<<5 | 26)
		buf.Write(binary.BigEndian.AppendUint32(nil, uint32(length)))
	default:
		buf.WriteByte(major<<5 | 27)
		buf.Write(binary.BigEndian.AppendUint64(nil, length))
	}
}

// UnmarshalCBOR decodes the map form above. It is not a general CBOR
// parser: a tree object is authenticated, so malformed input here means
// a broken writer.
func (x *Xattrs) UnmarshalCBOR(data []byte) error {
	pos := 0
	readHead := func() (major byte, length uint64, err error) {
		if pos >= len(data) {
			return 0, 0, fmt.Errorf("xattrs: truncated head")
		}
		b := data[pos]
		pos++
		major = b >> 5
		switch info := b & 0x1f; {
		case info < 24:
			return major, uint64(info), nil
		case info == 24:
			if pos >= len(data) {
				return 0, 0, fmt.Errorf("xattrs: truncated head")
			}
			v := data[pos]
			pos++
			return major, uint64(v), nil
		case info == 25:
			if pos+2 > len(data) {
				return 0, 0, fmt.Errorf("xattrs: truncated head")
			}
			v := uint64(data[pos])<<8 | uint64(data[pos+1])
			pos += 2
			return major, v, nil
		case info == 26:
			if pos+4 > len(data) {
				return 0, 0, fmt.Errorf("xattrs: truncated head")
			}
			v := uint64(data[pos])<<24 | uint64(data[pos+1])<<16 | uint64(data[pos+2])<<8 | uint64(data[pos+3])
			pos += 4
			return major, v, nil
		default:
			return 0, 0, fmt.Errorf("xattrs: indefinite or unsupported length")
		}
	}
	readBytes := func() ([]byte, error) {
		major, length, err := readHead()
		if err != nil {
			return nil, err
		}
		if major != 2 {
			return nil, fmt.Errorf("xattrs: expected byte string")
		}
		if pos+int(length) > len(data) { //nolint:gosec // length bounded by the input
			return nil, fmt.Errorf("xattrs: truncated byte string")
		}
		b := slices.Clone(data[pos : pos+int(length)]) //nolint:gosec // see above
		pos += int(length)                             //nolint:gosec // see above
		return b, nil
	}

	major, n, err := readHead()
	if err != nil {
		return err
	}
	if major != 5 {
		return fmt.Errorf("xattrs: not a map")
	}
	out := make(Xattrs, 0, n)
	for i := uint64(0); i < n; i++ {
		name, err := readBytes()
		if err != nil {
			return err
		}
		value, err := readBytes()
		if err != nil {
			return err
		}
		// format.md §4: duplicate map keys are a decode rejection. The
		// outer decoder's DupMapKey rule cannot see inside this
		// RawMessage, so the hand parser rejects them itself.
		if pos := slices.IndexFunc(out, func(e Xattr) bool {
			return bytes.Equal(e.Name, name)
		}); pos >= 0 {
			return fmt.Errorf("%w: duplicate xattr key %q", ErrCorrupt, name)
		}
		out = append(out, Xattr{Name: name, Value: value})
	}
	if pos != len(data) {
		return fmt.Errorf("xattrs: trailing bytes")
	}
	*x = out
	return nil
}

// An Entry is one name in a directory.
//
// v3 metadata fields are POINTERS: nil means the source did not record
// the field, and a nil field is absent on the wire. That is the v3 kind
// union -- an S3 object has no mode, and "no mode" must not encode as
// mode 0. A non-nil pointer to zero is a real zero (uid 0 is root) and
// is encoded. The field order below is the spec table
// (docs/format.md §4.1); it is the wire order and it feeds every
// tree ID.
type Entry struct {
	Name []byte `cbor:"n"`
	Type uint8  `cbor:"t"` // NodeType

	// MetaKind: which metadata family the fields below belong to.
	MetaKind uint8 `cbor:"mk"` // MetaKind

	// Size is the file's length in bytes; zero for other types.
	Size uint64 `cbor:"size,omitempty"`

	// Target is a symlink's destination.
	Target []byte `cbor:"target,omitempty"`

	// Chunks lists a file's content chunks in order (inline up to
	// MaxInlineChunks), or the chunks of its encoded ChunkList when
	// ContentType is indirect.
	Chunks []crypto.ID `cbor:"chunks,omitempty"`

	// ContentType: 0 (absent) direct, 1 indirect.
	ContentType uint8 `cbor:"ct,omitempty"`

	// Subtree is a directory's tree ID: the LAST segment when the
	// directory spans several trees.
	Subtree *crypto.ID `cbor:"tree,omitempty"`

	// Mode carries permission and type bits for posix and sftp sources.
	Mode *uint32 `cbor:"mode,omitempty"`
	UID  *uint32 `cbor:"uid,omitempty"`
	GID  *uint32 `cbor:"gid,omitempty"`

	// MTimeNs and CTimeNs are Unix nanoseconds. CTimeNs is the posix
	// fast-path signal: the kernel updates it on every write and it
	// cannot be set by a user, so a copy that preserves mtime is still
	// caught. Remote sources have second precision (the nanoseconds are
	// zero).
	MTimeNs *int64 `cbor:"mtime,omitempty"`
	CTimeNs *int64 `cbor:"ctime,omitempty"`

	// Device and Inode identify a hard link. They are recorded only when
	// Links is above one, and are used by restore to recreate the link
	// rather than a second copy of the data.
	Device *uint64 `cbor:"dev,omitempty"`
	Inode  *uint64 `cbor:"ino,omitempty"`
	Links  *uint64 `cbor:"nlink,omitempty"`

	// Xattrs are extended attributes. M1-era restores reported what they
	// did not apply rather than pretending they did; applying them is
	// later work, but the format records them from day one.
	Xattrs Xattrs `cbor:"xattrs,omitempty"`

	// Etag is a content fingerprint the source computed and vouches for
	// (an S3 ETag, for instance); Vern is the source object's version ID.
	// Both are what the next backup's fast path compares
	// (docs/format.md §8.2).
	Etag []byte `cbor:"etag,omitempty"`
	Vern []byte `cbor:"vern,omitempty"`
}

// entryWire is Entry with Xattrs as a pre-encoded message, so the field
// is really absent (not an empty map) when there are none. A nil
// RawMessage is a nil slice: omitempty drops it. The field order is the
// spec table: n, t, mk, size, target, ct, chunks, tree, mode, uid, gid,
// mtime, ctime, dev, ino, nlink, xattrs, etag, vern.
type entryWire struct {
	Name        []byte          `cbor:"n"`
	Type        uint8           `cbor:"t"`
	MetaKind    uint8           `cbor:"mk"`
	Size        uint64          `cbor:"size,omitempty"`
	Target      []byte          `cbor:"target,omitempty"`
	ContentType uint8           `cbor:"ct,omitempty"`
	Chunks      []crypto.ID     `cbor:"chunks,omitempty"`
	Subtree     *crypto.ID      `cbor:"tree,omitempty"`
	Mode        *uint32         `cbor:"mode,omitempty"`
	UID         *uint32         `cbor:"uid,omitempty"`
	GID         *uint32         `cbor:"gid,omitempty"`
	MTimeNs     *int64          `cbor:"mtime,omitempty"`
	CTimeNs     *int64          `cbor:"ctime,omitempty"`
	Device      *uint64         `cbor:"dev,omitempty"`
	Inode       *uint64         `cbor:"ino,omitempty"`
	Links       *uint64         `cbor:"nlink,omitempty"`
	Xattrs      cbor.RawMessage `cbor:"xattrs,omitempty"`
	Etag        []byte          `cbor:"etag,omitempty"`
	Vern        []byte          `cbor:"vern,omitempty"`
}

// MarshalCBOR renders the entry, omitting an empty Xattrs entirely.
func (e Entry) MarshalCBOR() ([]byte, error) {
	w := entryWire{
		Name: e.Name, Type: e.Type, MetaKind: e.MetaKind, Size: e.Size, Target: e.Target,
		Chunks: e.Chunks, ContentType: e.ContentType, Subtree: e.Subtree,
		Mode: e.Mode, UID: e.UID, GID: e.GID,
		MTimeNs: e.MTimeNs, CTimeNs: e.CTimeNs,
		Device: e.Device, Inode: e.Inode, Links: e.Links,
		Etag: e.Etag, Vern: e.Vern,
	}
	if len(e.Xattrs) > 0 {
		raw, err := e.Xattrs.MarshalCBOR()
		if err != nil {
			return nil, err
		}
		w.Xattrs = cbor.RawMessage(raw)
	}
	return crypto.Marshal(&w)
}

// UnmarshalCBOR decodes the entry, tolerating a missing Xattrs. A
// decoded all-zero subtree is normalised to nil: on the wire "tree" and
// "no tree" both mean no subtree, and a zero ID is never a valid
// content address.
func (e *Entry) UnmarshalCBOR(data []byte) error {
	var w entryWire
	if err := crypto.Unmarshal(data, &w); err != nil {
		return err
	}
	if w.Subtree != nil && w.Subtree.IsZero() {
		w.Subtree = nil
	}
	*e = Entry{
		Name: w.Name, Type: w.Type, MetaKind: w.MetaKind, Size: w.Size, Target: w.Target,
		Chunks: w.Chunks, ContentType: w.ContentType, Subtree: w.Subtree,
		Mode: w.Mode, UID: w.UID, GID: w.GID,
		MTimeNs: w.MTimeNs, CTimeNs: w.CTimeNs,
		Device: w.Device, Inode: w.Inode, Links: w.Links,
		Etag: w.Etag, Vern: w.Vern,
	}
	if len(w.Xattrs) > 0 {
		var x Xattrs
		if err := x.UnmarshalCBOR(w.Xattrs); err != nil {
			return err
		}
		e.Xattrs = x
	}
	return nil
}

// FileMode returns the entry's permission and type bits, or 0 when the
// source did not record a mode.
func (e Entry) FileMode() fs.FileMode {
	if e.Mode == nil {
		return 0
	}
	return fs.FileMode(*e.Mode)
}

// A Tree is one segment of a directory.
type Tree struct {
	Version uint64  `cbor:"v"`
	Entries []Entry `cbor:"entries"`

	// Prev names the previous segment when a directory was split;
	// readers walk it backwards to collect every segment.
	Prev *crypto.ID `cbor:"prev,omitempty"`
}

// New builds a tree from entries, sorting them into canonical order.
//
// Sorting is what makes a directory's encoding a function of its
// contents: two clients that walk the same directory in different orders
// must produce the same tree ID or deduplication stops at the first
// directory.
func New(entries []Entry) *Tree {
	sorted := slices.Clone(entries)
	slices.SortFunc(sorted, func(a, b Entry) int { return bytes.Compare(a.Name, b.Name) })
	return &Tree{Version: Version, Entries: sorted}
}

// A ChunkList is the indirect form of a file's chunk list.
type ChunkList struct {
	Version uint64      `cbor:"v"`
	Chunks  []crypto.ID `cbor:"chunks"`
}

// NewChunkList builds the indirect chunk-list document.
func NewChunkList(chunks []crypto.ID) *ChunkList {
	return &ChunkList{Version: Version, Chunks: chunks}
}

// Encode renders the tree as canonical CBOR and returns it with its ID.
// The ID is the keyed hash of the plaintext: a directory that has not
// changed keeps its name no matter what nonce or encoder version was
// involved in sealing it.
func (t *Tree) Encode(hashKey *crypto.Key) (crypto.ID, []byte, error) {
	if err := t.Validate(); err != nil {
		return crypto.ID{}, nil, err
	}

	encoded, err := crypto.Marshal(t)
	if err != nil {
		return crypto.ID{}, nil, fmt.Errorf("encode tree: %w", err)
	}
	return crypto.ContentID(hashKey, encoded), encoded, nil
}

// Load fetches and verifies a tree stored at its canonical key.
//
// The recovered plaintext is re-hashed and compared to the name it was
// fetched under. The AEAD tag already proves nobody edited the bytes; the
// re-hash proves the name was honest when it was written.
func Load(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID) (*Tree, error) {
	return LoadAt(ctx, b, keys, Key(id), id)
}

// LoadAt fetches and verifies a tree from an explicit key. It is how a
// .r1 replica is read: same object, same name verification, different
// storage location.
func LoadAt(ctx context.Context, b backend.Backend, keys *crypto.Keys, key string, id crypto.ID) (*Tree, error) {
	sealed, err := backend.GetAll(ctx, b, key)
	if err != nil {
		return nil, fmt.Errorf("load tree %s: %w", id, err)
	}

	encoded, err := crypto.Open(&keys.Meta, id[:], sealed)
	if err != nil {
		return nil, fmt.Errorf("load tree %s: %w: %w", id, ErrCorrupt, err)
	}
	if got := crypto.ContentID(&keys.Hash, encoded); got != id {
		return nil, fmt.Errorf("load tree %s: %w: contents hash to %s", id, ErrCorrupt, got)
	}

	var t Tree
	if err := crypto.Unmarshal(encoded, &t); err != nil {
		return nil, fmt.Errorf("load tree %s: %w: %w", id, ErrCorrupt, err)
	}
	if t.Version != Version {
		return nil, fmt.Errorf("load tree %s: %w: object declares version %d, this build reads %d", id, ErrCorrupt, t.Version, Version)
	}
	if err := t.Validate(); err != nil {
		return nil, fmt.Errorf("load tree %s: %w", id, err)
	}
	return &t, nil
}

// LoadChain walks a segmented directory from its last segment backwards
// and returns every entry, oldest first.
func LoadChain(ctx context.Context, b backend.Backend, keys *crypto.Keys, last crypto.ID) ([]Entry, error) {
	var parts [][]Entry
	next := &last
	seen := make(map[crypto.ID]struct{})
	for next != nil {
		if _, dup := seen[*next]; dup {
			return nil, fmt.Errorf("load tree chain: %w: loop at %s", ErrCorrupt, next)
		}
		seen[*next] = struct{}{}
		t, err := Load(ctx, b, keys, *next)
		if err != nil {
			return nil, err
		}
		parts = append(parts, t.Entries)
		next = t.Prev
	}
	slices.Reverse(parts)
	var all []Entry
	for _, p := range parts {
		all = append(all, p...)
	}
	return all, nil
}

// Ptr returns a pointer to v. Entry metadata fields are pointers, and a
// non-nil pointer to zero is a real zero (uid 0 is root), so builders
// need an ergonomic way to say "recorded, and it is 0".
func Ptr[T any](v T) *T { return &v }

// singleComponent reports whether name is exactly one clean path
// component. v3 has no synthetic root, so EVERY entry name obeys this --
// the v2 exception for the root tree's absolute-path names is gone.
func singleComponent(name []byte) bool {
	return len(name) > 0 && !bytes.ContainsRune(name, '/') && !bytes.ContainsRune(name, 0) &&
		!bytes.Equal(name, []byte(".")) && !bytes.Equal(name, []byte(".."))
}

// Validate rejects trees that are decodable but cannot mean anything,
// including the per-kind field matrix (docs/format.md §8.1): a
// reader must refuse an entry whose metadata fields contradict the kind
// that claims them.
func (t *Tree) Validate() error {
	if t.Version != Version {
		return fmt.Errorf("%w: object declares version %d, this build reads %d", ErrCorrupt, t.Version, Version)
	}
	var previous []byte
	for i, e := range t.Entries {
		if !singleComponent(e.Name) {
			return fmt.Errorf("%w: entry %d is not a single clean path component", ErrCorrupt, i)
		}
		if i > 0 && bytes.Compare(e.Name, previous) <= 0 {
			// Out of order means the object was not produced by New, and
			// its ID is therefore not a function of its contents.
			return fmt.Errorf("%w: entry %q follows %q; entries must be sorted and unique", ErrCorrupt, e.Name, previous)
		}
		previous = slices.Clone(e.Name)

		if e.ContentType > uint8(ContentIndirect) {
			return fmt.Errorf("%w: entry %q has unknown content type %d", ErrCorrupt, e.Name, e.ContentType)
		}
		switch NodeType(e.Type) {
		case TypeFile:
			if len(e.Target) > 0 {
				return fmt.Errorf("%w: file %q has a symlink target", ErrCorrupt, e.Name)
			}
			if e.Subtree != nil {
				return fmt.Errorf("%w: file %q has a subtree", ErrCorrupt, e.Name)
			}
		case TypeDir:
			if e.Size != 0 {
				return fmt.Errorf("%w: directory %q has a size", ErrCorrupt, e.Name)
			}
			if len(e.Target) > 0 {
				return fmt.Errorf("%w: directory %q has a symlink target", ErrCorrupt, e.Name)
			}
			if len(e.Chunks) > 0 {
				return fmt.Errorf("%w: directory %q has chunks", ErrCorrupt, e.Name)
			}
			if e.Subtree == nil {
				return fmt.Errorf("%w: directory %q has no subtree", ErrCorrupt, e.Name)
			}
		case TypeSymlink:
			if len(e.Target) == 0 {
				return fmt.Errorf("%w: symlink %q has no target", ErrCorrupt, e.Name)
			}
			if e.Size != 0 {
				return fmt.Errorf("%w: symlink %q has a size", ErrCorrupt, e.Name)
			}
			if len(e.Chunks) > 0 {
				return fmt.Errorf("%w: symlink %q has chunks", ErrCorrupt, e.Name)
			}
			if e.Subtree != nil {
				return fmt.Errorf("%w: symlink %q has a subtree", ErrCorrupt, e.Name)
			}
		default:
			return fmt.Errorf("%w: entry %q has unknown type %d", ErrCorrupt, e.Name, e.Type)
		}
		if NodeType(e.Type) != TypeFile && len(e.Chunks) > 0 {
			return fmt.Errorf("%w: entry %q has chunks but is not a file", ErrCorrupt, e.Name)
		}
		if len(e.Chunks) > MaxInlineChunks && ContentType(e.ContentType) == ContentDirect {
			return fmt.Errorf("%w: file %q has %d inline chunks, over the %d limit (must be indirect)", ErrCorrupt, e.Name, len(e.Chunks), MaxInlineChunks)
		}
		if !MetaKind(e.MetaKind).Valid() {
			return fmt.Errorf("%w: entry %q has unknown metadata kind %d", ErrCorrupt, e.Name, e.MetaKind)
		}
		switch MetaKind(e.MetaKind) {
		case MetaPOSIX:
			if e.Mode == nil || e.UID == nil || e.GID == nil || e.MTimeNs == nil {
				// uid/gid are REQUIRED: uid 0 is root, a real value, so
				// "not recorded" does not exist for a posix source.
				return fmt.Errorf("%w: posix entry %q requires mode/uid/gid/mtime", ErrCorrupt, e.Name)
			}
		case MetaSFTP:
			if e.MTimeNs == nil {
				return fmt.Errorf("%w: sftp entry %q requires mtime", ErrCorrupt, e.Name)
			}
			if e.CTimeNs != nil || e.Device != nil || e.Inode != nil || e.Links != nil ||
				len(e.Xattrs) > 0 || len(e.Etag) > 0 || len(e.Vern) > 0 {
				return fmt.Errorf("%w: sftp entry %q must not carry posix/s3 fields", ErrCorrupt, e.Name)
			}
		case MetaS3:
			// etag/vern are the source's claim and must not enter memory
			// or the repository unbounded (§8.1: 1 KiB each). The Rust
			// Validate enforces the same cap.
			if len(e.Etag) > MaxEtagVernBytes {
				return fmt.Errorf("%w: s3 entry %q etag exceeds %d bytes", ErrCorrupt, e.Name, MaxEtagVernBytes)
			}
			if len(e.Vern) > MaxEtagVernBytes {
				return fmt.Errorf("%w: s3 entry %q vern exceeds %d bytes", ErrCorrupt, e.Name, MaxEtagVernBytes)
			}
			if e.Mode != nil || e.UID != nil || e.GID != nil || e.CTimeNs != nil ||
				e.Device != nil || e.Inode != nil || e.Links != nil || len(e.Xattrs) > 0 {
				return fmt.Errorf("%w: s3 entry %q must not carry posix fields", ErrCorrupt, e.Name)
			}
		case MetaGeneric:
			if e.Mode != nil || e.UID != nil || e.GID != nil || e.CTimeNs != nil ||
				e.Device != nil || e.Inode != nil || e.Links != nil ||
				len(e.Xattrs) > 0 || len(e.Etag) > 0 || len(e.Vern) > 0 {
				return fmt.Errorf("%w: generic entry %q carries mtime only", ErrCorrupt, e.Name)
			}
		}
	}
	return nil
}
