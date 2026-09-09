package cmd

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/report"
)

// run executes the command tree with buffers attached, the way the binary
// would but without a subprocess.
func run(t *testing.T, args ...string) (stdout, stderr string, err error) {
	t.Helper()

	var out, errOut bytes.Buffer
	root := NewRootCommand()
	root.SetOut(&out)
	root.SetErr(&errOut)
	root.SetArgs(args)

	err = root.ExecuteContext(context.Background())
	return out.String(), errOut.String(), err
}

// The whole CLI surface of M1, in the order a person would use it.
func TestEndToEnd(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")

	repoDir := filepath.Join(t.TempDir(), "repo")
	source := t.TempDir()

	for path, data := range map[string]string{
		"readme.txt":      "hello kist\n",
		"docs/notes.md":   strings.Repeat("compressible. ", 4000),
		"docs/binary.dat": "\x00\x01\x02\x03",
	} {
		full := filepath.Join(source, filepath.FromSlash(path))
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatalf("mkdir: %v", err)
		}
		if err := os.WriteFile(full, []byte(data), 0o644); err != nil {
			t.Fatalf("write: %v", err)
		}
	}

	stdout, _, err := run(t, "init", "--repo", repoDir)
	if err != nil {
		t.Fatalf("init: %v", err)
	}
	if !strings.Contains(stdout, "created repository") {
		t.Errorf("init printed %q", stdout)
	}

	stdout, _, err = run(t, "backup", "--repo", repoDir, "--host", "testhost", source)
	if err != nil {
		t.Fatalf("backup: %v", err)
	}
	if !strings.Contains(stdout, "snapshot snapshots/") || !strings.Contains(stdout, "3 files") {
		t.Errorf("backup printed %q", stdout)
	}

	stdout, _, err = run(t, "snapshots", "--repo", repoDir)
	if err != nil {
		t.Fatalf("snapshots: %v", err)
	}
	if !strings.Contains(stdout, "testhost") || !strings.Contains(stdout, "snapshots/") {
		t.Errorf("snapshots printed %q", stdout)
	}

	key := snapshotKey(t, stdout)

	target := filepath.Join(t.TempDir(), "out")
	stdout, _, err = run(t, "restore", "--repo", repoDir, key, target)
	if err != nil {
		t.Fatalf("restore: %v", err)
	}
	if !strings.Contains(stdout, "restored 3 files") {
		t.Errorf("restore printed %q", stdout)
	}

	// v2 root-tree entries are named by the source's full absolute path,
	// so the restore lands under the target by that path.
	restored, err := os.ReadFile(filepath.Join(target, source, "readme.txt"))
	if err != nil {
		t.Fatalf("read restored file: %v", err)
	}
	if string(restored) != "hello kist\n" {
		t.Errorf("restored file = %q", restored)
	}

	for _, args := range [][]string{
		{"check", "--repo", repoDir},
		{"check", "--repo", repoDir, "--read-data"},
	} {
		stdout, _, err = run(t, args...)
		if err != nil {
			t.Fatalf("%v: %v", args, err)
		}
		if !strings.Contains(stdout, "no problems found") {
			t.Errorf("%v printed %q", args, stdout)
		}
	}

	stdout, _, err = run(t, "rebuild-index", "--repo", repoDir)
	if err != nil {
		t.Fatalf("rebuild-index: %v", err)
	}
	if !strings.Contains(stdout, "rebuilt the index") {
		t.Errorf("rebuild-index printed %q", stdout)
	}

	// A second backup, then forget the older one by policy. No rule and
	// no name is an error, not "forget everything".
	if _, _, err = run(t, "backup", "--repo", repoDir, source); err != nil {
		t.Fatalf("second backup: %v", err)
	}
	if _, _, err = run(t, "forget", "--repo", repoDir); err == nil {
		t.Fatal("forget with no rule succeeded")
	}
	stdout, _, err = run(t, "forget", "--repo", repoDir, "--keep-last", "1", "--dry-run")
	if err != nil {
		t.Fatalf("forget --dry-run: %v", err)
	}
	if !strings.Contains(stdout, "would remove "+key) {
		t.Errorf("forget --dry-run printed %q", stdout)
	}
	stdout, _, err = run(t, "forget", "--repo", repoDir, "--keep-last", "1")
	if err != nil {
		t.Fatalf("forget: %v", err)
	}
	if !strings.Contains(stdout, "removed "+key) || !strings.Contains(stdout, "removed 1 snapshot(s), kept 1") {
		t.Errorf("forget printed %q", stdout)
	}
	stdout, _, err = run(t, "snapshots", "--repo", repoDir)
	if err != nil {
		t.Fatalf("snapshots after forget: %v", err)
	}
	if strings.Contains(stdout, key) {
		t.Errorf("forgotten snapshot still listed: %q", stdout)
	}
}

