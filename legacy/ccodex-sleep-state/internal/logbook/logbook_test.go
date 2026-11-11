package logbook

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRotationBounded(t *testing.T) {
	dir := t.TempDir()
	log, w, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	w.max = 100
	for i := 0; i < 20; i++ {
		log.Info("request_finished", "status", 200, "duration_ms", i)
	}
	if err = w.Close(); err != nil {
		t.Fatal(err)
	}
	files, _ := filepath.Glob(filepath.Join(dir, "service.jsonl*"))
	if len(files) != 4 {
		t.Fatalf("files=%d, want 4", len(files))
	}
	for _, f := range files {
		data, err := os.ReadFile(f)
		if err != nil || !strings.Contains(string(data), "request_finished") {
			t.Fatal("bad log")
		}
	}
}
