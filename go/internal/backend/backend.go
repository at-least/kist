package backend

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
	"time"
)

// Sentinel errors every implementation must return, so callers can branch
// on the outcome instead of on a message.
var (
	// ErrNotFound means the key holds no object.
	ErrNotFound = errors.New("object not found")

	// ErrExists means PutIfAbsent found the key already taken. For a
	// content-addressed object this is success -- the bytes are already
	// there. For a snapshot it is a collision to retry.
	ErrExists = errors.New("object already exists")

	// ErrLocked means Delete was accepted but the object's bytes are
	// retained by the storage -- an Object Lock retention, say -- and
	// remain readable. The caller must go on treating the object as
	// present.
	ErrLocked = errors.New("object is retained by the storage")
)

// ReadToEnd is the length argument to Get that means "from off to the end
// of the object".
const ReadToEnd int64 = -1

// FileInfo is what Stat and List report about one object.
//
// Modified is the backend's own clock, truncated to whole seconds: S3
// reports seconds and its list/head precisions differ, and the gc mark
// protocol depends on times never appearing finer than they are.
type FileInfo struct {
	Key      string
	Size     int64
	Modified time.Time
}

// A Backend is a flat namespace of immutable objects.
//
// Keys are slash-separated lowercase paths such as "packs/<hex>". See
// ValidateKey for the exact rules; every method rejects a key that fails
// it rather than passing it to the storage layer.
type Backend interface {
	// Location is a human-readable description of where this backend
	// stores objects, for error messages and command output.
	Location() string

	// Get returns a reader over length bytes starting at off. A length of
	// ReadToEnd reads to the end of the object. The caller closes it.
	//
	// Ranged reads are not an optimisation: reading a 64 MiB pack to
	// reach its 4 KiB trailer is not acceptable over a network.
	Get(ctx context.Context, key string, off, length int64) (io.ReadCloser, error)

	// Put stores size bytes read from r at key, replacing any object
	// already there.
	//
	// Two callers are allowed: a future `key add` rewriting config's
	// key slots, and check --repair, which replaces a damaged pack with
	// bytes proven -- by hashing to the pack's name -- to be the ones
	// that were there before. Everything a backup writes goes through
	// PutIfAbsent. On a versioned bucket Put makes a new version, which
	// Object Lock permits where a delete would not.
	Put(ctx context.Context, key string, r io.Reader, size int64) error

	// PutIfAbsent stores size bytes read from r at key, or returns
	// ErrExists if the key is taken. The object must not become visible
	// under key until it is complete.
	//
	// Everything a repository writes except config goes through here, and
	// not only for the snapshot commit: a backup client that could
	// overwrite an existing pack could destroy a repository with nothing
	// but its own credentials. A conditional write is what makes "backup
	// cannot delete data" true rather than aspirational.
	PutIfAbsent(ctx context.Context, key string, r io.Reader, size int64) error

	// List calls fn for every object whose key starts with prefix, in no
	// particular order. Returning a non-nil error from fn stops the walk
	// and is returned to the caller.
	List(ctx context.Context, prefix string, fn func(FileInfo) error) error

	// Stat reports on a single object.
	Stat(ctx context.Context, key string) (FileInfo, error)

	// Delete removes an object. Deleting a key that holds no object is
	// not an error: the outcome the caller asked for already holds.
	Delete(ctx context.Context, key string) error

	// Close releases whatever the backend is holding open.
	Close() error
}

// ErrInvalidKey means a key does not satisfy ValidateKey.
var ErrInvalidKey = errors.New("invalid key")

// ValidateKey enforces the key grammar shared by every backend.
//
// The rules are the intersection of what a filesystem, an S3 bucket and
// an SFTP server all accept, and they are deliberately narrow: keys are
// built from hex content addresses, fixed prefixes and the snapshot
// timestamp's uppercase ISO 8601 digits, so nothing legitimate needs a
// character outside this set. In particular there are no colons, which
// are illegal in Windows filenames, and no traversal.
//
// A segment may not begin with a dot, which keeps the whole namespace
// clear of the dot-prefixed scratch names the local backend writes while
// a Put is still in flight.
//
//	segment  = (lowercase-alnum / "-" / "_") *( lowercase-alnum / "-" / "_" / "." )
//	key      = segment *( "/" segment )
func ValidateKey(key string) error {
	// Uppercase is allowed for one reason: the snapshot key's timestamp
	// is uppercase ISO 8601 basic format (YYYYMMDDTHHMMSSnnnnnnnnnZ), the
	// unified v2 form. Everything else kist writes is lowercase hex.
	const allowed = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_."
	segment := func(seg string) bool {
		if seg == "" || seg[0] == '.' {
			return false
		}
		for i := 0; i < len(seg); i++ {
			if !strings.ContainsRune(allowed, rune(seg[i])) {
				return false
			}
		}
		return true
	}
	if key == "" {
		return fmt.Errorf("%w: key is empty", ErrInvalidKey)
	}
	if len(key) > 1024 {
		return fmt.Errorf("%w: key is %d bytes, over the 1024 limit", ErrInvalidKey, len(key))
	}
	if strings.Contains(key, "//") {
		return fmt.Errorf("%w: %q has an empty path segment", ErrInvalidKey, key)
	}
	for _, seg := range strings.Split(key, "/") {
		if !segment(seg) {
			return fmt.Errorf("%w: %q", ErrInvalidKey, key)
		}
	}
	return nil
}

// ValidatePrefix accepts what List accepts: the empty string, a valid key,
// or a valid key followed by a trailing slash to mean "this directory".
func ValidatePrefix(prefix string) error {
	if prefix == "" {
		return nil
	}
	return ValidateKey(strings.TrimSuffix(prefix, "/"))
}

// GetAll reads a whole object into memory. It is a convenience for the
// small objects -- config, trees, snapshots, index blobs -- where holding
// the bytes is what the caller wants anyway.
func GetAll(ctx context.Context, b Backend, key string) ([]byte, error) {
	r, err := b.Get(ctx, key, 0, ReadToEnd)
	if err != nil {
		return nil, err
	}
	defer func() { _ = r.Close() }()

	data, err := io.ReadAll(r)
	if err != nil {
		return nil, fmt.Errorf("read %s from %s: %w", key, b.Location(), err)
	}
	return data, nil
}

// PutBytes stores a byte slice unconditionally.
func PutBytes(ctx context.Context, b Backend, key string, data []byte) error {
	return b.Put(ctx, key, bytes.NewReader(data), int64(len(data)))
}

// PutBytesIfAbsent is PutIfAbsent for an object already in memory: trees,
// snapshots, index blobs and gc markers.
func PutBytesIfAbsent(ctx context.Context, b Backend, key string, data []byte) error {
	return b.PutIfAbsent(ctx, key, bytes.NewReader(data), int64(len(data)))
}

// Exists reports whether an object is stored under key.
func Exists(ctx context.Context, b Backend, key string) (bool, error) {
	switch _, err := b.Stat(ctx, key); {
	case err == nil:
		return true, nil
	case errors.Is(err, ErrNotFound):
		return false, nil
	default:
		return false, err
	}
}