// Every command speaks JSON on request: one object, or one array for
// snapshots, and the exit code still says whether it went well.
func TestJSONOutput(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")
	repoDir := filepath.Join(t.TempDir(), "repo")
	source := t.TempDir()
	if err := os.WriteFile(filepath.Join(source, "f.txt"), []byte("json"), 0o644); err != nil {
		t.Fatal(err)
	}

	decode := func(t *testing.T, stdout string) report.Event {
		t.Helper()
		var ev report.Event
		if err := json.Unmarshal([]byte(stdout), &ev); err != nil {
			t.Fatalf("not one JSON object: %v\n%s", err, stdout)
		}
		return ev
	}

	stdout, _, err := run(t, "init", "--json", "--repo", repoDir)
	if err != nil {
		t.Fatal(err)
	}
	if ev := decode(t, stdout); ev.Kind != "init" || !ev.OK || ev.Init == nil || ev.Init.ClientID == "" {
		t.Errorf("init: %s", stdout)
	}

	stdout, _, err = run(t, "backup", "--json", "--repo", repoDir, source)
	if err != nil {
		t.Fatal(err)
	}
	backup := decode(t, stdout)
	if backup.Kind != "backup" || !backup.OK || backup.Backup == nil || backup.Backup.Files != 1 || backup.Backup.Snapshot == "" {
		t.Errorf("backup: %s", stdout)
	}
	key := backup.Backup.Snapshot

	stdout, _, err = run(t, "snapshots", "--json", "--repo", repoDir)
	if err != nil {
		t.Fatal(err)
	}
	var rows []report.SnapshotSummary
	if err := json.Unmarshal([]byte(stdout), &rows); err != nil || len(rows) != 1 || rows[0].Snapshot != key || rows[0].Files != 1 {
		t.Errorf("snapshots: %v %s", err, stdout)
	}

	stdout, _, err = run(t, "check", "--json", "--repo", repoDir, "--read-data")
	if err != nil {
		t.Fatal(err)
	}
	if ev := decode(t, stdout); ev.Kind != "check" || !ev.OK || ev.Check == nil || !ev.Check.ReadData || ev.Check.Packs != 1 || ev.Check.Problems == nil {
		t.Errorf("check: %s", stdout)
	}

	target := filepath.Join(t.TempDir(), "out")
	stdout, _, err = run(t, "restore", "--json", "--repo", repoDir, key, target)
	if err != nil {
		t.Fatal(err)
	}
	if ev := decode(t, stdout); ev.Kind != "restore" || ev.Restore == nil || ev.Restore.Files != 1 || ev.Restore.Target != target {
		t.Errorf("restore: %s", stdout)
	}

	stdout, _, err = run(t, "prune", "--json", "--repo", repoDir, "--dry-run")
	if err != nil {
		t.Fatal(err)
	}
	if ev := decode(t, stdout); ev.Kind != "prune" || ev.Prune == nil || !ev.Prune.DryRun || ev.Prune.PacksStored != 1 || ev.Prune.PacksLive != 1 {
		t.Errorf("prune: %s", stdout)
	}

	stdout, _, err = run(t, "forget", "--json", "--repo", repoDir, "--keep-last", "5", "--dry-run")
	if err != nil {
		t.Fatal(err)
	}
	if ev := decode(t, stdout); ev.Kind != "forget" || ev.Forget == nil || len(ev.Forget.Kept) != 1 || len(ev.Forget.Removed) != 0 {
		t.Errorf("forget: %s", stdout)
	}

	stdout, _, err = run(t, "rebuild-index", "--json", "--repo", repoDir)
	if err != nil {
		t.Fatal(err)
	}
	if ev := decode(t, stdout); ev.Kind != "rebuild_index" || ev.Index == nil || ev.Index.Chunks != 1 {
		t.Errorf("rebuild-index: %s", stdout)
	}

	// A failure is still one object, with ok=false, and a non-zero exit.
	stdout, stderr, err := run(t, "restore", "--json", "--repo", repoDir, "snapshots/nobody/20260101T000000000000000Z", filepath.Join(t.TempDir(), "x"))
	if err == nil {
		t.Fatal("restore of a missing snapshot succeeded")
	}
	if ev := decode(t, stdout); ev.OK || ev.Error == "" || ev.Kind != "restore" {
		t.Errorf("failed restore: stdout %s stderr %s", stdout, stderr)
	}
}

