// Package metrics is a Prometheus text exposition with two metric kinds,
// hand-rolled: what kist has to say fits in counters and gauges, and the
// client library's dependency tree does not earn its place for that.
package metrics

import (
	"fmt"
	"net/http"
	"sort"
	"strings"
	"sync"
)

// Registry holds the metrics and serves them.
type Registry struct {
	mu     sync.Mutex
	series map[string]*series
	order  []string
}

type series struct {
	name, help, kind string
	values           map[string]float64 // keyed by rendered label set
}

// New returns an empty registry.
func New() *Registry { return &Registry{series: map[string]*series{}} }

func (r *Registry) get(name, help, kind string) *series {
	s, ok := r.series[name]
	if !ok {
		s = &series{name: name, help: help, kind: kind, values: map[string]float64{}}
		r.series[name] = s
		r.order = append(r.order, name)
	}
	return s
}

// Add increments a counter.
func (r *Registry) Add(name, help string, labels map[string]string, delta float64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	s := r.get(name, help, "counter")
	s.values[render(labels)] += delta
}

// Set sets a gauge.
func (r *Registry) Set(name, help string, labels map[string]string, value float64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	s := r.get(name, help, "gauge")
	s.values[render(labels)] = value
}

func render(labels map[string]string) string {
	if len(labels) == 0 {
		return ""
	}
	keys := make([]string, 0, len(labels))
	for k := range labels {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	parts := make([]string, 0, len(keys))
	for _, k := range keys {
		// Escaped by hand, once: %q would escape the escapes.
		v := strings.NewReplacer(`\`, `\\`, `"`, `\"`, "\n", `\n`).Replace(labels[k])
		parts = append(parts, k+`="`+v+`"`)
	}
	return "{" + strings.Join(parts, ",") + "}"
}

// Text renders the exposition format.
func (r *Registry) Text() string {
	r.mu.Lock()
	defer r.mu.Unlock()
	var b strings.Builder
	for _, name := range r.order {
		s := r.series[name]
		fmt.Fprintf(&b, "# HELP %s %s\n# TYPE %s %s\n", name, s.help, name, s.kind)
		keys := make([]string, 0, len(s.values))
		for k := range s.values {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			fmt.Fprintf(&b, "%s%s %g\n", name, k, s.values[k])
		}
	}
	return b.String()
}

// ServeHTTP serves the exposition.
func (r *Registry) ServeHTTP(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
	_, _ = fmt.Fprint(w, r.Text())
}
