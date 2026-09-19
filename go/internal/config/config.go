// Package config reads the declarative configuration that `kist run`
// executes: which repository, what to back up on which schedule, what
// to keep, when to prune, whom to tell.
package config

import (
	"errors"
	"fmt"
	"net/url"
	"os"
	"strings"
	"time"

	"github.com/BurntSushi/toml"
	"github.com/robfig/cron/v3"

	"github.com/at-least/kist/internal/repo"
)

// Config is one kist.toml.
//
// Decoding is strict: a key this struct does not know is an error, not a
// silently ignored typo. A misspelt "keep_dialy" that kept nothing would
// be found the day the disk was needed.
type Config struct {
	Repository Repository `toml:"repository"`
	Backups    []Backup   `toml:"backup"`
	Retention  *Retention `toml:"retention"`
	Prune      *Prune     `toml:"prune"`
	Webhook    *Webhook   `toml:"webhook"`
	Metrics    *Metrics   `toml:"metrics"`
}

// Repository says where and how to open the repository.
type Repository struct {
	Location string `toml:"location"`

	// PasswordFile holds the repository password. Empty means the
	// KIST_PASSWORD environment variable; there is no terminal prompt in
	// run mode, because nobody is there to answer it.
	PasswordFile string `toml:"password_file"`

	// ClientID overrides the persisted client identity. Normally empty.
	ClientID string `toml:"client_id"`

	// StateDir and CacheDir override the default locations.
	StateDir string `toml:"state_dir"`
	CacheDir string `toml:"cache_dir"`

	// Parity is the number of Reed-Solomon parity shards to store beside
	// each pack this machine writes, out of 16; 0 stores none.
	Parity int `toml:"parity"`
}

// Backup is one scheduled backup job.
type Backup struct {
	Name     string   `toml:"name"`
	Paths    []string `toml:"paths"`
	Schedule string   `toml:"schedule"`
	Host     string   `toml:"host"`
	SpoolDir string   `toml:"spool_dir"`

	// PreBackup and PostBackup are commands, as argv, run around the
	// backup: take a filesystem snapshot, release it. A failing
	// PreBackup aborts the backup; PostBackup always runs and its
	// failure is reported, not fatal. This is the whole of kist's VSS
	// and LVM story: the script that knows the machine is the user's.
	PreBackup  []string `toml:"pre_backup"`
	PostBackup []string `toml:"post_backup"`

	schedule cron.Schedule
}

// Retention maps onto repo.RetentionPolicy. It is applied by the
// maintenance job, before prune, because forgetting a snapshot is a
// delete and the backup role has no delete.
type Retention struct {
	KeepLast    int           `toml:"keep_last"`
	KeepHourly  int           `toml:"keep_hourly"`
	KeepDaily   int           `toml:"keep_daily"`
	KeepWeekly  int           `toml:"keep_weekly"`
	KeepMonthly int           `toml:"keep_monthly"`
	KeepYearly  int           `toml:"keep_yearly"`
	KeepWithin  time.Duration `toml:"keep_within"`
}

// Policy converts to the repository's type.
func (r *Retention) Policy() repo.RetentionPolicy {
	if r == nil {
		return repo.RetentionPolicy{}
	}
	return repo.RetentionPolicy{
		Last: r.KeepLast, Hourly: r.KeepHourly, Daily: r.KeepDaily, Weekly: r.KeepWeekly,
		Monthly: r.KeepMonthly, Yearly: r.KeepYearly, Within: r.KeepWithin,
	}
}

// Prune is the maintenance job: forget by the retention policy, then
// prune. It belongs on a maintenance host with delete credentials, not
// on the machines being backed up.
type Prune struct {
	Schedule           string        `toml:"schedule"`
	Grace              time.Duration `toml:"grace"`
	ForgetClientsAfter time.Duration `toml:"forget_clients_after"`

	// ClockSkew is a pointer so that an absent key and an explicit zero
	// are different settings: absent takes the library default, an
	// explicit "0s" really means "the clocks agree". The library takes
	// the value it is given literally.
	ClockSkew *time.Duration `toml:"clock_skew"`

	schedule cron.Schedule
}

// Options converts to the repository's type the way the maintenance job
// runs it: every key the config leaves out takes the default the CLI
// flags default to, so a scheduled prune and a hand-run prune agree.
func (p *Prune) Options() repo.PruneOptions {
	opts := repo.PruneOptions{
		Grace:              p.Grace,
		ForgetClientsAfter: p.ForgetClientsAfter,
	}
	if p.ClockSkew != nil {
		opts.ClockSkew = *p.ClockSkew
	} else {
		opts.ClockSkew = repo.DefaultClockSkew
	}
	return opts
}

// Webhook receives a JSON report after every job.
type Webhook struct {
	URL     string        `toml:"url"`
	Timeout time.Duration `toml:"timeout"`
}

// Metrics serves Prometheus metrics while run is up.
type Metrics struct {
	Listen string `toml:"listen"`
}