// backup --parity writes parity; check --repair uses it; the JSON says so.
func TestParityRepairFromTheCommandLine(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")
	repoDir := filepath.Join(t.TempDir(), "repo")
	source := t.TempDir()
	if err := os.WriteFile(filepath.Join(source, "f.txt"), []byte(strings.Repeat("parity ", 5000)), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, _, err := run(t, "init", "--repo", repoDir); err != nil {
		t.Fatal(err)
	}
	if _, _, err := run(t, "backup", "--repo", repoDir, "--parity", "2", source); err != nil {
		t.Fatal(err)
	}
	stdout, _, err := run(t, "check", "--json", "--repo", repoDir)
	if err != nil {
		t.Fatal(err)
	}
	var ev report.Event
	if err := json.Unmarshal([]byte(stdout), &ev); err != nil || ev.Check.ParityPacks != 1 {
		t.Fatalf("check: %v %s", err, stdout)
	}

	// Flip a byte in the one pack.
	entries, err := os.ReadDir(filepath.Join(repoDir, "packs"))
	if err != nil || len(entries) != 1 {
		t.Fatalf("packs: %v %v", entries, err)
	}
	path := filepath.Join(repoDir, "packs", entries[0].Name())
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	data[len(data)/2] ^= 0x01
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}

	if _, _, err := run(t, "check", "--repo", repoDir, "--read-data"); err == nil {
		t.Fatal("check did not fail on the damaged pack")
	}
	stdout, _, err = run(t, "check", "--json", "--repo", repoDir, "--repair")
	if err != nil {
		t.Fatalf("check --repair: %v\n%s", err, stdout)
	}
	if err := json.Unmarshal([]byte(stdout), &ev); err != nil || !ev.OK || len(ev.Check.Repaired) != 1 || ev.Check.Repaired[0] != entries[0].Name() {
		t.Fatalf("check --repair: %v %s", err, stdout)
	}
	if _, _, err := run(t, "check", "--repo", repoDir, "--read-data"); err != nil {
		t.Fatalf("check after repair: %v", err)
	}
}

// check must exit non-zero when it finds something, or a cron job that
// runs it learns nothing.
func TestCheckExitsNonZeroOnDamage(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")

	repoDir := filepath.Join(t.TempDir(), "repo")
	source := t.TempDir()
	if err := os.WriteFile(filepath.Join(source, "a.txt"), []byte("payload"), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}

	if _, _, err := run(t, "init", "--repo", repoDir); err != nil {
		t.Fatalf("init: %v", err)
	}
	if _, _, err := run(t, "backup", "--repo", repoDir, source); err != nil {
		t.Fatalf("backup: %v", err)
	}

	packs, err := filepath.Glob(filepath.Join(repoDir, "packs", "*"))
	if err != nil || len(packs) == 0 {
		t.Fatalf("glob packs: %v (%d found)", err, len(packs))
	}
	if err := os.Remove(packs[0]); err != nil {
		t.Fatalf("remove pack: %v", err)
	}

	stdout, _, err := run(t, "check", "--repo", repoDir)
	if err == nil {
		t.Fatalf("check on a damaged repository: got nil error, output %q", stdout)
	}
	if !strings.Contains(stdout, "problem:") {
		t.Errorf("check printed %q, want a problem line", stdout)
	}
}

