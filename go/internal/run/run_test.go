package run

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/at-least/kist/internal/backend"
	"github.com/at-least/kist/internal/config"
	"github.com/at-least/kist/internal/crypto"
	"github.com/at-least/kist/internal/repo"
	"github.com/at-least/kist/internal/report"
)

func cheapKDF() *crypto.KDFParams {
	p := crypto.DefaultKDFParams()
	p.Time, p.MemoryKiB, p.Threads = 1, 8, 1
	return &p
}

// A whole configuration, run once against a local repository: hooks,
// two backups, retention, prune, the webhook and the metrics.
func TestRunnerOnceEndToEnd(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("the hooks use sh")
	}
	ctx := context.Background()
	repoDir := filepath.Join(t.TempDir(), "repo")
	b, err := backend.CreateLocal(repoDir)
	if err != nil {
		t.Fatal(err)
	}
	password := []byte("run-test")
	stateDir, cacheDir := t.TempDir(), t.TempDir()
	init, err := repo.Init(ctx, b, repo.Options{Password: password, StateDir: stateDir, CacheDir: cacheDir, KDF: cheapKDF()})
	if err != nil {
		t.Fatal(err)
	}
	if err := init.Close(); err != nil {
		t.Fatal(err)
	}

	source := t.TempDir()
	if err := os.WriteFile(filepath.Join(source, "a.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(t.TempDir(), "hooks.log")

	var (
		mu     sync.Mutex
		posted []report.Event
	)
	hook := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var ev report.Event
		if err := json.NewDecoder(r.Body).Decode(&ev); err != nil {
			t.Errorf("webhook body: %v", err)
		}
		mu.Lock()
		posted = append(posted, ev)
		mu.Unlock()
		w.WriteHeader(http.StatusNoContent)
	}))
	defer hook.Close()

	cfg, err := config.Parse(`
[repository]
location = "` + repoDir + `"
state_dir = "` + stateDir + `"
cache_dir = "` + cacheDir + `"

[[backup]]
name = "docs"
paths = ["` + source + `"]
schedule = "@daily"
pre_backup = ["sh", "-c", "echo pre $KIST_JOB $KIST_HOOK >> ` + marker + `"]
post_backup = ["sh", "-c", "echo post >> ` + marker + `"]

[[backup]]
name = "broken"
paths = ["` + filepath.Join(t.TempDir(), "missing") + `"]
schedule = "@daily"
pre_backup = ["sh", "-c", "exit 3"]

[retention]
keep_last = 1

[prune]
schedule = "@weekly"
grace = "1h"

[webhook]
url = "` + hook.URL + `"
`)
	if err != nil {
		t.Fatal(err)
	}

	var logs []string
	var events []report.Event
	r := &Runner{
		Config:   cfg,
		Password: password,
		Logf:     func(f string, a ...any) { logs = append(logs, fmt.Sprintf(f, a...)) },
		OpenBackend: func(_ context.Context, location string) (backend.Backend, error) {
			return backend.OpenLocal(location)
		},
		Events: func(ev report.Event) { events = append(events, ev) },
	}
	err = r.Once(ctx)
	if err == nil || !strings.Contains(err.Error(), "broken") {
		t.Fatalf("Once: err = %v, want the broken job reported", err)
	}
	if len(logs) == 0 {
		t.Error("nothing logged")
	}

	if len(events) != 3 {
		t.Fatalf("%d events, want 3: %+v", len(events), events)
	}
	docs, broken, maint := events[0], events[1], events[2]
	if !docs.OK || docs.Backup == nil || docs.Backup.Files != 1 {
		t.Errorf("docs: %+v", docs)
	}
	if broken.OK || !strings.Contains(broken.Error, "pre_backup") || !strings.Contains(broken.Error, "exit status 3") {
		t.Errorf("broken: %+v", broken)
	}
	if !maint.OK || maint.Prune == nil || maint.Forget == nil || maint.Prune.PacksStored != 1 {
		t.Errorf("maintenance: %+v", maint)
	}
	hooks, err := os.ReadFile(marker)
	if err != nil {
		t.Fatal(err)
	}
	if string(hooks) != "pre docs pre_backup\npost\n" {
		t.Errorf("hooks ran: %q", hooks)
	}

	mu.Lock()
	defer mu.Unlock()
	if len(posted) != 3 || posted[0].Kind != "backup" || posted[1].Job != "broken" || posted[2].Kind != "prune" {
		t.Errorf("webhook received %+v", posted)
	}
	text := r.Metrics.Text()
	for _, want := range []string{
		`kist_runs_total{job="docs",result="ok"} 1`,
		`kist_runs_total{job="broken",result="error"} 1`,
		`kist_last_backup_files{job="docs"} 1`,
		`kist_last_prune_packs_stored 1`,
		"# TYPE kist_runs_total counter",
	} {
		if !strings.Contains(text, want) {
			t.Errorf("metrics lack %q:\n%s", want, text)
		}
	}
}

