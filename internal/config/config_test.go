package config

import (
	"strings"
	"testing"
	"time"
)

const full = `
[repository]
location = "/srv/kist"
password_file = "/etc/kist/password"

[[backup]]
name = "home"
paths = ["/home", "/etc"]
schedule = "0 2 * * *"
pre_backup = ["/usr/local/bin/snap", "create"]
post_backup = ["/usr/local/bin/snap", "release"]

[[backup]]
paths = ["/var/lib/app"]
schedule = "@hourly"

[retention]
keep_daily = 7
keep_weekly = 4
keep_within = "72h"

[prune]
schedule = "0 4 * * 0"
grace = "96h"
clock_skew = "30m"

[webhook]
url = "https://hooks.example/kist"
timeout = "5s"

[metrics]
listen = "127.0.0.1:9345"
`

func TestParseFullConfig(t *testing.T) {
	cfg, err := Parse(full)
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Backups) != 2 || cfg.Backups[0].Name != "home" || cfg.Backups[1].Name != "backup-2" {
		t.Errorf("backups = %+v", cfg.Backups)
	}
	if cfg.Backups[0].Cron() == nil || cfg.Prune.Cron() == nil {
		t.Error("schedules not parsed")
	}
	next := cfg.Backups[0].Cron().Next(time.Date(2026, 1, 1, 12, 0, 0, 0, time.UTC))
	if want := time.Date(2026, 1, 2, 2, 0, 0, 0, time.UTC); !next.Equal(want) {
		t.Errorf("next run of '0 2 * * *' after noon = %s, want %s", next, want)
	}
	p := cfg.Retention.Policy()
	if p.Daily != 7 || p.Weekly != 4 || p.Within != 72*time.Hour {
		t.Errorf("policy = %+v", p)
	}
	if cfg.Prune.Grace != 96*time.Hour || cfg.Prune.ClockSkew != 30*time.Minute {
		t.Errorf("prune = %+v", cfg.Prune)
	}
	if cfg.Webhook.Timeout != 5*time.Second || cfg.Metrics.Listen != "127.0.0.1:9345" {
		t.Errorf("webhook %+v metrics %+v", cfg.Webhook, cfg.Metrics)
	}
	if !cfg.MixesRoles() {
		t.Error("a config with backups and prune does not report mixed roles")
	}
}

func TestParseRejectsMistakes(t *testing.T) {
	cases := []struct{ name, text, want string }{
		{"unknown key", "[repository]\nlocation='x'\n[[backup]]\npaths=['/a']\nschedule='@daily'\n[retention]\nkeep_dialy=7\n[prune]\nschedule='@weekly'", "unknown key retention.keep_dialy"},
		{"no location", "[[backup]]\npaths=['/a']\nschedule='@daily'", "repository.location is required"},
		{"nothing to run", "[repository]\nlocation='x'", "nothing to run"},
		{"bad schedule", "[repository]\nlocation='x'\n[[backup]]\npaths=['/a']\nschedule='every day'", "schedule"},
		{"no paths", "[repository]\nlocation='x'\n[[backup]]\nschedule='@daily'", "paths is required"},
		{"retention without prune", "[repository]\nlocation='x'\n[[backup]]\npaths=['/a']\nschedule='@daily'\n[retention]\nkeep_last=1", "[retention] needs a [prune]"},
		{"duplicate names", "[repository]\nlocation='x'\n[[backup]]\nname='a'\npaths=['/a']\nschedule='@daily'\n[[backup]]\nname='a'\npaths=['/b']\nschedule='@daily'", "duplicate name"},
		{"webhook without url", "[repository]\nlocation='x'\n[prune]\nschedule='@daily'\n[webhook]\ntimeout='1s'", "webhook.url is required"},
		{"bad duration", "[repository]\nlocation='x'\n[prune]\nschedule='@daily'\ngrace='three days'", "grace"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := Parse(tc.text)
			if err == nil || !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("err = %v, want it to mention %q", err, tc.want)
			}
		})
	}
}

func TestPasswordSources(t *testing.T) {
	r := Repository{}
	t.Setenv("KIST_TEST_PW", "from-env")
	pw, err := r.Password("KIST_TEST_PW")
	if err != nil || string(pw) != "from-env" {
		t.Errorf("env: %q %v", pw, err)
	}
	if _, err := (&Repository{}).Password("KIST_TEST_PW_UNSET"); err == nil {
		t.Error("no source: no error")
	}
}
