package snapshot

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/crypto"
)

// Version is the snapshot object schema version.
const Version = 2

// Prefix is the repository prefix snapshots live under.
const Prefix = "snapshots/"

// A snapshot's key timestamp is YYYYMMDDTHHMMSSnnnnnnnnnZ: fixed width
// so that lexical order is chronological order, no colons (Windows), and
// no dot before the nanoseconds -- Go's reference layouts cannot express
// nine fixed digits without a decimal point, so the conversion is done
// by hand below.
const tsLen = 8 + 1 + 6 + 9 + 1 // 20260905T114202395568091Z

// FormatKeyTime renders t in the key timestamp form (mount uses it for
// its directory names).
func FormatKeyTime(t time.Time) string { return formatKeyTime(t) }

func formatKeyTime(t time.Time) string {
	t = t.UTC()
	return fmt.Sprintf("%04d%02d%02dT%02d%02d%02d%09dZ",
		t.Year(), int(t.Month()), t.Day(), t.Hour(), t.Minute(), t.Second(), t.Nanosecond())
}

func parseKeyTime(s string) (time.Time, error) {
	if len(s) != tsLen || s[8] != 'T' || s[24] != 'Z' {
		return time.Time{}, fmt.Errorf("timestamp %q is not YYYYMMDDTHHMMSSnnnnnnnnnZ", s)
	}
	digits := func(run string) (int, error) {
		v, err := strconv.Atoi(run)
		if err != nil || v < 0 {
			return 0, fmt.Errorf("timestamp %q: %q is not a number", s, run)
		}
		return v, nil
	}
	year, err := digits(s[0:4])
	if err != nil {
		return time.Time{}, err
	}
	month, err := digits(s[4:6])
	if err != nil {
		return time.Time{}, err
	}
	day, err := digits(s[6:8])
	if err != nil {
		return time.Time{}, err
	}
	hour, err := digits(s[9:11])
	if err != nil {
		return time.Time{}, err
	}
	minute, err := digits(s[11:13])
	if err != nil {
		return time.Time{}, err
	}
	second, err := digits(s[13:15])
	if err != nil {
		return time.Time{}, err
	}
	nanos, err := digits(s[15:24])
	if err != nil {
		return time.Time{}, err
	}
	return time.Date(year, time.Month(month), day, hour, minute, second, nanos, time.UTC), nil
}

// ErrCorrupt means a snapshot object is structurally invalid.
var ErrCorrupt = errors.New("snapshot object is corrupt")

// maxTimestampRetries bounds the walk forward through timestamps when a
// key is already taken. Reaching it means something other than a
// collision is wrong -- a clock stuck in the past, or a repository being
// filled deliberately -- and failing is better than looping.
const maxTimestampRetries = 1000

// Stats summarise what a backup did. They are reporting, not structure:
// nothing reads them back to make a decision. The field set is the union
// of the two v1 implementations.
type Stats struct {
	Files        uint64 `cbor:"files,omitempty"`
	Dirs         uint64 `cbor:"dirs,omitempty"`
	Symlinks     uint64 `cbor:"symlinks,omitempty"`
	Bytes        uint64 `cbor:"bytes,omitempty"`
	ChunksNew    uint64 `cbor:"chunks_new,omitempty"`
	ChunksRead   uint64 `cbor:"chunks_read,omitempty"`
	PacksAdded   uint64 `cbor:"packs_new,omitempty"`
	PacksRevived uint64 `cbor:"packs_revived,omitempty"`
	BytesStored  uint64 `cbor:"bytes_stored,omitempty"`
	Errors       uint64 `cbor:"errors,omitempty"`
	FilesReused  uint64 `cbor:"files_reused,omitempty"`
}

// A Snapshot is one completed backup.
type Snapshot struct {
	Version uint64 `cbor:"v"`

	// Root is the tree the backup produced (its last segment if the root
	// directory was split).
	Root crypto.ID `cbor:"root"`

	// TimeNs is when the backup started, in UTC nanoseconds. The key
	// carries the same instant; this field is what a reader trusts,
	// because the key is only a name.
	TimeNs int64 `cbor:"time"`

	// Host, User and Paths describe where the data came from, for a human
	// choosing which snapshot to restore. Paths are byte strings: on Unix
	// they are the raw OS bytes of the backed-up paths.
	Host  string   `cbor:"host"`
	User  string   `cbor:"user,omitempty"`
	Paths [][]byte `cbor:"paths"`

	// ClientID is the 16-byte identity of the client that wrote this
	// snapshot. It duplicates the key's namespace so that a snapshot
	// moved to another namespace fails to agree with itself.
	ClientID []byte `cbor:"client"`

	// Parent is this client's previous snapshot for the same paths. It is
	// an accelerator only (the Rust implementation uses it for a
	// metadata-unchanged fast path); readers must not depend on it.
	Parent *string `cbor:"parent,omitempty"`

	Stats Stats `cbor:"stats"`
}

// Key returns the repository key a snapshot is stored under.
func Key(clientID []byte, at time.Time) string {
	return Prefix + hex.EncodeToString(clientID) + "/" + formatKeyTime(at)
}

// A Handle names a stored snapshot without loading it.
type Handle struct {
	// ClientID is the hex form, which is what appears in the key.
	ClientID string
	Time     time.Time
	Key      string
}

