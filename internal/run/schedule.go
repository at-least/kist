package run

import (
	"context"
	"time"

	"github.com/robfig/cron/v3"
)

// A Job is something to run on a schedule.
type Job struct {
	Name     string
	Schedule cron.Schedule
	Run      func(ctx context.Context) error
}

// Scheduler runs jobs when their schedules say so, one at a time.
//
// Jobs do not overlap: a backup that runs past the next tick delays the
// tick rather than starting a second backup, which is also what the
// prune safety argument assumes of a client. The clock and the wait are
// injectable so that a year of schedule can be tested in a millisecond.
type Scheduler struct {
	Now  func() time.Time
	Wait func(ctx context.Context, d time.Duration) error
	Logf func(format string, args ...any)
}

func (s *Scheduler) now() time.Time {
	if s.Now != nil {
		return s.Now()
	}
	return time.Now()
}

func (s *Scheduler) wait(ctx context.Context, d time.Duration) error {
	if s.Wait != nil {
		return s.Wait(ctx, d)
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}

func (s *Scheduler) logf(format string, args ...any) {
	if s.Logf != nil {
		s.Logf(format, args...)
	}
}

// Run loops until the context ends. A job's error is logged; the loop
// goes on, because a failed backup tonight is not a reason to skip the
// one tomorrow.
func (s *Scheduler) Run(ctx context.Context, jobs []Job) error {
	next := make([]time.Time, len(jobs))
	for i, j := range jobs {
		next[i] = j.Schedule.Next(s.now())
		s.logf("%s: next run at %s", j.Name, next[i].Format(time.RFC3339))
	}
	for {
		earliest := -1
		for i := range jobs {
			if earliest < 0 || next[i].Before(next[earliest]) {
				earliest = i
			}
		}
		if earliest < 0 {
			<-ctx.Done()
			return ctx.Err()
		}
		if d := next[earliest].Sub(s.now()); d > 0 {
			if err := s.wait(ctx, d); err != nil {
				return err
			}
		}
		now := s.now()
		for i, j := range jobs {
			if next[i].After(now) {
				continue
			}
			if err := ctx.Err(); err != nil {
				return err
			}
			if err := j.Run(ctx); err != nil {
				s.logf("%s: %v", j.Name, err)
			}
			next[i] = j.Schedule.Next(s.now())
			s.logf("%s: next run at %s", j.Name, next[i].Format(time.RFC3339))
		}
	}
}