func TestCommandsRequireARepository(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")
	t.Setenv(RepositoryEnv, "")

	for _, args := range [][]string{
		{"backup", "/tmp"},
		{"snapshots"},
		{"check"},
		{"restore", "snapshots/x/y", "/tmp/out"},
		{"rebuild-index"},
		{"init"},
	} {
		t.Run(args[0], func(t *testing.T) {
			if _, _, err := run(t, args...); err == nil || !strings.Contains(err.Error(), "no repository given") {
				t.Fatalf("%v: err = %v, want a missing-repository error", args, err)
			}
		})
	}
}

func TestRepositoryComesFromTheEnvironment(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")

	repoDir := filepath.Join(t.TempDir(), "repo")
	t.Setenv(RepositoryEnv, repoDir)

	if _, _, err := run(t, "init"); err != nil {
		t.Fatalf("init: %v", err)
	}
	if _, _, err := run(t, "snapshots"); err != nil {
		t.Fatalf("snapshots: %v", err)
	}
}

func TestUnsupportedSchemeIsRejectedClearly(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")

	_, _, err := run(t, "snapshots", "--repo", "ftp://host/path")
	if err == nil || !strings.Contains(err.Error(), "not supported") {
		t.Fatalf("err = %v, want a clear unsupported-scheme error", err)
	}
}

func TestPasswordFileIsUsed(t *testing.T) {
	repoDir := filepath.Join(t.TempDir(), "repo")
	passwordFile := filepath.Join(t.TempDir(), "pw")
	if err := os.WriteFile(passwordFile, []byte("from the file\n"), 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}

	if _, _, err := run(t, "init", "--repo", repoDir, "--password-file", passwordFile); err != nil {
		t.Fatalf("init: %v", err)
	}

	// The trailing newline must be stripped, or the same file would not
	// reopen the repository it just created.
	if _, _, err := run(t, "snapshots", "--repo", repoDir, "--password-file", passwordFile); err != nil {
		t.Fatalf("snapshots: %v", err)
	}

	t.Setenv(PasswordEnv, "a different password")
	if _, _, err := run(t, "snapshots", "--repo", repoDir); err == nil {
		t.Fatal("opening with the wrong password succeeded")
	}
}

func TestHumanBytes(t *testing.T) {
	for in, want := range map[uint64]string{
		0:           "0 B",
		1023:        "1023 B",
		1024:        "1.0 KiB",
		1536:        "1.5 KiB",
		1024 * 1024: "1.0 MiB",
		3 * 1 << 30: "3.0 GiB",
		5 * 1 << 40: "5.0 TiB",
		2 * 1 << 50: "2.0 PiB",
	} {
		if got := humanBytes(in); got != want {
			t.Errorf("humanBytes(%d) = %q, want %q", in, got, want)
		}
	}
}

func snapshotKey(t *testing.T, listing string) string {
	t.Helper()

	for _, line := range strings.Split(listing, "\n") {
		for _, field := range strings.Fields(line) {
			if strings.HasPrefix(field, "snapshots/") {
				return field
			}
		}
	}
	t.Fatalf("no snapshot key in listing:\n%s", listing)
	return ""
}

// A flag validation failure still honours the JSON contract: one object
// on stdout saying what happened, the error for the exit code.
func TestPruneRejectsBadFlagsButStillSpeaksJSON(t *testing.T) {
	t.Setenv(PasswordEnv, "a test password")

	for _, flag := range []string{"--grace=0", "--forget-clients-after=0", "--clock-skew=-1s"} {
		stdout, _, err := run(t, "prune", "--json", flag)
		if err == nil {
			t.Errorf("%s: accepted", flag)
		}
		var ev report.Event
		if jsonErr := json.Unmarshal([]byte(stdout), &ev); jsonErr != nil {
			t.Errorf("%s: stdout is not one JSON object: %q (%v)", flag, stdout, jsonErr)
			continue
		}
		if ev.OK || ev.Kind != "prune" || ev.Error == "" {
			t.Errorf("%s: event = %+v", flag, ev)
		}
	}

	repoDir := filepath.Join(t.TempDir(), "repo")
	if _, _, err := run(t, "init", "--repo", repoDir); err != nil {
		t.Fatal(err)
	}
	// Zero skew is a real setting for clients that share a clock: it
	// passes validation and runs.
	if _, _, err := run(t, "prune", "--repo", repoDir, "--clock-skew=0"); err != nil {
		t.Errorf("clock-skew 0 is a real setting, got %v", err)
	}
}