// ParseKey splits a snapshot key back into its parts.
func ParseKey(key string) (Handle, error) {
	rest, ok := strings.CutPrefix(key, Prefix)
	if !ok {
		return Handle{}, fmt.Errorf("%w: key %q is not under %s", ErrCorrupt, key, Prefix)
	}
	clientID, stamp, ok := strings.Cut(rest, "/")
	if !ok || clientID == "" || stamp == "" {
		return Handle{}, fmt.Errorf("%w: key %q is not %s<client>/<timestamp>", ErrCorrupt, key, Prefix)
	}

	at, err := parseKeyTime(stamp)
	if err != nil {
		return Handle{}, fmt.Errorf("%w: key %q has an unparseable timestamp: %w", ErrCorrupt, key, err)
	}
	return Handle{ClientID: clientID, Time: at, Key: key}, nil
}

// Save commits the snapshot.
//
// This is the only write in a repository whose ordering matters, and the
// only one that must not silently replace what is there: two clients that
// happen to pick the same nanosecond are two different backups, not one.
// A collision advances the timestamp and retries, and the returned handle
// says where the snapshot actually landed.
//
// The AAD is the full key, so a snapshot object moved into another
// client's namespace, or renamed to another time, no longer opens.
func (s *Snapshot) Save(ctx context.Context, b backend.Backend, keys *crypto.Keys, nonceSource io.Reader) (Handle, error) {
	if err := s.validate(); err != nil {
		return Handle{}, err
	}

	at := time.Unix(0, s.TimeNs).UTC()
	for range maxTimestampRetries {
		key := Key(s.ClientID, at)

		encoded, err := crypto.Marshal(s)
		if err != nil {
			return Handle{}, fmt.Errorf("save snapshot: %w", err)
		}
		sealed, err := crypto.Seal(&keys.Meta, []byte(key), encoded, nonceSource)
		if err != nil {
			return Handle{}, fmt.Errorf("save snapshot %s: %w", key, err)
		}

		switch err := backend.PutBytesIfAbsent(ctx, b, key, sealed); {
		case err == nil:
			return Handle{ClientID: hex.EncodeToString(s.ClientID), Time: at, Key: key}, nil
		case errors.Is(err, backend.ErrExists):
			at = at.Add(time.Nanosecond)
			s.TimeNs = at.UnixNano()
		default:
			return Handle{}, fmt.Errorf("save snapshot %s: %w", key, err)
		}
	}
	return Handle{}, fmt.Errorf("save snapshot: %d consecutive timestamps under %s%s/ were taken", maxTimestampRetries, Prefix, s.ClientID)
}

// Load fetches and verifies one snapshot.
func Load(ctx context.Context, b backend.Backend, keys *crypto.Keys, key string) (*Snapshot, error) {
	handle, err := ParseKey(key)
	if err != nil {
		return nil, err
	}

	sealed, err := backend.GetAll(ctx, b, key)
	if err != nil {
		return nil, fmt.Errorf("load snapshot %s: %w", key, err)
	}
	encoded, err := crypto.Open(&keys.Meta, []byte(key), sealed)
	if err != nil {
		return nil, fmt.Errorf("load snapshot %s: %w", key, err)
	}

	var s Snapshot
	if err := crypto.Unmarshal(encoded, &s); err != nil {
		return nil, fmt.Errorf("load snapshot %s: %w", key, err)
	}
	if s.Version != Version {
		return nil, fmt.Errorf("load snapshot %s: %w: object declares version %d, this build reads %d", key, ErrCorrupt, s.Version, Version)
	}
	if err := s.validate(); err != nil {
		return nil, fmt.Errorf("load snapshot %s: %w", key, err)
	}

	// The key is a name; the object is the record. They must agree.
	if hex.EncodeToString(s.ClientID) != handle.ClientID {
		return nil, fmt.Errorf("load snapshot %s: %w: object claims client %q", key, ErrCorrupt, s.ClientID)
	}
	if !time.Unix(0, s.TimeNs).UTC().Equal(handle.Time) {
		return nil, fmt.Errorf("load snapshot %s: %w: object claims time %s", key, ErrCorrupt, time.Unix(0, s.TimeNs).UTC().Format(time.RFC3339Nano))
	}
	return &s, nil
}

// List returns every snapshot in the repository, oldest first. Passing an
// empty clientID lists them all.
func List(ctx context.Context, b backend.Backend, clientID string) ([]Handle, error) {
	prefix := Prefix
	if clientID != "" {
		prefix += clientID + "/"
	}

	var handles []Handle
	err := b.List(ctx, prefix, func(fi backend.FileInfo) error {
		handle, err := ParseKey(fi.Key)
		if err != nil {
			return err
		}
		handles = append(handles, handle)
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("list snapshots: %w", err)
	}

	slices.SortFunc(handles, func(a, b Handle) int {
		if c := a.Time.Compare(b.Time); c != 0 {
			return c
		}
		return strings.Compare(a.ClientID, b.ClientID)
	})
	return handles, nil
}

func (s *Snapshot) validate() error {
	switch {
	case s.Root.IsZero():
		return fmt.Errorf("%w: no root tree", ErrCorrupt)
	case len(s.ClientID) != 16:
		return fmt.Errorf("%w: client ID is %d bytes, want 16", ErrCorrupt, len(s.ClientID))
	case strings.ContainsAny(hex.EncodeToString(s.ClientID), "/"):
		return fmt.Errorf("%w: client ID %q contains a slash", ErrCorrupt, s.ClientID)
	case s.TimeNs == 0:
		return fmt.Errorf("%w: no timestamp", ErrCorrupt)
	case len(s.Paths) == 0:
		return fmt.Errorf("%w: no source paths", ErrCorrupt)
	}
	return nil
}
