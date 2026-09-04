// Package run executes a configuration: scheduled backups, the
// maintenance job, the webhook and the metrics endpoint.
package run

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/config"
	"github.com/at-least/kist/internal/metrics"
	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

// Runner executes one configuration.
type Runner struct {
	Config *config.Config

	// OpenBackend opens the repository's storage; the CLI supplies its
	// scheme dispatch.
	OpenBackend func(ctx context.Context, location string) (backend.Backend, error)

	// Password is the repository password.
	Password []byte

	// Logf receives one line per notable thing, for the journal.
	Logf func(format string, args ...any)

	// Now and Wait drive the scheduler; nil means real time.
	Now  func() time.Time
	Wait func(ctx context.Context, d time.Duration) error

	// HTTP posts the webhook. Nil means http.DefaultClient with the
	// configured timeout.
	HTTP *http.Client

	// Metrics is the registry served on the metrics endpoint, and
	// updated whether or not one is configured.
	Metrics *metrics.Registry

	// Events receives every report as it is produced, for tests and for
	// --once's summary. Optional.
	Events func(report.Event)
}

func (r *Runner) logf(format string, args ...any) {
	if r.Logf != nil {
		r.Logf(format, args...)
	}
}

func (r *Runner) now() time.Time {
	if r.Now != nil {
		return r.Now()
	}
	return time.Now()
}

// Jobs builds the scheduled jobs from the configuration.
func (r *Runner) Jobs() []Job {
	var jobs []Job
	for i := range r.Config.Backups {
		b := &r.Config.Backups[i]
		jobs = append(jobs, Job{Name: b.Name, Schedule: b.Cron(), Run: func(ctx context.Context) error {
			return r.backup(ctx, b)
		}})
	}
	if p := r.Config.Prune; p != nil {
		jobs = append(jobs, Job{Name: "maintenance", Schedule: p.Cron(), Run: r.maintain})
	}
	return jobs
}

// Once runs every job now, in order, and reports the first failure.
func (r *Runner) Once(ctx context.Context) error {
	var failed []string
	for _, j := range r.Jobs() {
		if err := j.Run(ctx); err != nil {
			r.logf("%s: %v", j.Name, err)
			failed = append(failed, j.Name)
		}
	}
	if len(failed) > 0 {
		return fmt.Errorf("%d job(s) failed: %s", len(failed), strings.Join(failed, ", "))
	}
	return nil
}

// Serve runs the scheduler and the metrics endpoint until ctx ends.
func (r *Runner) Serve(ctx context.Context) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	if m := r.Config.Metrics; m != nil {
		ln, err := net.Listen("tcp", m.Listen)
		if err != nil {
			return fmt.Errorf("metrics: %w", err)
		}
		srv := &http.Server{Handler: r.metricsHandler(), ReadHeaderTimeout: 5 * time.Second}
		go func() {
			if err := srv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
				r.logf("metrics: %v", err)
				cancel()
			}
		}()
		defer func() {
			shutdown, stop := context.WithTimeout(context.Background(), 2*time.Second)
			defer stop()
			if err := srv.Shutdown(shutdown); err != nil {
				r.logf("metrics: shutdown: %v", err)
			}
		}()
		r.logf("metrics on http://%s/metrics", ln.Addr())
	}

	s := &Scheduler{Now: r.Now, Wait: r.Wait, Logf: r.Logf}
	return s.Run(ctx, r.Jobs())
}

func (r *Runner) metricsHandler() http.Handler {
	mux := http.NewServeMux()
	mux.Handle("/metrics", r.registry())
	return mux
}

func (r *Runner) registry() *metrics.Registry {
	if r.Metrics == nil {
		r.Metrics = metrics.New()
	}
	return r.Metrics
}

func (r *Runner) closeRepo(rp *repo.Repository) {
	if err := rp.Close(); err != nil {
		r.logf("closing the repository: %v", err)
	}
}

