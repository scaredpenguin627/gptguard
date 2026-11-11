package codexconfig

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/pelletier/go-toml/v2"
)

func installedFixture(t *testing.T, source string, options Options) (string, string, []byte) {
	t.Helper()
	dir, home := t.TempDir(), t.TempDir()
	target := filepath.Join(home, "config.toml")
	if err := os.WriteFile(target, []byte(source), 0600); err != nil {
		t.Fatal(err)
	}
	if err := InstallWithOptions(dir, home, "http://127.0.0.1:17841/backend-api/codex", options); err != nil {
		t.Fatal(err)
	}
	current, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	return dir, target, current
}

func TestUnrelatedEditsRemainManagedAndSurviveRestore(t *testing.T) {
	source := "# my settings\nmodel='old' # keep\n[features]\nfoo=true\n"
	dir, target, current := installedFixture(t, source, Options{})
	current = bytes.Replace(current, []byte("foo=true"), []byte("foo=false # user update"), 1)
	current = append([]byte("# added by CCS\n"), current...)
	current = append(current, []byte("\n[projects.'/new/project']\ntrust_level='trusted'\n")...)
	os.WriteFile(target, current, 0600)
	if err := CheckManaged(dir); err != nil {
		t.Fatal(err)
	}
	if err := Restore(dir); err != nil {
		t.Fatal(err)
	}
	restored, _ := os.ReadFile(target)
	for _, wanted := range []string{"# added by CCS", "model='old' # keep", "foo=false # user update", "[projects.'/new/project']"} {
		if !bytes.Contains(restored, []byte(wanted)) {
			t.Fatalf("lost %q: %s", wanted, restored)
		}
	}
	var doc map[string]any
	toml.Unmarshal(restored, &doc)
	if referencesManaged(doc) || tableAt(doc, "model_providers", provider) != nil {
		t.Fatal("managed references remain")
	}
}

func TestSemanticGuardRejectsOwnedRoutingAndAuthChanges(t *testing.T) {
	for _, change := range []struct{ old, new string }{
		{"name = 'OpenAI'", "name = 'Other'"},
		{"http://127.0.0.1:17841/backend-api/codex", "http://127.0.0.1:18888/backend-api/codex"},
		{"requires_openai_auth = true", "requires_openai_auth = false"},
		{"gpt-6-astra", "gpt-5.6-sol"},
	} {
		t.Run(change.old, func(t *testing.T) {
			dir, target, current := installedFixture(t, "", Options{})
			changed := bytes.Replace(current, []byte(change.old), []byte(change.new), 1)
			if bytes.Equal(changed, current) {
				t.Fatal("test mutation missing")
			}
			os.WriteFile(target, changed, 0600)
			if CheckManaged(dir) == nil {
				t.Fatal("routing edit accepted")
			}
			if Restore(dir) == nil {
				t.Fatal("routing edit overwritten")
			}
			got, _ := os.ReadFile(target)
			if !bytes.Equal(got, changed) {
				t.Fatal("changed contents overwritten")
			}
		})
	}
}

func TestOriginalUpstreamAndProfileAreGuarded(t *testing.T) {
	source := "profile='work'\n[profiles.work]\nmodel_provider='relay'\n[profiles.other]\nmodel='other'\n[model_providers.relay]\nname='My relay'\nbase_url='https://old.invalid/v1'\nenv_key='MY_KEY'\n"
	for _, change := range []struct{ old, new string }{{"https://old.invalid/v1", "https://new.invalid/v1"}, {"profile='work'", "profile='other'"}, {"env_key='MY_KEY'", "env_key='OTHER_KEY'"}} {
		dir, target, current := installedFixture(t, source, Options{})
		os.WriteFile(target, bytes.Replace(current, []byte(change.old), []byte(change.new), 1), 0600)
		if CheckManaged(dir) == nil {
			t.Fatal("upstream change accepted")
		}
	}
	dir, target, current := installedFixture(t, source, Options{})
	os.WriteFile(target, bytes.Replace(current, []byte("model='other'"), []byte("model='changed-unrelated'"), 1), 0600)
	if err := CheckManaged(dir); err != nil {
		t.Fatal(err)
	}
	if err := Restore(dir); err != nil {
		t.Fatal(err)
	}
	got, _ := os.ReadFile(target)
	if !bytes.Contains(got, []byte("model='changed-unrelated'")) {
		t.Fatal("other profile lost")
	}
}