// Load reads and validates a configuration file.
func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path) //nolint:gosec // the path is the user's own argument
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	cfg, err := Parse(string(data))
	if err != nil {
		return nil, fmt.Errorf("config %s: %w", path, err)
	}
	return cfg, nil
}

// Parse decodes and validates configuration text.
func Parse(text string) (*Config, error) {
	var cfg Config
	md, err := toml.Decode(text, &cfg)
	if err != nil {
		return nil, err
	}
	if undecoded := md.Undecoded(); len(undecoded) > 0 {
		keys := make([]string, len(undecoded))
		for i, k := range undecoded {
			keys[i] = k.String()
		}
		return nil, fmt.Errorf("unknown key %s", strings.Join(keys, ", "))
	}
	if err := cfg.validate(); err != nil {
		return nil, err
	}
	return &cfg, nil
}

func (c *Config) validate() error {
	if c.Repository.Location == "" {
		return errors.New("repository.location is required")
	}
	if c.Repository.Parity < 0 || c.Repository.Parity > 8 {
		return fmt.Errorf("repository.parity is %d, want 0..8", c.Repository.Parity)
	}
	if len(c.Backups) == 0 && c.Prune == nil {
		return errors.New("nothing to run: no [[backup]] and no [prune]")
	}
	names := map[string]struct{}{}
	for i := range c.Backups {
		b := &c.Backups[i]
		if b.Name == "" {
			b.Name = fmt.Sprintf("backup-%d", i+1)
		}
		if _, dup := names[b.Name]; dup {
			return fmt.Errorf("backup %q: duplicate name", b.Name)
		}
		names[b.Name] = struct{}{}
		if len(b.Paths) == 0 {
			return fmt.Errorf("backup %q: paths is required", b.Name)
		}
		if b.Schedule == "" {
			return fmt.Errorf("backup %q: schedule is required", b.Name)
		}
		s, err := cron.ParseStandard(b.Schedule)
		if err != nil {
			return fmt.Errorf("backup %q: schedule %q: %w", b.Name, b.Schedule, err)
		}
		b.schedule = s
	}
	if c.Retention != nil && c.Prune == nil {
		return errors.New("[retention] needs a [prune] section: forgetting is a delete, and it runs with the maintenance job")
	}
	if c.Prune != nil {
		if c.Prune.Schedule == "" {
			return errors.New("prune.schedule is required")
		}
		s, err := cron.ParseStandard(c.Prune.Schedule)
		if err != nil {
			return fmt.Errorf("prune.schedule %q: %w", c.Prune.Schedule, err)
		}
		c.Prune.schedule = s
		if c.Prune.Grace < 0 {
			return fmt.Errorf("prune.grace is %s, want a positive duration (omit it for the default of %s)", c.Prune.Grace, repo.DefaultGrace)
		}
		if c.Prune.ClockSkew != nil && *c.Prune.ClockSkew < 0 {
			return fmt.Errorf("prune.clock_skew is %s, want a non-negative duration", *c.Prune.ClockSkew)
		}
		if c.Prune.ForgetClientsAfter < 0 {
			return fmt.Errorf("prune.forget_clients_after is %s, want a non-negative duration (omit it for the default)", c.Prune.ForgetClientsAfter)
		}
	}
	if c.Webhook != nil {
		if c.Webhook.URL == "" {
			return errors.New("webhook.url is required")
		}
		// http(s) only (the Rust side enforces the same): anything else
		// reaching a transport would be misinterpreted at best, and URLs
		// embedding credentials in other schemes are a habit to refuse
		// early.
		u, err := url.Parse(c.Webhook.URL)
		if err != nil || (u.Scheme != "http" && u.Scheme != "https") {
			return fmt.Errorf("webhook.url %q must start with http:// or https://", c.Webhook.URL)
		}
	}
	if c.Metrics != nil && c.Metrics.Listen == "" {
		return errors.New("metrics.listen is required")
	}
	return nil
}

// Cron returns the parsed schedule of a backup job.
func (b *Backup) Cron() cron.Schedule { return b.schedule }

// Cron returns the parsed schedule of the maintenance job.
func (p *Prune) Cron() cron.Schedule { return p.schedule }

// MixesRoles reports whether one configuration holds both backup and
// maintenance jobs. It is allowed -- a single-machine setup is real --
// but it means the machine being backed up holds delete credentials,
// which is exactly what the permission model exists to avoid.
func (c *Config) MixesRoles() bool { return len(c.Backups) > 0 && c.Prune != nil }

// Password reads the repository password from the file or the
// environment.
func (r *Repository) Password(env string) ([]byte, error) {
	if r.PasswordFile != "" {
		data, err := os.ReadFile(r.PasswordFile)
		if err != nil {
			return nil, fmt.Errorf("read password file: %w", err)
		}
		return []byte(strings.TrimRight(string(data), "\r\n")), nil
	}
	if v, ok := os.LookupEnv(env); ok {
		return []byte(v), nil
	}
	return nil, fmt.Errorf("no password: set repository.password_file or %s", env)
}
