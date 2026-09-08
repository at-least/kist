package repo

import (
	"testing"
)

func TestProbeRace(t *testing.T) {
	s := newScenario(t)
	src := s.source("one", 300<<10)
	a := s.open(clientA)
	s.forget(a, s.backup(a, src))

	client := s.open(clientA)
	p := s.pruner()
	var marked PruneReport
	backupHooks.afterMarks = func() {
		backupHooks.afterMarks = nil
		marked = s.prune(p, shortGrace)
	}
	defer func() { backupHooks.afterMarks = nil }()
	s.backup(client, src)
	t.Logf("inner marked: %d, packs: %d", len(marked.Marked), s.packs())
	m := s.marks()
	t.Logf("marks after backup: %d", len(m))
	for _, id := range m {
		t.Logf("  mark %s", id)
	}
}
