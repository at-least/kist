package run

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/robfig/cron/v3"
)

// A year of a daily and a weekly schedule in a few milliseconds: the
// fake Wait moves the clock instead of waiting on it.
func TestSchedulerRunsJobsInOrderWithoutOverlap(t *testing.T) {
	now := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	daily, weekly := mustSchedule(t, "0 2 * * *"), mustSchedule(t, "0 4 * * 0")

	var fired []string
	ctx, cancel := context.WithCancel(context.Background())
	s := &Scheduler{
		Now: func() time.Time { return now },
		Wait: func(_ context.Context, d time.Duration) error {
			now = now.Add(d)
			return nil
		},
	}
	jobs := []Job{
		{Name: "daily", Schedule: daily, Run: func(context.Context) error {
			fired = append(fired, "daily@"+now.Format("01-02T15"))
			now = now.Add(3 * time.Hour) // a slow backup runs past the next tick of nothing
			if len(fired) >= 10 {
				cancel()
			}
			return nil
		}},
		{Name: "weekly", Schedule: weekly, Run: func(context.Context) error {
			fired = append(fired, "weekly@"+now.Format("01-02T15"))
			return errors.New("weekly failed") // must not stop the loop
		}},
	}
	err := s.Run(ctx, jobs)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run returned %v, want context.Canceled", err)
	}
	// Jan 4 2026 is a Sunday. The daily at 02:00 takes three hours, so
	// the weekly due at 04:00 runs when the daily is done, at 05:00 --
	// late, never concurrently.
	want := []string{
		"daily@01-01T02", "daily@01-02T02", "daily@01-03T02", "daily@01-04T02", "weekly@01-04T05",
		"daily@01-05T02", "daily@01-06T02", "daily@01-07T02", "daily@01-08T02", "daily@01-09T02",
	}
	if len(fired) != len(want) {
		t.Fatalf("fired %v\nwant  %v", fired, want)
	}
	for i := range want {
		if fired[i] != want[i] {
			t.Fatalf("fired %v\nwant  %v", fired, want)
		}
	}
}

func TestSchedulerStopsOnContext(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	daily := mustSchedule(t, "@daily")
	s := &Scheduler{Wait: func(ctx context.Context, _ time.Duration) error { return ctx.Err() }}
	err := s.Run(ctx, []Job{{Name: "x", Schedule: daily, Run: func(context.Context) error { t.Fatal("ran"); return nil }}})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("err = %v", err)
	}
}

func mustSchedule(t *testing.T, spec string) cron.Schedule {
	t.Helper()
	s, err := cron.ParseStandard(spec)
	if err != nil {
		t.Fatal(err)
	}
	return s
}
