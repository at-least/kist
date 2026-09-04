package repo

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/at-least/kist/internal/snapshot"
)

// RetentionPolicy says which snapshots to keep. Every rule is a reason
// to keep; a snapshot kept by any rule is kept.
//
// The bucketed rules keep the newest snapshot in each of the N most
// recent buckets that hold one, counted backwards from the newest
// snapshot. Buckets are in UTC. A repository backed up every hour with
// Daily=7 keeps one snapshot per day for the last seven days that have a
// snapshot -- not necessarily the last seven calendar days.
type RetentionPolicy struct {
	Last    int
	Hourly  int
	Daily   int
	Weekly  int
	Monthly int
	Yearly  int

	// Within keeps every snapshot newer than now-Within.
	Within time.Duration
}

// IsZero reports whether the policy keeps nothing on its own.
func (p RetentionPolicy) IsZero() bool {
	return p.Last == 0 && p.Hourly == 0 && p.Daily == 0 && p.Weekly == 0 &&
		p.Monthly == 0 && p.Yearly == 0 && p.Within == 0
}

// Apply partitions snapshots into those the policy keeps and those it
// does not. It is a pure function of its inputs, which is what makes it
// testable without a repository.
func (p RetentionPolicy) Apply(handles []snapshot.Handle, now time.Time) (keep, remove []snapshot.Handle) {
	sorted := slices.Clone(handles)
	slices.SortFunc(sorted, func(a, b snapshot.Handle) int { return b.Time.Compare(a.Time) }) // newest first

	kept := make([]bool, len(sorted))
	for i := range sorted {
		if p.Last > 0 && i < p.Last {
			kept[i] = true
		}
		if p.Within > 0 && !sorted[i].Time.Before(now.Add(-p.Within)) {
			kept[i] = true
		}
	}

	for _, rule := range []struct {
		n      int
		bucket func(time.Time) string
	}{
		{p.Hourly, func(t time.Time) string { return t.UTC().Format("2006-01-02T15") }},
		{p.Daily, func(t time.Time) string { return t.UTC().Format("2006-01-02") }},
		{p.Weekly, func(t time.Time) string {
			y, w := t.UTC().ISOWeek()
			return fmt.Sprintf("%04d-W%02d", y, w)
		}},
		{p.Monthly, func(t time.Time) string { return t.UTC().Format("2006-01") }},
		{p.Yearly, func(t time.Time) string { return t.UTC().Format("2006") }},
	} {
		if rule.n == 0 {
			continue
		}
		seen := map[string]struct{}{}
		for i, h := range sorted {
			b := rule.bucket(h.Time)
			if _, done := seen[b]; done {
				continue
			}
			seen[b] = struct{}{}
			kept[i] = true
			if len(seen) == rule.n {
				break
			}
		}
	}

	for i, h := range sorted {
		if kept[i] {
			keep = append(keep, h)
		} else {
			remove = append(remove, h)
		}
	}
	return keep, remove
}

// ForgetOptions configure Forget.
type ForgetOptions struct {
	// Policy is applied per client: each client keeps its own last N, so
	// a machine that backs up rarely is not forgotten because another
	// backs up often.
	Policy RetentionPolicy

	// ClientID restricts the policy to one client. Empty means all.
	ClientID string

	// Keys are snapshots to remove explicitly, whatever the policy says.
	Keys []string

	// DryRun reports what would be removed and removes nothing.
	DryRun bool
}

// ForgetResult reports what Forget did, or would do.
type ForgetResult struct {
	Kept    []snapshot.Handle
	Removed []snapshot.Handle
}

// ErrNothingToForget means no rule and no key was given. Forgetting
// everything is a thing a person must type, not a default.
var ErrNothingToForget = errors.New("forget: no retention rule and no snapshot given; refusing to forget everything")

// Forget removes snapshots. Only snapshots: a snapshot is a leaf, nothing
// refers to it, so deleting one leaves nothing dangling and needs no
// grace period. The data it referenced stays until prune finds it
// unreferenced.
func (r *Repository) Forget(ctx context.Context, opts ForgetOptions) (ForgetResult, error) {
	if opts.Policy.IsZero() && len(opts.Keys) == 0 {
		return ForgetResult{}, ErrNothingToForget
	}

	handles, err := snapshot.List(ctx, r.backend, opts.ClientID)
	if err != nil {
		return ForgetResult{}, fmt.Errorf("forget: %w", err)
	}
	byKey := make(map[string]snapshot.Handle, len(handles))
	for _, h := range handles {
		byKey[h.Key] = h
	}

	explicit := map[string]struct{}{}
	for _, key := range opts.Keys {
		if _, ok := byKey[key]; !ok {
			return ForgetResult{}, fmt.Errorf("forget: %s: %w", key, errSnapshotNotFound)
		}
		explicit[key] = struct{}{}
	}

	var result ForgetResult
	now := r.now()
	for _, group := range groupByClient(handles) {
		keep, remove := group, []snapshot.Handle(nil)
		if !opts.Policy.IsZero() {
			keep, remove = opts.Policy.Apply(group, now)
		}
		for _, h := range keep {
			if _, forced := explicit[h.Key]; forced {
				remove = append(remove, h)
			} else {
				result.Kept = append(result.Kept, h)
			}
		}
		result.Removed = append(result.Removed, remove...)
	}
	sortHandles(result.Kept)
	sortHandles(result.Removed)

	if opts.DryRun {
		return result, nil
	}
	for _, h := range result.Removed {
		if err := r.backend.Delete(ctx, h.Key); err != nil {
			return result, fmt.Errorf("forget %s: %w", h.Key, err)
		}
	}
	return result, nil
}

var errSnapshotNotFound = errors.New("no such snapshot")

func groupByClient(handles []snapshot.Handle) [][]snapshot.Handle {
	byClient := map[string][]snapshot.Handle{}
	var order []string
	for _, h := range handles {
		if _, ok := byClient[h.ClientID]; !ok {
			order = append(order, h.ClientID)
		}
		byClient[h.ClientID] = append(byClient[h.ClientID], h)
	}
	slices.Sort(order)
	out := make([][]snapshot.Handle, 0, len(order))
	for _, c := range order {
		out = append(out, byClient[c])
	}
	return out
}

func sortHandles(handles []snapshot.Handle) {
	slices.SortFunc(handles, func(a, b snapshot.Handle) int {
		if c := a.Time.Compare(b.Time); c != 0 {
			return c
		}
		return strings.Compare(a.ClientID, b.ClientID)
	})
}