func TestExplicitKeepCurrentCCSSwitch(t *testing.T) {
	source := "model='original'\nmodel_provider='openai'\n[model_providers.relay]\nname='My relay'\nbase_url='https://relay.invalid/v1'\nenv_key='RELAY_KEY'\n"
	dir, target, current := installedFixture(t, source, Options{})
	current = bytes.Replace(current, []byte("model_provider=\"ccodex-sleep-state\""), []byte("model_provider='relay'"), 1)
	if !bytes.Contains(current, []byte("model_provider='relay'")) {
		t.Fatal("mutation missing")
	}
	current = append([]byte("# CCS chose relay\n"), current...)
	os.WriteFile(target, current, 0600)
	if Restore(dir) == nil {
		t.Fatal("automatic recovery should not handle provider switches")
	}
	preview, err := PreviewRecovery(dir)
	if err != nil {
		t.Fatal(err)
	}
	if !preview.CanKeepCurrent {
		t.Fatalf("cannot recover: %#v", preview)
	}
	result, err := Recover(dir, RecoveryOptions{Mode: "keep_current", ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedTransactionSHA256: preview.TransactionSHA256})
	if err != nil {
		t.Fatal(err)
	}
	got, _ := os.ReadFile(target)
	var doc map[string]any
	toml.Unmarshal(got, &doc)
	if doc["model_provider"] != "relay" || doc["model"] != "original" || doc["openai_base_url"] != nil || tableAt(doc, "model_providers", provider) != nil {
		t.Fatalf("incorrect merge: %s", got)
	}
	if !bytes.Contains(got, []byte("# CCS chose relay")) {
		t.Fatal("comment lost")
	}
	archived, _ := os.ReadFile(filepath.Join(result.Archive, "current.toml"))
	if !bytes.Equal(archived, current) {
		t.Fatal("current backup missing")
	}
	if _, err = os.Stat(filepath.Join(result.Archive, "transaction.json")); err != nil {
		t.Fatal("receipt archive missing")
	}
	if _, err = os.Stat(journal(dir)); !os.IsNotExist(err) {
		t.Fatal("receipt not retired")
	}
}

func TestRecoveryRequiresFreshPreview(t *testing.T) {
	dir, target, current := installedFixture(t, "model='old'\n", Options{})
	preview, err := PreviewRecovery(dir)
	if err != nil {
		t.Fatal(err)
	}
	current = append([]byte("# changed after preview\n"), current...)
	os.WriteFile(target, current, 0600)
	_, err = Recover(dir, RecoveryOptions{Mode: "restore_backup", ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedTransactionSHA256: preview.TransactionSHA256})
	if err == nil {
		t.Fatal("stale confirmation accepted")
	}
	got, _ := os.ReadFile(target)
	if !bytes.Equal(got, current) {
		t.Fatal("new edit overwritten")
	}
}

func TestFullRecoveryArchivesChangedManagedProvider(t *testing.T) {
	original := "# original\nmodel='old'\n"
	dir, target, current := installedFixture(t, original, Options{})
	current = bytes.Replace(current, []byte("name = 'OpenAI'"), []byte("name = 'Changed'"), 1)
	os.WriteFile(target, current, 0600)
	preview, err := PreviewRecovery(dir)
	if err != nil {
		t.Fatal(err)
	}
	if preview.CanKeepCurrent || !preview.CanRestoreBackup {
		t.Fatal("unsafe recovery options")
	}
	result, err := Recover(dir, RecoveryOptions{Mode: "restore_backup", ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedTransactionSHA256: preview.TransactionSHA256})
	if err != nil {
		t.Fatal(err)
	}
	got, _ := os.ReadFile(target)
	if string(got) != original {
		t.Fatal("original not restored")
	}
	saved, _ := os.ReadFile(filepath.Join(result.Archive, "current.toml"))
	if !bytes.Equal(saved, current) {
		t.Fatal("changed file not archived")
	}
}

func TestLegacyReceiptUsesChecksumVerifiedReconstruction(t *testing.T) {
	original := "model='old'\n"
	dir, target, current := installedFixture(t, original, Options{})
	current = bytes.Replace(current, []byte("name = 'OpenAI'"), []byte("name = 'Sleep State (local)'"), 1)
	raw, _ := os.ReadFile(journal(dir))
	var r receipt
	json.Unmarshal(raw, &r)
	r.Snapshot = ""
	r.Installed = digest(current)
	raw, _ = json.Marshal(r)
	os.WriteFile(journal(dir), raw, 0600)
	current = append([]byte("# harmless legacy edit\n"), current...)
	os.WriteFile(target, current, 0600)
	if err := CheckManaged(dir); err != nil {
		t.Fatal(err)
	}
	if err := Restore(dir); err != nil {
		t.Fatal(err)
	}
	got, _ := os.ReadFile(target)
	if !strings.Contains(string(got), "# harmless legacy edit") || !strings.Contains(string(got), "model='old'") {
		t.Fatal("legacy restore lost edits")
	}
}

