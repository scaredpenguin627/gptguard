package service

import (
	"bytes"
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"

	"github.com/gylive/ccodex-sleep-state/internal/codexconfig"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
	"github.com/pelletier/go-toml/v2"
)

func fixtureControl(t *testing.T, source, auth, profile string) *control {
	t.Helper()
	home, data := t.TempDir(), t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(source), 0600); err != nil {
		t.Fatal(err)
	}
	if auth != "" {
		if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(auth), 0600); err != nil {
			t.Fatal(err)
		}
	}
	cfg := settings.Default()
	cfg.CodexHome = home
	cfg.CodexProfile = profile
	c := &control{ctx: context.Background(), dir: data, config: cfg, configure: true, log: slog.New(slog.NewTextHandler(io.Discard, nil))}
	t.Cleanup(func() { _ = codexconfig.Restore(data) })
	return c
}

func TestProviderSetupIntegration(t *testing.T) {
	relay := "model_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\n"
	for _, tt := range []struct {
		name, source, auth, profile, upstream, kind string
		requires                                    bool
	}{
		{name: "official", auth: `{"tokens":{"access_token":"fixture-oauth"}}`, upstream: "https://chatgpt.com/backend-api/codex", kind: "official", requires: true},
		{name: "relay-env", source: relay + "env_key='FIXTURE_RELAY_KEY'\n", upstream: "https://relay.invalid/v1", kind: "relay"},
		{name: "ccs-key", source: relay + "requires_openai_auth=true\n", auth: `{"OPENAI_API_KEY":"fixture-key"}`, upstream: "https://relay.invalid/v1", kind: "relay", requires: true},
		{name: "official-api-key", auth: `{"OPENAI_API_KEY":"fixture-key"}`, upstream: "https://api.openai.com/v1", kind: "relay", requires: true},
		{name: "explicit-profile", source: "model_provider='openai'\n[profiles.work]\nmodel_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\nenv_key='FIXTURE_RELAY_KEY'\n", profile: "work", upstream: "https://relay.invalid/v1", kind: "relay"},
	} {
		t.Run(tt.name, func(t *testing.T) {
			c := fixtureControl(t, tt.source, tt.auth, tt.profile)
			c.setup()
			if !c.managed || c.setupError != "" {
				t.Fatal(c.setupError)
			}
			if c.targetURL != tt.upstream || c.targetKind != tt.kind {
				t.Fatalf("wrong routing: %q %q", c.targetURL, c.targetKind)
			}
			if tt.kind == "relay" && !c.effective().InjectionDisabled {
				t.Fatal("relay injection enabled")
			}
			if err := c.checkManaged(); err != nil {
				t.Fatal(err)
			}
			patched, err := os.ReadFile(filepath.Join(c.config.CodexHome, "config.toml"))
			if err != nil {
				t.Fatal(err)
			}
			var doc map[string]any
			if err = toml.Unmarshal(patched, &doc); err != nil {
				t.Fatal(err)
			}
			managed := doc["model_providers"].(map[string]any)["ccodex-sleep-state"].(map[string]any)
			if managed["requires_openai_auth"] != tt.requires {
				t.Fatal("authentication policy changed")
			}
			if tt.profile != "" && doc["model_provider"] != "openai" {
				t.Fatal("root provider unexpectedly changed")
			}
		})
	}
}

func TestProviderSetupRejectsOAuthRelay(t *testing.T) {
	original := "model_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\nrequires_openai_auth=true\n"
	c := fixtureControl(t, original, `{"tokens":{"access_token":"fixture-oauth"}}`, "")
	c.setup()
	if c.managed || c.setupError == "" {
		t.Fatal("OAuth relay accepted")
	}
	got, _ := os.ReadFile(filepath.Join(c.config.CodexHome, "config.toml"))
	if !bytes.Equal(got, []byte(original)) {
		t.Fatal("rejected setup changed config")
	}
}

func TestProviderSwitchBlockedBeforeForwarding(t *testing.T) {
	for _, mode := range []string{"config", "auth"} {
		t.Run(mode, func(t *testing.T) {
			c := fixtureControl(t, "", `{"tokens":{"access_token":"fixture-oauth"}}`, "")
			c.setup()
			if !c.managed {
				t.Fatal(c.setupError)
			}
			if mode == "config" {
				os.WriteFile(filepath.Join(c.config.CodexHome, "config.toml"), []byte("model_provider='ccs'"), 0600)
			} else {
				os.WriteFile(filepath.Join(c.config.CodexHome, "auth.json"), []byte(`{"OPENAI_API_KEY":"fixture-key"}`), 0600)
			}
			recorder := httptest.NewRecorder()
			c.ServeHTTP(recorder, httptest.NewRequest(http.MethodPost, "http://127.0.0.1/backend-api/codex/responses", nil))
			if recorder.Code != http.StatusConflict {
				t.Fatalf("switch not blocked: %d", recorder.Code)
			}
		})
	}
}

func TestProviderInstallRejectsStaleResolution(t *testing.T) {
	c := fixtureControl(t, "", "", "")
	selected, err := codexconfig.ResolveWithAuth(nil, "", "unknown")
	if err != nil {
		t.Fatal(err)
	}
	changed := []byte("# switched by config manager\nmodel_provider='openai'\n")
	target := filepath.Join(c.config.CodexHome, "config.toml")
	os.WriteFile(target, changed, 0600)
	err = codexconfig.InstallWithOptions(c.dir, c.config.CodexHome, "http://127.0.0.1:17841/backend-api/codex", codexconfig.Options{ExpectedConfigSHA256: selected.ConfigSHA256})
	if err == nil {
		t.Fatal("stale resolution installed")
	}
	got, _ := os.ReadFile(target)
	if !bytes.Equal(got, changed) {
		t.Fatal("manager changes overwritten")
	}
}