// open opens the repository for one job.
func (r *Runner) open(ctx context.Context, warn func(string, ...any)) (*repo.Repository, error) {
	b, err := r.OpenBackend(ctx, r.Config.Repository.Location)
	if err != nil {
		return nil, err
	}
	rp, err := repo.Open(ctx, b, repo.Options{
		Password: r.Password,
		ClientID: r.Config.Repository.ClientID,
		StateDir: r.Config.Repository.StateDir,
		CacheDir: r.Config.Repository.CacheDir,
		Now:      r.Now,
		Warnf:    warn,
	})
	if err != nil {
		if cerr := b.Close(); cerr != nil {
			r.logf("closing the backend: %v", cerr)
		}
		return nil, err
	}
	return rp, nil
}

func (r *Runner) backup(ctx context.Context, b *config.Backup) error {
	ev := report.Event{Kind: "backup", Job: b.Name, Started: r.now()}
	warn := func(format string, args ...any) {
		msg := fmt.Sprintf(format, args...)
		ev.Warnings = append(ev.Warnings, msg)
		r.logf("%s: warning: %s", b.Name, msg)
	}
	err := func() error {
		if len(b.PreBackup) > 0 {
			if err := r.hook(ctx, b.Name, "pre_backup", b.PreBackup); err != nil {
				return err
			}
		}
		if len(b.PostBackup) > 0 {
			defer func() {
				if err := r.hook(ctx, b.Name, "post_backup", b.PostBackup); err != nil {
					warn("%v", err)
				}
			}()
		}
		rp, err := r.open(ctx, warn)
		if err != nil {
			return err
		}
		defer r.closeRepo(rp)
		snap, handle, err := rp.Backup(ctx, b.Paths, repo.BackupOptions{Host: b.Host, SpoolDir: b.SpoolDir, Parity: r.Config.Repository.Parity, Warnf: warn})
		if err != nil {
			return err
		}
		ev.Backup = report.FromBackup(snap, handle)
		return nil
	}()
	return r.finish(ctx, &ev, err)
}

func (r *Runner) maintain(ctx context.Context) error {
	p := r.Config.Prune
	ev := report.Event{Kind: "prune", Job: "maintenance", Started: r.now()}
	warn := func(format string, args ...any) {
		msg := fmt.Sprintf(format, args...)
		ev.Warnings = append(ev.Warnings, msg)
		r.logf("maintenance: warning: %s", msg)
	}
	err := func() error {
		rp, err := r.open(ctx, warn)
		if err != nil {
			return err
		}
		defer r.closeRepo(rp)
		if policy := r.Config.Retention.Policy(); !policy.IsZero() {
			result, err := rp.Forget(ctx, repo.ForgetOptions{Policy: policy})
			if err != nil {
				return err
			}
			ev.Forget = report.FromForget(result, false)
			r.logf("maintenance: forgot %d snapshot(s), kept %d", len(result.Removed), len(result.Kept))
		}
		result, err := rp.Prune(ctx, repo.PruneOptions{
			Grace: p.Grace, ClockSkew: p.ClockSkew, ForgetClientsAfter: p.ForgetClientsAfter,
			Progressf: func(format string, args ...any) { r.logf("maintenance: "+format, args...) },
		})
		if err != nil {
			return err
		}
		ev.Prune = report.FromPrune(result, false)
		for _, key := range result.UnreadableClients {
			warn("%s is not a readable client record; left in place", key)
		}
		r.logf("maintenance: marked %d, unmarked %d, held %d, deleted %d", len(result.Marked), len(result.Unmarked), len(result.Held), len(result.Deleted))
		return nil
	}()
	return r.finish(ctx, &ev, err)
}

// finish stamps the event, records metrics, posts the webhook and
// returns the job's error.
func (r *Runner) finish(ctx context.Context, ev *report.Event, err error) error {
	ev.Finished = r.now()
	ev.OK = err == nil
	if err != nil {
		ev.Error = err.Error()
	}
	r.record(ev)
	if r.Events != nil {
		r.Events(*ev)
	}
	if r.Config.Webhook != nil {
		if werr := r.post(ctx, *ev); werr != nil {
			r.logf("%s: webhook: %v", ev.Job, werr)
		}
	}
	if err != nil {
		return err
	}
	if ev.Backup != nil {
		r.logf("%s: snapshot %s: %d files, %s read, %s stored in %d new packs, %s",
			ev.Job, ev.Backup.Snapshot, ev.Backup.Files, human(ev.Backup.Bytes), human(ev.Backup.BytesStored), ev.Backup.PacksAdded, ev.Duration().Round(time.Millisecond))
	}
	return nil
}