func TestLegacyReplacedConfigRequiresExplicitBackup(t *testing.T) {
	dir, target, _ := installedFixture(t, "# old\n", Options{})
	raw, _ := os.ReadFile(journal(dir))
	var r receipt
	json.Unmarshal(raw, &r)
	r.Snapshot = ""
	raw, _ = json.Marshal(r)
	os.WriteFile(journal(dir), raw, 0600)
	os.WriteFile(target, []byte("model_provider='openai'\nmodel='new'\n"), 0600)
	preview, err := PreviewRecovery(dir)
	if err != nil {
		t.Fatal(err)
	}
	if preview.CanKeepCurrent || !preview.CanRestoreBackup {
		t.Fatal("legacy replacement cannot be auto merged")
	}
	_, err = Recover(dir, RecoveryOptions{Mode: "restore_backup", ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedTransactionSHA256: preview.TransactionSHA256})
	if err != nil {
		t.Fatal(err)
	}
}

func TestPatchModelsAndProviderNames(t *testing.T) {
	for _, model := range []string{"gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra"} {
		data, err := PatchWithOptions(nil, "http://127.0.0.1:17841/backend-api/codex", Options{Model: model})
		if err != nil {
			t.Fatal(err)
		}
		var doc map[string]any
		toml.Unmarshal(data, &doc)
		if doc["model"] != model || tableAt(doc, "model_providers", provider)["name"] != "OpenAI" {
			t.Fatal("model or compaction capability lost")
		}
	}
	if _, err := PatchWithOptions(nil, "http://127.0.0.1:1", Options{Model: "not-supported"}); err == nil {
		t.Fatal("unsupported model accepted")
	}
	data, err := Patch([]byte("model_provider='r'\n[model_providers.r]\nname='Private relay'\nbase_url='https://relay.invalid/v1'\n"), "http://127.0.0.1:1")
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	toml.Unmarshal(data, &doc)
	if tableAt(doc, "model_providers", provider)["name"] != "Private relay" {
		t.Fatal("relay capability name overwritten")
	}
}

func TestRestoreKeepsCommentsInsideAndAfterManagedTable(t *testing.T) {
	dir, target, current := installedFixture(t, "model='old'\n", Options{})
	current = bytes.Replace(current, []byte("name = 'OpenAI'"), []byte("name = 'OpenAI' # custom inline note"), 1)
	current = append(current, []byte("# my footer note\n")...)
	os.WriteFile(target, current, 0600)
	if err := Restore(dir); err != nil {
		t.Fatal(err)
	}
	got, _ := os.ReadFile(target)
	for _, note := range []string{"# custom inline note", "# my footer note"} {
		if !bytes.Contains(got, []byte(note)) {
			t.Fatalf("comment lost: %s", got)
		}
	}
}

func TestSnapshotChecksumAndBackupChecksumProtectRecovery(t *testing.T) {
	for _, field := range []string{"backup", "snapshot"} {
		t.Run(field, func(t *testing.T) {
			dir, _, _ := installedFixture(t, "# original\n", Options{})
			raw, _ := os.ReadFile(journal(dir))
			var r receipt
			json.Unmarshal(raw, &r)
			path := r.Backup
			if field == "snapshot" {
				path = r.Snapshot
			}
			os.WriteFile(path, []byte("changed"), 0600)
			if _, err := PreviewRecovery(dir); err == nil {
				t.Fatal("damaged backup accepted")
			}
			if err := Restore(dir); err == nil {
				t.Fatal("damaged backup restored")
			}
		})
	}
}

func TestProfileInheritedRoutingIsGuarded(t *testing.T) {
	source := "model_provider='r'\nopenai_base_url='https://relay.invalid/v1'\nprofile='work'\n[profiles.work]\nmodel='old'\n[model_providers.r]\nenv_key='KEY'\n"
	dir, target, current := installedFixture(t, source, Options{})
	current = bytes.Replace(current, []byte("https://relay.invalid/v1"), []byte("https://changed.invalid/v1"), 1)
	os.WriteFile(target, current, 0600)
	if err := CheckManaged(dir); err == nil {
		t.Fatal("inherited routing change went unnoticed")
	}
}
