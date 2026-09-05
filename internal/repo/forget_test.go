package repo

import (
	"context"
	"encoding/hex"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/at-least/kist/internal/snapshot"
)

func handleAt(client string, at time.Time) snapshot.Handle {
	id, err := hex.DecodeString(client)
	if err != nil {
		panic(err)
	}
	return snapshot.Handle{ClientID: client, Time: at, Key: snapshot.Key(id, at)}
}

// hourly returns n snapshots one hour apart ending at end, oldest first.
func hourly(client string, end time.Time, n int) []snapshot.Handle {
	out := make([]snapshot.Handle, 0, n)
	for i := n - 1; i >= 0; i-- {
		out = append(out, handleAt(client, end.Add(-time.Duration(i)*time.Hour)))
	}
	return out
}

func keys(handles []snapshot.Handle) string {
	parts := make([]string, len(handles))
	for i, h := range handles {
		parts[i] = h.Time.UTC().Format("01-02T15")
	}
	return strings.Join(parts, " ")
}

func TestRetentionPolicyApply(t *testing.T) {
	end := time.Date(2026, 3, 10, 23, 0, 0, 0, time.UTC) // a Tuesday
	// Ten days of hourly snapshots.
	clientC := "cccccccccccccccccccccccccccccccc"
	all := hourly(clientC, end, 24*10)

	cases := []struct {
		name   string
		policy RetentionPolicy
		want   string // newest first
	}{
		{"last 3", RetentionPolicy{Last: 3}, "03-10T23 03-10T22 03-10T21"},
		{"hourly 2", RetentionPolicy{Hourly: 2}, "03-10T23 03-10T22"},
		{"daily 3", RetentionPolicy{Daily: 3}, "03-10T23 03-09T23 03-08T23"},
		{"weekly 2", RetentionPolicy{Weekly: 2}, "03-10T23 03-08T23"}, // ISO week 11 newest, week 10 ends Sun 03-08
		{"monthly 1", RetentionPolicy{Monthly: 1}, "03-10T23"},
		{"yearly 1", RetentionPolicy{Yearly: 1}, "03-10T23"},
		{"within 90m", RetentionPolicy{Within: 90 * time.Minute}, "03-10T23 03-10T22"},
		{"last 1 + daily 2", RetentionPolicy{Last: 1, Daily: 2}, "03-10T23 03-09T23"},
		{"monthly 12 beyond history", RetentionPolicy{Monthly: 12}, "03-10T23"}, // history starts 03-01T00
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			keep, remove := tc.policy.Apply(all, end)
			if got := keys(keep); got != tc.want {
				t.Errorf("keep = %q, want %q", got, tc.want)
			}
			if len(keep)+len(remove) != len(all) {
				t.Errorf("keep %d + remove %d != %d", len(keep), len(remove), len(all))
			}
		})
	}
}

// Buckets are counted over buckets that hold a snapshot, not calendar
// periods: a gap in the history does not eat into the count.
func TestRetentionPolicySkipsEmptyBuckets(t *testing.T) {
	base := time.Date(2026, 1, 1, 12, 0, 0, 0, time.UTC)
	client := "cccccccccccccccccccccccccccccccc"
	handles := []snapshot.Handle{
		handleAt(client, base),
		handleAt(client, base.AddDate(0, 0, 1)),
		handleAt(client, base.AddDate(0, 0, 10)), // nine-day gap
	}
	keep, _ := RetentionPolicy{Daily: 3}.Apply(handles, base.AddDate(0, 0, 11))
	if len(keep) != 3 {
		t.Errorf("daily 3 kept %d of 3 snapshots across a gap, want all 3", len(keep))
	}
}

func TestRetentionPolicyIsZero(t *testing.T) {
	if !(RetentionPolicy{}).IsZero() {
		t.Error("empty policy is not zero")
	}
	if (RetentionPolicy{Within: time.Second}).IsZero() {
		t.Error("policy with Within is zero")
	}
}

