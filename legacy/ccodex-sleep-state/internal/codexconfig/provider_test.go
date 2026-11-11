package codexconfig

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/pelletier/go-toml/v2"
)

func TestResolveProvider(t *testing.T) {
	for _, tt := range []struct {
		name, source, upstream, auth string
		official                     bool
	}{
		{"default", "", "https://chatgpt.com/backend-api/codex", "chatgpt", true},
		{"relay", "model_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\nenv_key='MY_KEY'\n", "https://relay.invalid/v1", "api_key", false},
		{"ccs", "model_provider='ccs'\n[model_providers.ccs]\nbase_url='http://127.0.0.1:9000/v1'\nrequires_openai_auth=false\n", "http://127.0.0.1:9000/v1", "api_key", false},
		{"profile", "model_provider='openai'\nprofile='work'\n[profiles.work]\nmodel_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\n", "https://relay.invalid/v1", "api_key", false},
	} {
		t.Run(tt.name, func(t *testing.T) {
			s, err := Resolve([]byte(tt.source), "")
			if err != nil {
				t.Fatal(err)
			}
			if s.Upstream != tt.upstream || s.AuthKind != tt.auth || s.Official != tt.official {
				t.Fatalf("unexpected selection: %#v", s)
			}
			data, _ := json.Marshal(s)
			if strings.Contains(string(data), tt.upstream) {
				t.Fatal("upstream leaked in public JSON")
			}
		})
	}
}
func TestRejectUnsafeOrAmbiguousProvider(t *testing.T) {
	for _, source := range []string{
		"model_provider='missing'",
		"profile='missing'",
		"openai_base_url='https://relay.invalid/v1'",
		"model_provider='r'\n[model_providers.r]\nbase_url='https://relay.invalid/v1'\nrequires_openai_auth=true",
		"model_provider='r'\n[model_providers.r]\nbase_url='https://key:secret@relay.invalid/v1'",
		"model_provider='r'\n[model_providers.r]\nbase_url='https://relay.invalid/v1?key=secret'",
		"model_provider='r'\n[model_providers.r]\nbase_url='https://relay.invalid/v1'\nwire_api='chat'",
	} {
		if _, err := Resolve([]byte(source), ""); err == nil {
			t.Errorf("accepted %s", source)
		}
	}
}
func TestPatchSelectedProfileAndAuthentication(t *testing.T) {
	original := []byte("model='root-model'\nmodel_provider='openai'\n[profiles.work]\nmodel_provider='relay' # preserve\n[profiles.other]\nmodel='other-model'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\nenv_key='RELAY_TOKEN'\nrequires_openai_auth=false\n[model_providers.relay.env_http_headers]\nX-Custom='HEADER_ENV'\n")
	patched, err := PatchWithOptions(original, "http://127.0.0.1:17841/backend-api/codex", Options{Profile: "work"})
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	if err = toml.Unmarshal(patched, &doc); err != nil {
		t.Fatal(err)
	}
	if doc["model"] != "root-model" || doc["model_provider"] != "openai" {
		t.Fatal("root provider overwritten")
	}
	profiles := doc["profiles"].(map[string]any)
	work := profiles["work"].(map[string]any)
	if work["model_provider"] != provider || work["model"] != "gpt-6-astra" {
		t.Fatalf("profile not patched: %s", patched)
	}
	providers := doc["model_providers"].(map[string]any)
	managed := providers[provider].(map[string]any)
	if managed["env_key"] != "RELAY_TOKEN" || managed["requires_openai_auth"] != false || managed["env_http_headers"].(map[string]any)["X-Custom"] != "HEADER_ENV" {
		t.Fatal("relay authentication lost")
	}
	if !strings.Contains(string(patched), "# preserve") {
		t.Fatal("comment lost")
	}
}
func TestCheckManagedDetectsCCSSwitch(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	if CheckManaged(dir) == nil {
		t.Fatal("missing receipt accepted")
	}
	if err := Install(dir, home, "http://127.0.0.1:17841/backend-api/codex"); err != nil {
		t.Fatal(err)
	}
	if err := CheckManaged(dir); err != nil {
		t.Fatal(err)
	}
	os.WriteFile(filepath.Join(home, "config.toml"), []byte("model_provider='ccs'"), 0600)
	if CheckManaged(dir) == nil {
		t.Fatal("CCS switch not detected")
	}
	if Restore(dir) == nil {
		t.Fatal("CCS switch overwritten")
	}
}

func TestInstallBindsResolvedProvider(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	selection, err := Resolve(nil, "")
	if err != nil {
		t.Fatal(err)
	}
	changed := []byte("model_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\n")
	target := filepath.Join(home, "config.toml")
	if err = os.WriteFile(target, changed, 0600); err != nil {
		t.Fatal(err)
	}
	err = InstallWithOptions(dir, home, "http://127.0.0.1:17841/backend-api/codex", Options{ExpectedConfigSHA256: selection.ConfigSHA256})
	if err == nil {
		t.Fatal("provider changed after Resolve but install proceeded")
	}
	got, _ := os.ReadFile(target)
	if string(got) != string(changed) {
		t.Fatal("changed configuration overwritten")
	}
	if _, err = os.Stat(journal(dir)); !os.IsNotExist(err) {
		t.Fatal("rejected install left a transaction")
	}
}

func TestExplicitProfileOverridesDefault(t *testing.T) {
	source := []byte("profile='home'\n[profiles.home]\nmodel_provider='openai'\n[profiles.work]\nmodel_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\n")
	selected, err := Resolve(source, "work")
	if err != nil {
		t.Fatal(err)
	}
	if selected.Profile != "work" || selected.ProviderID != "relay" {
		t.Fatal("explicit profile ignored")
	}
	patched, err := PatchWithOptions(source, "http://127.0.0.1:17841/backend-api/codex", Options{Profile: "work"})
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	toml.Unmarshal(patched, &doc)
	if doc["profile"] != "home" {
		t.Fatal("default profile changed without consent")
	}
}

func TestPatchExplicitProvidersParent(t *testing.T) {
	source := []byte("model_provider='relay'\n[model_providers]\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\n")
	if _, err := Patch(source, "http://127.0.0.1:17841/backend-api/codex"); err != nil {
		t.Fatal(err)
	}
}