// A webhook that is down does not fail the job.
func TestWebhookFailureIsNotFatal(t *testing.T) {
	cfg, err := config.Parse("[repository]\nlocation='x'\n[prune]\nschedule='@daily'\n[webhook]\nurl='http://127.0.0.1:1/nope'\ntimeout='200ms'")
	if err != nil {
		t.Fatal(err)
	}
	var logs []string
	r := &Runner{Config: cfg, Logf: func(f string, a ...any) { logs = append(logs, f) }}
	ev := report.Event{Kind: "backup", Job: "j", Started: time.Now()}
	if err := r.finish(context.Background(), &ev, nil); err != nil {
		t.Fatalf("finish: %v", err)
	}
	found := false
	for _, l := range logs {
		if strings.Contains(l, "webhook") {
			found = true
		}
	}
	if !found {
		t.Errorf("webhook failure not logged: %v", logs)
	}
}

func TestServeExposesMetrics(t *testing.T) {
	cfg, err := config.Parse("[repository]\nlocation='x'\n[prune]\nschedule='@yearly'\n[metrics]\nlisten='127.0.0.1:0'")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	addr := make(chan string, 1)
	r := &Runner{
		Config: cfg,
		Logf: func(f string, a ...any) {
			line := fmt.Sprintf(f, a...)
			if strings.HasPrefix(line, "metrics on ") {
				addr <- strings.TrimPrefix(line, "metrics on ")
			}
		},
		Wait: func(ctx context.Context, _ time.Duration) error { <-ctx.Done(); return ctx.Err() },
	}
	done := make(chan error, 1)
	go func() { done <- r.Serve(ctx) }()
	var url string
	select {
	case url = <-addr:
	case <-time.After(5 * time.Second):
		t.Fatal("metrics endpoint never announced")
	}
	resp, err := http.Get(url) //nolint:gosec // the URL is the test's own listener
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = resp.Body.Close() }()
	if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "text/plain") {
		t.Errorf("content type %q", ct)
	}
	cancel()
	if err := <-done; !errors.Is(err, context.Canceled) {
		t.Errorf("Serve returned %v", err)
	}
}