func TestForgetRefusesToForgetEverything(t *testing.T) {
	r, _, _ := backedUpRepo(t, "forget-all")
	if _, err := r.Forget(context.Background(), ForgetOptions{}); !errors.Is(err, ErrNothingToForget) {
		t.Fatalf("forget with no rule: err = %v, want ErrNothingToForget", err)
	}
}

func TestForgetAppliesThePolicyPerClient(t *testing.T) {
	ctx := context.Background()
	r, dir := initRepo(t, "forget-clients")
	source := t.TempDir()
	writeTree(t, source, []fileSpec{{path: "a", data: []byte("a")}})

	// Client A backs up three times, client B once. Last=2 must keep two
	// of A's and B's only one.
	for range 3 {
		if _, _, err := r.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
			t.Fatalf("backup A: %v", err)
		}
	}
	other := reopenAs(t, dir, "forget-clients-b", "ffffffffffffffffffffffffffffffff")
	if _, _, err := other.Backup(ctx, []string{source}, BackupOptions{SpoolDir: t.TempDir()}); err != nil {
		t.Fatalf("backup B: %v", err)
	}

	dry, err := other.Forget(ctx, ForgetOptions{Policy: RetentionPolicy{Last: 2}, DryRun: true})
	if err != nil {
		t.Fatalf("forget dry run: %v", err)
	}
	if len(dry.Kept) != 3 || len(dry.Removed) != 1 {
		t.Fatalf("dry run: kept %d removed %d, want 3 and 1", len(dry.Kept), len(dry.Removed))
	}
	if n := countKeys(t, other.Backend(), snapshot.Prefix); n != 4 {
		t.Errorf("dry run removed something: %d snapshots left, want 4", n)
	}

	result, err := other.Forget(ctx, ForgetOptions{Policy: RetentionPolicy{Last: 2}})
	if err != nil {
		t.Fatalf("forget: %v", err)
	}
	if len(result.Removed) != 1 {
		t.Errorf("removed %d, want 1", len(result.Removed))
	}
	if n := countKeys(t, other.Backend(), snapshot.Prefix); n != 3 {
		t.Errorf("%d snapshots left, want 3", n)
	}

	// The oldest of A's was the one to go, and the data is untouched.
	handles, err := other.Snapshots(ctx, "")
	if err != nil {
		t.Fatalf("snapshots: %v", err)
	}
	report, err := other.Check(ctx, CheckOptions{ReadData: true})
	if err != nil || !report.OK() {
		t.Fatalf("check after forget: %v %v", err, report.Problems)
	}
	if len(handles) != 3 {
		t.Fatalf("%d handles", len(handles))
	}
}

func TestForgetByExplicitKey(t *testing.T) {
	ctx := context.Background()
	r, _, handle := backedUpRepo(t, "forget-key")

	if _, err := r.Forget(ctx, ForgetOptions{Keys: []string{"snapshots/nobody/20260101T000000000000000Z"}}); err == nil {
		t.Fatal("forget of a missing key: got nil error")
	}

	result, err := r.Forget(ctx, ForgetOptions{Keys: []string{handle.Key}})
	if err != nil {
		t.Fatalf("forget: %v", err)
	}
	if len(result.Removed) != 1 || result.Removed[0].Key != handle.Key {
		t.Errorf("removed = %v, want [%s]", result.Removed, handle.Key)
	}
	if n := countKeys(t, r.Backend(), snapshot.Prefix); n != 0 {
		t.Errorf("%d snapshots left, want 0", n)
	}
}

func TestGroupByClientIsSorted(t *testing.T) {
	now := time.Now()
	clientA := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	clientB := "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	groups := groupByClient([]snapshot.Handle{handleAt(clientB, now), handleAt(clientA, now), handleAt(clientB, now.Add(time.Hour))})
	if len(groups) != 2 || groups[0][0].ClientID != clientA || len(groups[1]) != 2 {
		t.Errorf("groups = %v", groups)
	}
}
