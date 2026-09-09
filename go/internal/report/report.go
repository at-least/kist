// Package report holds the machine-readable shapes of what kist did.
// The same structs are what --json prints and what the webhook posts,
// so a consumer parses one format.
package report

import (
	"time"

	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/snapshot"
)

// Event is one completed job or command.
type Event struct {
	// Kind is backup, forget, prune, check, restore or rebuild_index.
	Kind string `json:"kind"`

	// Job names the configured job, in run mode. Empty for a command.
	Job string `json:"job,omitempty"`

	Started  time.Time `json:"started"`
	Finished time.Time `json:"finished"`
	OK       bool      `json:"ok"`
	Error    string    `json:"error,omitempty"`
	Warnings []string  `json:"warnings,omitempty"`

	Init    *InitResult    `json:"init,omitempty"`
	Backup  *BackupResult  `json:"backup,omitempty"`
	Forget  *ForgetResult  `json:"forget,omitempty"`
	Prune   *PruneResult   `json:"prune,omitempty"`
	Check   *CheckResult   `json:"check,omitempty"`
	Restore *RestoreResult `json:"restore,omitempty"`
	Index   *IndexResult   `json:"index,omitempty"`
}

// Duration is how long the event took.
func (e Event) Duration() time.Duration { return e.Finished.Sub(e.Started) }

// InitResult is a created repository.
type InitResult struct {
	Location string `json:"location"`
	ClientID string `json:"client_id"`
}

// SnapshotSummary is one row of `kist snapshots --json`. Roots are the
// backup sources as recorded (lossy UTF-8): v3 snapshots carry roots,
// not a root tree plus paths.
type SnapshotSummary struct {
	Snapshot string    `json:"snapshot"`
	ClientID string    `json:"client_id"`
	Time     time.Time `json:"time"`
	Host     string    `json:"host,omitempty"`
	Roots    []string  `json:"roots,omitempty"`
	Files    uint64    `json:"files"`
	Dirs     uint64    `json:"dirs"`
	Symlinks uint64    `json:"symlinks"`
	Bytes    uint64    `json:"bytes"`
	Error    string    `json:"error,omitempty"`
}

// BackupResult is a committed snapshot. The Stats fields are the data
// facts the snapshot itself carries; the Report fields are this run's
// process counters, which depend on GC state and dedup order and so live
// here, never in the snapshot (format-v3-draft.md §9.1).
type BackupResult struct {
	Snapshot string   `json:"snapshot"`
	Host     string   `json:"host"`
	Roots    []string `json:"roots"`
	Files    uint64   `json:"files"`
	Dirs     uint64   `json:"dirs"`
	Symlinks uint64   `json:"symlinks"`
	Bytes    uint64   `json:"bytes"`
	Errors   uint64   `json:"errors"`

	Report repo.BackupReport `json:"report"`
}

// FromBackup builds the result of a backup.
func FromBackup(summary repo.BackupSummary) *BackupResult {
	snap := summary.Snapshot
	return &BackupResult{
		Snapshot: summary.Handle.Key, Host: snap.Host, Roots: rootsOf(snap.Roots),
		Files: snap.Stats.Files, Dirs: snap.Stats.Dirs, Symlinks: snap.Stats.Symlinks,
		Bytes: snap.Stats.Bytes, Errors: summary.Report.Errors,
		Report: summary.Report,
	}
}

func rootsOf(roots []snapshot.Root) []string {
	out := make([]string, len(roots))
	for i, r := range roots {
		out[i] = string(r.Path)
	}
	return out
}

// ForgetResult lists what forget removed and kept.
type ForgetResult struct {
	DryRun  bool     `json:"dry_run"`
	Removed []string `json:"removed"`
	Kept    []string `json:"kept"`
	Locked  []string `json:"locked,omitempty"`
}

// FromForget builds the result of a forget.
func FromForget(r repo.ForgetResult, dryRun bool) *ForgetResult {
	return &ForgetResult{DryRun: dryRun, Removed: handleKeys(r.Removed), Kept: handleKeys(r.Kept), Locked: handleKeys(r.Locked)}
}

