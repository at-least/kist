package repo

import (
	"context"
	"testing"
)

func TestProbeGCState(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	h := s.backup(a, src)
	t.Logf("handle: %s", h.Key)
	s.forget(a, h)

	handles, err := a.Snapshots(context.Background(), "")
	if err != nil {
		t.Fatalf("snapshots: %v", err)
	}
	t.Logf("snapshots after forget: %d", len(handles))
	for _, hh := range handles {
		t.Logf("  %s", hh.Key)
	}

	p := s.pruner()
	first := s.prune(p, shortGrace)
	t.Logf("report: stored %d live %d marked %v treesMarked %v deleted %v", first.Stored, first.Live, first.Marked, first.TreesMarked, first.Deleted)
}
