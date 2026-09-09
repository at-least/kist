package metrics

import (
	"strings"
	"testing"
)

func TestTextExposition(t *testing.T) {
	r := New()
	r.Add("kist_runs_total", "Runs.", map[string]string{"job": `we"ird\name`, "result": "ok"}, 1)
	r.Add("kist_runs_total", "Runs.", map[string]string{"job": `we"ird\name`, "result": "ok"}, 1)
	r.Set("kist_last_run_duration_seconds", "Seconds.", nil, 1.5)
	got := r.Text()
	want := "# HELP kist_runs_total Runs.\n# TYPE kist_runs_total counter\n" +
		`kist_runs_total{job="we\"ird\\name",result="ok"} 2` + "\n" +
		"# HELP kist_last_run_duration_seconds Seconds.\n# TYPE kist_last_run_duration_seconds gauge\n" +
		"kist_last_run_duration_seconds 1.5\n"
	if got != want {
		t.Errorf("got:\n%s\nwant:\n%s", got, want)
	}
	if !strings.HasSuffix(got, "\n") {
		t.Error("exposition must end with a newline")
	}
}