func handleKeys(handles []snapshot.Handle) []string {
	keys := make([]string, 0, len(handles))
	for _, h := range handles {
		keys = append(keys, h.Key)
	}
	return keys
}

// PruneResult is one prune run.
type PruneResult struct {
	DryRun         bool       `json:"dry_run"`
	PacksStored    int        `json:"packs_stored"`
	PacksLive      int        `json:"packs_live"`
	Marked         []string   `json:"marked"`
	Unmarked       []string   `json:"unmarked"`
	Deleted        []string   `json:"deleted"`
	TreesMarked    []string   `json:"trees_marked"`
	TreesDeleted   []string   `json:"trees_deleted"`
	OrphanTouches  int        `json:"orphan_touches_removed"`
	Locked         []string   `json:"locked,omitempty"`
	Held           []HeldPack `json:"held"`
	BytesReclaimed uint64     `json:"bytes_reclaimed"`
}

// HeldPack is a marked object not deleted, and why.
type HeldPack struct {
	Pack   string `json:"pack"`
	Reason string `json:"reason"`
	Kind   string `json:"kind"`
}

// FromPrune builds the result of a prune.
func FromPrune(r repo.PruneReport, dryRun bool) *PruneResult {
	out := &PruneResult{
		DryRun: dryRun, PacksStored: r.Stored, PacksLive: r.Live,
		Marked: idStrings(r.Marked), Unmarked: idStrings(r.Unmarked), Deleted: idStrings(r.Deleted),
		TreesMarked: idStrings(r.TreesMarked), TreesDeleted: idStrings(r.TreesDeleted),
		OrphanTouches: r.OrphanTouches,
		Locked:        idStrings(r.Locked), Held: make([]HeldPack, 0, len(r.Held)), BytesReclaimed: r.BytesReclaimed,
	}
	for _, h := range r.Held {
		out.Held = append(out.Held, HeldPack{Pack: h.Pack.String(), Reason: h.Reason, Kind: h.Kind})
	}
	return out
}

func idStrings[T interface{ String() string }](ids []T) []string {
	out := make([]string, 0, len(ids))
	for _, id := range ids {
		out = append(out, id.String())
	}
	return out
}

// CheckResult is one check.
type CheckResult struct {
	ReadData  bool     `json:"read_data"`
	Snapshots int      `json:"snapshots"`
	Trees     int      `json:"trees"`
	Chunks    int      `json:"chunks"`
	Packs     int      `json:"packs"`
	Problems  []string `json:"problems"`
	Warnings  []string `json:"warnings,omitempty"`

	ParityPacks  int      `json:"parity_packs"`
	Repaired     []string `json:"repaired,omitempty"`
	Unrepairable []string `json:"unrepairable,omitempty"`
}

// FromCheck builds the result of a check.
func FromCheck(r repo.CheckReport, readData bool) *CheckResult {
	problems := r.Problems
	if problems == nil {
		problems = []string{}
	}
	return &CheckResult{
		ReadData: readData, Snapshots: r.Snapshots, Trees: r.Trees, Chunks: r.Chunks, Packs: r.Packs, Problems: problems, Warnings: r.Warnings,
		ParityPacks: r.ParityPacks, Repaired: idStrings(r.Repaired), Unrepairable: idStrings(r.Unrepairable),
	}
}

// RestoreResult is one restore.
type RestoreResult struct {
	Snapshot  string `json:"snapshot"`
	Target    string `json:"target"`
	Files     uint64 `json:"files"`
	Dirs      uint64 `json:"dirs"`
	Symlinks  uint64 `json:"symlinks"`
	HardLinks uint64 `json:"hard_links"`
	Bytes     uint64 `json:"bytes"`
}

// IndexResult is one rebuild-index.
type IndexResult struct {
	Chunks int `json:"chunks"`
}