func (r *Runner) record(ev *report.Event) {
	reg := r.registry()
	result := "ok"
	if !ev.OK {
		result = "error"
	}
	reg.Add("kist_runs_total", "Jobs run, by job and result.", map[string]string{"job": ev.Job, "result": result}, 1)
	reg.Set("kist_last_run_timestamp_seconds", "When the job last finished, by result.", map[string]string{"job": ev.Job, "result": result}, float64(ev.Finished.Unix()))
	reg.Set("kist_last_run_duration_seconds", "How long the job's last run took.", map[string]string{"job": ev.Job}, ev.Duration().Seconds())
	if b := ev.Backup; b != nil {
		labels := map[string]string{"job": ev.Job}
		reg.Set("kist_last_backup_files", "Files in the last successful backup.", labels, float64(b.Files))
		reg.Set("kist_last_backup_bytes", "Bytes read by the last successful backup.", labels, float64(b.Bytes))
		reg.Set("kist_last_backup_bytes_stored", "Bytes uploaded by the last successful backup.", labels, float64(b.BytesStored))
		reg.Set("kist_last_backup_packs_added", "Packs written by the last successful backup.", labels, float64(b.PacksAdded))
	}
	if p := ev.Prune; p != nil {
		reg.Set("kist_last_prune_packs_stored", "Packs in the repository at the last prune.", nil, float64(p.PacksStored))
		reg.Set("kist_last_prune_packs_live", "Packs some snapshot needs at the last prune.", nil, float64(p.PacksLive))
		reg.Set("kist_last_prune_packs_deleted", "Packs deleted by the last prune.", nil, float64(len(p.Deleted)))
		reg.Set("kist_last_prune_packs_held", "Marked packs the last prune left for later.", nil, float64(len(p.Held)))
		reg.Add("kist_prune_bytes_reclaimed_total", "Bytes reclaimed by prune.", nil, float64(p.BytesReclaimed))
	}
}

// post sends the event to the webhook. A webhook that fails is logged,
// not fatal: the backup happened, and a monitoring system that is down
// is not a reason to report that it did not.
func (r *Runner) post(ctx context.Context, ev report.Event) error {
	body, err := json.Marshal(ev)
	if err != nil {
		return err
	}
	timeout := r.Config.Webhook.Timeout
	if timeout == 0 {
		timeout = 10 * time.Second
	}
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, r.Config.Webhook.URL, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("User-Agent", "kist")
	client := r.HTTP
	if client == nil {
		client = http.DefaultClient
	}
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer func() { _ = resp.Body.Close() }()
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 64<<10)) //nolint:errcheck // draining a response body whose content is irrelevant
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return fmt.Errorf("%s returned %s", r.Config.Webhook.URL, resp.Status)
	}
	return nil
}

// hook runs one pre/post command with the job's environment.
func (r *Runner) hook(ctx context.Context, job, which string, argv []string) error {
	cmd := exec.CommandContext(ctx, argv[0], argv[1:]...) //nolint:gosec // the command is the user's own configuration
	cmd.Env = append(os.Environ(), "KIST_JOB="+job, "KIST_HOOK="+which)
	var out bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &out
	err := cmd.Run()
	if text := strings.TrimSpace(out.String()); text != "" {
		r.logf("%s: %s: %s", job, which, text)
	}
	if err != nil {
		return fmt.Errorf("%s %v: %w", which, argv, err)
	}
	return nil
}

func human(n uint64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}
	div, exp := uint64(unit), 0
	for m := n / unit; m >= unit; m /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", float64(n)/float64(div), "KMGTPE"[exp])
}