// A [prune] grace shorter than the default must reach the backup's
// commit gate: GCGrace's own contract says it must match what prune
// uses, and the runner is the one mode where backup and prune share a
// config. With a millisecond grace the gate refuses immediately (the
// safe side) instead of committing under a 72h assumption the pruner on
// the same config does not share.
func TestRunnerForwardsPruneGraceToTheBackupGate(t *testing.T) {
	ctx := context.Background()
	repoDir := filepath.Join(t.TempDir(), "repo")
	b, err := backend.CreateLocal(repoDir)
	if err != nil {
		t.Fatal(err)
	}
	password := []byte("run-test")
	stateDir, cacheDir := t.TempDir(), t.TempDir()
	init, err := repo.Init(ctx, b, repo.Options{Password: password, StateDir: stateDir, CacheDir: cacheDir, KDF: cheapKDF()})
	if err != nil {
		t.Fatal(err)
	}
	if err := init.Close(); err != nil {
		t.Fatal(err)
	}

	source := t.TempDir()
	if err := os.WriteFile(filepath.Join(source, "a.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg, err := config.Parse(`
[repository]
location = "` + repoDir + `"
state_dir = "` + stateDir + `"
cache_dir = "` + cacheDir + `"

[[backup]]
name = "docs"
paths = ["` + source + `"]
schedule = "@daily"

[prune]
schedule = "@daily"
grace = "1us"
`)
	if err != nil {
		t.Fatal(err)
	}

	var events []report.Event
	r := &Runner{
		Config:   cfg,
		Password: password,
		Logf:     func(string, ...any) {},
		OpenBackend: func(_ context.Context, location string) (backend.Backend, error) {
			return backend.OpenLocal(location)
		},
		Events: func(ev report.Event) { events = append(events, ev) },
	}
	if err := r.Once(ctx); err == nil || !strings.Contains(err.Error(), "docs") {
		t.Fatalf("Once: err = %v, want the docs job reported failed", err)
	}
	if len(events) == 0 || events[0].OK || !strings.Contains(events[0].Error, "longer than the gc grace") {
		t.Fatalf("backup event: %+v, want it refused for outliving the configured 1us grace", events)
	}
}

// A webhook error must not carry the URL into the logs: webhook URLs
// routinely embed tokens or basic-auth userinfo, and warnings persist.
func TestWebhookErrorOmitsTheURL(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer srv.Close()
	// credentials in the userinfo of an otherwise valid http URL.
	u, err := url.Parse(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	u.User = url.UserPassword("ops", "hunter2")
	cfg, err := config.Parse("[repository]\nlocation='x'\n[prune]\nschedule='@daily'\n[webhook]\nurl='" + u.String() + "'")
	if err != nil {
		t.Fatal(err)
	}
	var logs []string
	r := &Runner{Config: cfg, Logf: func(f string, a ...any) { logs = append(logs, fmt.Sprintf(f, a...)) }}
	ev := report.Event{Kind: "backup", Job: "j", Started: time.Now()}
	if err := r.finish(context.Background(), &ev, nil); err != nil {
		t.Fatalf("finish: %v", err)
	}
	for _, l := range logs {
		if strings.Contains(l, "ops:hunter2") || strings.Contains(l, u.Host) {
			t.Errorf("log leaks the webhook URL: %q", l)
		}
	}

	// 傳輸失敗（連不上）走 *url.Error 的拆除路徑：底因進 log，URL 不進。
	dead, err := url.Parse("http://ops:hunter2@127.0.0.1:1/nope")
	if err != nil {
		t.Fatal(err)
	}
	cfg2, err := config.Parse("[repository]\nlocation='x'\n[prune]\nschedule='@daily'\n[webhook]\nurl='" + dead.String() + "'\ntimeout='200ms'")
	if err != nil {
		t.Fatal(err)
	}
	logs = nil
	r2 := &Runner{Config: cfg2, Logf: func(f string, a ...any) { logs = append(logs, fmt.Sprintf(f, a...)) }}
	ev2 := report.Event{Kind: "backup", Job: "j", Started: time.Now()}
	if err := r2.finish(context.Background(), &ev2, nil); err != nil {
		t.Fatalf("finish: %v", err)
	}
	for _, l := range logs {
		// 憑證（userinfo）絕不進 log；撥號位址與設定檔同級，不是秘密。
		if strings.Contains(l, "ops:hunter2") || strings.Contains(l, "dead.String()") {
			t.Errorf("transport-failure log leaks the webhook credentials: %q", l)
		}
	}
}

// A redirect must not be followed: the webhook endpoint (or whoever
// has compromised it) could bounce the event JSON at internal URLs.
// A 3xx is reported as a webhook failure instead.
func TestWebhookDoesNotFollowRedirects(t *testing.T) {
	var redirected atomic.Int32
	final := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		redirected.Add(1)
		w.WriteHeader(http.StatusOK)
	}))
	defer final.Close()
	hook := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, final.URL+"/stolen", http.StatusFound)
	}))
	defer hook.Close()

	cfg, err := config.Parse(fmt.Sprintf("[repository]\nlocation='x'\n[prune]\nschedule='@daily'\n[webhook]\nurl='%s/hook'\ntimeout='2s'", hook.URL))
	if err != nil {
		t.Fatal(err)
	}
	var logs []string
	r := &Runner{Config: cfg, Logf: func(f string, a ...any) { logs = append(logs, fmt.Sprintf(f, a...)) }}
	ev := report.Event{Kind: "backup", Job: "j", Started: time.Now()}
	if err := r.finish(context.Background(), &ev, nil); err != nil {
		t.Fatalf("finish: %v", err)
	}
	if redirected.Load() != 0 {
		t.Fatal("the webhook redirect was followed")
	}
	found := false
	for _, l := range logs {
		if strings.Contains(l, "302") {
			found = true
		}
	}
	if !found {
		t.Errorf("the 3xx must be logged as a webhook failure: %v", logs)
	}
}
