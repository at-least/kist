package tree

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"slices"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

// Version is the tree object schema version.
const Version = 1

// Prefix is the repository prefix tree objects live under.
const Prefix = "trees/"

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

// String renders the type for command output.
func (t NodeType) String() string {
	switch t {
	case TypeFile:
		return "file"
	case TypeDir:
		return "dir"
	case TypeSymlink:
		return "symlink"
	default:
		return fmt.Sprintf("unknown(%d)", uint8(t))
	}
}

// An Entry is one name in a directory.
//
// Every optional field is omitempty, so a plain file costs nothing for
// the symlink target or the extended attributes it does not have.
type Entry struct {
	Name string   `cbor:"n"`
	Type NodeType `cbor:"t"`

	Mode    uint32 `cbor:"mode"`
	UID     uint32 `cbor:"uid,omitempty"`
	GID     uint32 `cbor:"gid,omitempty"`
	MTimeNs int64  `cbor:"mtime,omitempty"`
	CTimeNs int64  `cbor:"ctime,omitempty"`

	// Size is the file's length in bytes; zero for other types.
	Size uint64 `cbor:"size,omitempty"`

	// Target is a symlink's destination.
	Target string `cbor:"target,omitempty"`

	// Chunks lists a file's content chunks in order. It is stored inline:
	// a 100 GiB file is about 1.6 MiB of IDs, which a tree carries
	// comfortably. The case this does not serve well is a directory of
	// many very large files, where touching one rewrites a large tree;
	// that is the trigger for a v2 with an indirection blob, recorded in
	// ADR 004 and deliberately not built now.
	Chunks []crypto.ID `cbor:"chunks,omitempty"`

	// Subtree is a directory's tree ID.
	Subtree crypto.ID `cbor:"tree,omitempty"`

	// Device and Inode identify a hard link. They are recorded only when
	// Links is above one, and are used by restore to recreate the link
	// rather than a second copy of the data.
	Device uint64 `cbor:"dev,omitempty"`
	Inode  uint64 `cbor:"ino,omitempty"`
	Links  uint64 `cbor:"nlink,omitempty"`

	// Xattrs are extended attributes. M1 records them; restoring them is
	// M4 work, so a restore reports what it did not apply rather than
	// pretending it did.
	Xattrs map[string][]byte `cbor:"xattrs,omitempty"`
}

// FileMode returns the entry's permission and type bits.
func (e Entry) FileMode() fs.FileMode { return fs.FileMode(e.Mode) }

// A Tree is one directory.
type Tree struct {
	Version uint64  `cbor:"v"`
	Entries []Entry `cbor:"entries"`
}

// New builds a tree from entries, sorting them into canonical order.
//
// Sorting is what makes a directory's encoding a function of its
// contents: two clients that walk the same directory in different orders
// must produce the same tree ID or deduplication stops at the first
// directory.
func New(entries []Entry) *Tree {
	sorted := slices.Clone(entries)
	slices.SortFunc(sorted, func(a, b Entry) int { return bytes.Compare([]byte(a.Name), []byte(b.Name)) })
	return &Tree{Version: Version, Entries: sorted}
}

// Encode renders the tree as canonical CBOR and returns it with its ID.
func (t *Tree) Encode(hashKey *crypto.Key) (crypto.ID, []byte, error) {
	if err := t.validate(); err != nil {
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
// place of another one. Storing is PutIfAbsent: an unchanged subtree is
// already there, and that is the whole point.
func (t *Tree) Save(ctx context.Context, b backend.Backend, keys *crypto.Keys, nonceSource io.Reader) (crypto.ID, error) {
	id, encoded, err := t.Encode(&keys.Hash)
	if err != nil {
		return crypto.ID{}, err
	}

	sealed, err := crypto.Seal(&keys.Meta, id[:], encoded, nonceSource)
	if err != nil {
		return crypto.ID{}, fmt.Errorf("save tree %s: %w", id, err)
	}

	switch err := backend.PutBytesIfAbsent(ctx, b, Key(id), sealed); {
	case err == nil, errors.Is(err, backend.ErrExists):
		return id, nil
	default:
		return crypto.ID{}, fmt.Errorf("save tree %s: %w", id, err)
	}
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
	if err := t.validate(); err != nil {
		return nil, fmt.Errorf("load tree %s: %w", id, err)
	}
	return &t, nil
}

// validate rejects trees that are decodable but cannot mean anything.
func (t *Tree) validate() error {
	var previous string
	for i, e := range t.Entries {
		switch {
		case e.Name == "":
			return fmt.Errorf("%w: entry %d has an empty name", ErrCorrupt, i)
		case e.Name == "." || e.Name == "..":
			return fmt.Errorf("%w: entry %d is named %q", ErrCorrupt, i, e.Name)
		case bytes.ContainsAny([]byte(e.Name), "/\x00"):
			return fmt.Errorf("%w: entry %q contains a path separator or NUL", ErrCorrupt, e.Name)
		case i > 0 && e.Name <= previous:
			// Out of order means the object was not produced by New, and
			// its ID is therefore not a function of its contents.
			return fmt.Errorf("%w: entry %q follows %q; entries must be sorted and unique", ErrCorrupt, e.Name, previous)
		}
		previous = e.Name

		switch e.Type {
		case TypeFile:
			if !e.Subtree.IsZero() {
				return fmt.Errorf("%w: file %q has a subtree", ErrCorrupt, e.Name)
			}
			if e.Target != "" {
				return fmt.Errorf("%w: file %q has a symlink target", ErrCorrupt, e.Name)
			}
		case TypeDir:
			if e.Subtree.IsZero() {
				return fmt.Errorf("%w: directory %q has no subtree", ErrCorrupt, e.Name)
			}
			if len(e.Chunks) > 0 {
				return fmt.Errorf("%w: directory %q has chunks", ErrCorrupt, e.Name)
			}
		case TypeSymlink:
			if e.Target == "" {
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
