package report

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/at-least/kist/internal/repo"
)

// The README documents the backup sub-object's field names in snake_case
// (`bytes_stored chunks_new packs_revived ...`) and says --json and the
// webhook post the same shapes. repo.BackupReport carried no json tags,
// so it marshalled as nested PascalCase and every consumer written
// against the README broke.
func TestBackupReportJSONIsSnakeCase(t *testing.T) {
	b, err := json.Marshal(repo.BackupReport{
		ChunksNew:    1,
		ChunksRead:   2,
		PacksNew:     3,
		PacksRevived: 4,
		BytesStored:  5,
		Errors:       6,
		FilesReused:  7,
	})
	if err != nil {
		t.Fatal(err)
	}
	s := string(b)
	for _, want := range []string{
		`"chunks_new":1`, `"chunks_read":2`, `"packs_new":3`,
		`"packs_revived":4`, `"bytes_stored":5`, `"errors":6`,
		`"files_reused":7`,
	} {
		if !strings.Contains(s, want) {
			t.Fatalf("BackupReport JSON must use the documented snake_case names, got %s", s)
		}
	}
	if strings.ContainsAny(s, "ABCDEFGHIJKLMNOPQRSTUVWXYZ") {
		t.Fatalf("no PascalCase keys may remain, got %s", s)
	}
}
