package tree

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"slices"

	"github.com/fxamacker/cbor/v2"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

// Version is the tree object schema version.
const Version = 2

// Prefix is the repository prefix tree objects live under.
const Prefix = "trees/"

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
func writeHead(buf *bytes.Buffer, major, length uint64) {
	switch {
	case length < 24:
		buf.WriteByte(byte(major)<<5 | byte(length))
	case length <= 0xff:
		buf.WriteByte(byte(major)<<5 | 24)
		buf.WriteByte(byte(length))
	case length <= 0xffff:
		buf.WriteByte(byte(major)<<5 | 25)
		buf.WriteByte(byte(length >> 8))
		buf.WriteByte(byte(length))
	default:
		buf.WriteByte(byte(major)<<5 | 26)
		buf.WriteByte(byte(length >> 24))
		buf.WriteByte(byte(length >> 16))
		buf.WriteByte(byte(length >> 8))
		buf.WriteByte(byte(length))
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
// Every optional field is omitempty, so a plain file costs nothing for
// the symlink target or the extended attributes it does not have. The
// set of omitted-at-zero fields is format: both implementations must
// drop exactly the same fields, or two encoders would produce two names
// for one directory. ID-valued fields use pointers because Go's omitempty
// does not consider a zero array empty; Xattrs is handled by the custom
// marshaler below, because omitempty cannot see through a type with its
// own MarshalCBOR.
type Entry struct {
	Name []byte `cbor:"n"`
	Type uint8  `cbor:"t"` // NodeType

	Mode uint32 `cbor:"mode"`
	UID  uint32 `cbor:"uid,omitempty"`
	GID  uint32 `cbor:"gid,omitempty"`

	// MTimeNs and CTimeNs are Unix nanoseconds. CTimeNs is the fast-path
	// signal: the kernel updates it on every write and it cannot be set
	// by a user, so a copy that preserves mtime is still caught. Zero
	// means "not recorded on this platform"; it is never compared.
	MTimeNs int64 `cbor:"mtime"`
	CTimeNs int64 `cbor:"ctime,omitempty"`

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

	// Device and Inode identify a hard link. They are recorded only when
	// Links is above one, and are used by restore to recreate the link
	// rather than a second copy of the data.
	Device uint64 `cbor:"dev,omitempty"`
	Inode  uint64 `cbor:"ino,omitempty"`
	Links  uint64 `cbor:"nlink,omitempty"`

	// Xattrs are extended attributes. M1-era restores reported what they
	// did not apply rather than pretending they did; applying them is
	// later work, but the format records them from day one.
	Xattrs Xattrs `cbor:"xattrs,omitempty"`
}

// entryWire is Entry with Xattrs as a pre-encoded message, so the field
// is really absent (not an empty map) when there are none. A nil
// RawMessage is a nil slice: omitempty drops it.
type entryWire struct {
	Name        []byte          `cbor:"n"`
	Type        uint8           `cbor:"t"`
	Mode        uint32          `cbor:"mode"`
	UID         uint32          `cbor:"uid,omitempty"`
	GID         uint32          `cbor:"gid,omitempty"`
	MTimeNs     int64           `cbor:"mtime"`
	CTimeNs     int64           `cbor:"ctime,omitempty"`
	Size        uint64          `cbor:"size,omitempty"`
	Target      []byte          `cbor:"target,omitempty"`
	Chunks      []crypto.ID     `cbor:"chunks,omitempty"`
	ContentType uint8           `cbor:"ct,omitempty"`
	Subtree     *crypto.ID      `cbor:"tree,omitempty"`
	Device      uint64          `cbor:"dev,omitempty"`
	Inode       uint64          `cbor:"ino,omitempty"`
	Links       uint64          `cbor:"nlink,omitempty"`
	Xattrs      cbor.RawMessage `cbor:"xattrs,omitempty"`
}

// MarshalCBOR renders the entry, omitting an empty Xattrs entirely.
func (e Entry) MarshalCBOR() ([]byte, error) {
	w := entryWire{
		Name: e.Name, Type: e.Type, Mode: e.Mode, UID: e.UID, GID: e.GID,
		MTimeNs: e.MTimeNs, CTimeNs: e.CTimeNs, Size: e.Size, Target: e.Target,
		Chunks: e.Chunks, ContentType: e.ContentType, Subtree: e.Subtree,
		Device: e.Device, Inode: e.Inode, Links: e.Links,
	}
	if len(e.Xattrs) > 0 {
		raw, err := e.Xattrs.MarshalCBOR()
		if err != nil {
			return nil, err
		}
		w.Xattrs = cbor.RawMessage(raw)
	}
	// crypto.Marshal (not the raw cbor package): the Core Deterministic
	// encoder mode is what sorts the map keys; the bare package default
	// would emit them in field order and fork the tree ID.
	return crypto.Marshal(&w)
}

// UnmarshalCBOR decodes the entry, tolerating a missing Xattrs.
func (e *Entry) UnmarshalCBOR(data []byte) error {
	var w entryWire
	if err := crypto.Unmarshal(data, &w); err != nil {
		return err
	}
	*e = Entry{
		Name: w.Name, Type: w.Type, Mode: w.Mode, UID: w.UID, GID: w.GID,
		MTimeNs: w.MTimeNs, CTimeNs: w.CTimeNs, Size: w.Size, Target: w.Target,
		Chunks: w.Chunks, ContentType: w.ContentType, Subtree: w.Subtree,
		Device: w.Device, Inode: w.Inode, Links: w.Links,
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

// FileMode returns the entry's permission and type bits.
func (e Entry) FileMode() fs.FileMode { return fs.FileMode(e.Mode) }

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

// Save encodes, seals and stores the tree, returning its ID.
//
// The AAD is the tree's own ID, so a sealed tree cannot be served in
// place of another one. Storing is an unconditional Put, deliberately:
// same name means same bytes, so re-putting is a no-op that (a) repairs
// a damaged tree on the next backup and (b) refreshes the object's
// modification time, which the gc mark protocol on the writer side
// compares against (a PutIfAbsent would leave an old mtime on the
// already-exists path and lose that signal).
func (t *Tree) Save(ctx context.Context, b backend.Backend, keys *crypto.Keys, nonceSource io.Reader) (crypto.ID, error) {
	id, encoded, err := t.Encode(&keys.Hash)
	if err != nil {
		return crypto.ID{}, err
	}

	sealed, err := crypto.Seal(&keys.Meta, id[:], encoded, nonceSource)
	if err != nil {
		return crypto.ID{}, fmt.Errorf("save tree %s: %w", id, err)
	}

	if err := backend.PutBytes(ctx, b, Key(id), sealed); err != nil {
		return crypto.ID{}, fmt.Errorf("save tree %s: %w", id, err)
	}
	return id, nil
}

// Load fetches and verifies a tree.
//
// The recovered plaintext is re-hashed and compared to the name it was
// fetched under. The AEAD tag already proves nobody edited the bytes; the
// re-hash proves the name was honest when it was written.
func Load(ctx context.Context, b backend.Backend, keys *crypto.Keys, id crypto.ID) (*Tree, error) {
	sealed, err := backend.GetAll(ctx, b, Key(id))
	if err != nil {
		return nil, fmt.Errorf("load tree %s: %w", id, err)
	}

	encoded, err := crypto.Open(&keys.Meta, id[:], sealed)
	if err != nil {
		return nil, fmt.Errorf("load tree %s: %w", id, err)
	}
	if got := crypto.ContentID(&keys.Hash, encoded); got != id {
		return nil, fmt.Errorf("load tree %s: %w: contents hash to %s", id, ErrCorrupt, got)
	}

	var t Tree
	if err := crypto.Unmarshal(encoded, &t); err != nil {
		return nil, fmt.Errorf("load tree %s: %w", id, err)
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

// validAbsolutePath reports whether name is an absolute path made of
// clean components -- the form the synthetic root tree's entries use.
func validAbsolutePath(name []byte) bool {
	if len(name) == 0 || name[0] != '/' {
		return false
	}
	for _, comp := range bytes.Split(name[1:], []byte("/")) {
		if len(comp) == 0 || bytes.Equal(comp, []byte(".")) || bytes.Equal(comp, []byte("..")) {
			return false
		}
	}
	return true
}

// Validate rejects trees that are decodable but cannot mean anything.
func (t *Tree) Validate() error {
	var previous []byte
	for i, e := range t.Entries {
		switch {
		case len(e.Name) == 0:
			return fmt.Errorf("%w: entry %d has an empty name", ErrCorrupt, i)
		case bytes.Equal(e.Name, []byte(".")) || bytes.Equal(e.Name, []byte("..")):
			return fmt.Errorf("%w: entry %d is named %q", ErrCorrupt, i, e.Name)
		case bytes.ContainsAny(e.Name, "\x00"):
			return fmt.Errorf("%w: entry %q contains NUL", ErrCorrupt, e.Name)
		case bytes.Contains(e.Name, []byte("/")) && !validAbsolutePath(e.Name):
			// An absolute path is the naming rule for the entries of the
			// synthetic ROOT tree (one entry per backup source). A '/'
			// anywhere else means a child entry that is not a single
			// component; restore refuses those independently.
			return fmt.Errorf("%w: entry %q is neither a single component nor an absolute path", ErrCorrupt, e.Name)
		case i > 0 && bytes.Compare(e.Name, previous) <= 0:
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
			if e.Subtree != nil {
				return fmt.Errorf("%w: file %q has a subtree", ErrCorrupt, e.Name)
			}
			if len(e.Target) > 0 {
				return fmt.Errorf("%w: file %q has a symlink target", ErrCorrupt, e.Name)
			}
			if len(e.Chunks) > MaxInlineChunks && ContentType(e.ContentType) == ContentDirect {
				return fmt.Errorf("%w: file %q has %d inline chunks, over the %d limit (must be indirect)", ErrCorrupt, e.Name, len(e.Chunks), MaxInlineChunks)
			}
		case TypeDir:
			if e.Subtree == nil || e.Subtree.IsZero() {
				return fmt.Errorf("%w: directory %q has no subtree", ErrCorrupt, e.Name)
			}
			if len(e.Chunks) > 0 {
				return fmt.Errorf("%w: directory %q has chunks", ErrCorrupt, e.Name)
			}
		case TypeSymlink:
			if len(e.Target) == 0 {
				return fmt.Errorf("%w: symlink %q has no target", ErrCorrupt, e.Name)
			}
			if len(e.Chunks) > 0 {
				return fmt.Errorf("%w: symlink %q has chunks", ErrCorrupt, e.Name)
			}
		default:
			return fmt.Errorf("%w: entry %q has unknown type %d", ErrCorrupt, e.Name, e.Type)
		}
	}
	return nil
}
