package codexconfig

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/pelletier/go-toml/v2"
)

func TestPatchPreservesCommentsAndTables(t *testing.T) {
	source := []byte("# keep this\nmodel = 'previous' # keep inline\nmodel_provider = \"old\"\nmessage = '''\n[not_a_table]\n'''\n[model_providers.old]\nbase_url = 'https://example.invalid'\n[projects.'C:\\Work']\ntrust_level = 'trusted'\n")
	patched, err := Patch(source, "http://127.0.0.1:17841/backend-api/codex")
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"# keep this", "# keep inline", "[model_providers.old]", "[projects.'C:\\Work']", "[not_a_table]"} {
		if !bytes.Contains(patched, []byte(want)) {
			t.Errorf("lost %s", want)
		}
	}
	var document map[string]any
	if err = toml.Unmarshal(patched, &document); err != nil {
		t.Fatal(err)
	}
	if document["model"] != "gpt-6-astra" || document["model_provider"] != provider {
		t.Fatal("root settings incorrect")
	}
	if _, err = Patch(patched, "http://127.0.0.1:17841/backend-api/codex"); err == nil {
		t.Fatal("existing provider overwritten")
	}
}
func TestPatchEmptyAndMalformed(t *testing.T) {
	for _, source := range []string{"", "# only comments\n", "[features]\nfeature = true\n", "\"model\" = '''old'''\r\n"} {
		if _, err := Patch([]byte(source), "http://127.0.0.1:1/backend-api/codex"); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := Patch([]byte("model = [\n"), "http://127.0.0.1:1"); err == nil {
		t.Fatal("invalid TOML changed")
	}
}
func TestInstallRestoreRoundTrip(t *testing.T) {
	for _, exists := range []bool{true, false} {
		t.Run(map[bool]string{true: "existing", false: "absent"}[exists], func(t *testing.T) {
			dir, home := t.TempDir(), t.TempDir()
			target := filepath.Join(home, "config.toml")
			original := []byte("# user settings\nmodel = 'old'\n")
			if exists {
				if err := os.WriteFile(target, original, 0600); err != nil {
					t.Fatal(err)
				}
			}
			if err := Install(dir, home, "http://127.0.0.1:1/backend-api/codex"); err != nil {
				t.Fatal(err)
			}
			if err := Install(dir, home, "http://127.0.0.1:1/backend-api/codex"); err == nil {
				t.Fatal("second install accepted")
			}
			backups, _ := filepath.Glob(filepath.Join(home, "*.bak"))
			if len(backups) != 1 {
				t.Fatal("missing independent backup")
			}
			if err := Restore(dir); err != nil {
				t.Fatal(err)
			}
			got, err := os.ReadFile(target)
			if exists && (err != nil || !bytes.Equal(got, original)) {
				t.Fatal("original not restored")
			}
			if !exists && !os.IsNotExist(err) {
				t.Fatal("created config not removed")
			}
			if err = Restore(dir); err != nil {
				t.Fatal(err)
			}
		})
	}
}
func TestRestorePreservesUserEdits(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	if err := Install(dir, home, "http://127.0.0.1:1/backend-api/codex"); err != nil {
		t.Fatal(err)
	}
	target := filepath.Join(home, "config.toml")
	changed := []byte("# modified while running\nmodel = 'other'\n")
	if err := os.WriteFile(target, changed, 0600); err != nil {
		t.Fatal(err)
	}
	if err := Restore(dir); err == nil || !strings.Contains(err.Error(), "出于安全原因暂不覆盖") {
		t.Fatal("expected restore conflict")
	}
	current, _ := os.ReadFile(target)
	if !bytes.Equal(current, changed) {
		t.Fatal("user edit lost")
	}
	if _, err := os.Stat(journal(dir)); err != nil {
		t.Fatal("recovery receipt lost")
	}
}
func TestSymlinkNotReplaced(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	real := filepath.Join(t.TempDir(), "real.toml")
	os.WriteFile(real, []byte("# real\n"), 0600)
	target := filepath.Join(home, "config.toml")
	if err := os.Symlink(real, target); err != nil {
		t.Skip("symlink privilege unavailable")
	}
	if err := Install(dir, home, "http://127.0.0.1:1"); err == nil {
		t.Fatal("symlink accepted")
	}
	if info, err := os.Lstat(target); err != nil || info.Mode()&os.ModeSymlink == 0 {
		t.Fatal("symlink changed")
	}
}

// Regression: re-validating the patched document must not decode into the map
// already holding the original's array tables, which panics inside go-toml.
func TestPatchConfigWithArrayTables(t *testing.T) {
	source := []byte("model = 'old'\n[[skills.config]]\nname = 'first'\n[[skills.config]]\nname = 'second'\n")
	patched, err := Patch(source, "http://127.0.0.1:17841/backend-api/codex")
	if err != nil {
		t.Fatal(err)
	}
	var document map[string]any
	if err = toml.Unmarshal(patched, &document); err != nil {
		t.Fatal(err)
	}
	if document["model"] != "gpt-6-astra" || document["model_provider"] != provider {
		t.Fatal("root settings incorrect")
	}
	skills, _ := document["skills"].(map[string]any)
	if entries, _ := skills["config"].([]any); len(entries) != 2 {
		t.Fatalf("array table lost: %v", skills["config"])
	}
}
