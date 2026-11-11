package codexconfig

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/pelletier/go-toml/v2"
)

func TestClassifyAuthFixtures(t *testing.T) {
	for _, tt := range []struct{ json, mode string }{
		{`{"OPENAI_API_KEY":"fixture-only"}`, "api_key"},
		{`{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-only","tokens":null}`, "api_key"},
		{`{"OPENAI_API_KEY":null,"tokens":{"access_token":"fixture-only"}}`, "chatgpt"},
		{`{"auth_mode":"chatgpt","tokens":{"id_token":"fixture-only"}}`, "chatgpt"},
		{`{"OPENAI_API_KEY":"fixture-only","tokens":{"access_token":"fixture-only"}}`, "ambiguous"},
		{`{"auth_mode":"chatgpt","OPENAI_API_KEY":"fixture-only"}`, "ambiguous"},
		{`{"OPENAI_API_KEY":123}`, "ambiguous"},
		{`{"tokens":{"access_token":false}}`, "ambiguous"},
		{`broken`, "ambiguous"},
		{`{"tokens":{},"OPENAI_API_KEY":""}`, "unknown"},
	} {
		if got := ClassifyAuth([]byte(tt.json)); got != tt.mode {
			t.Errorf("classification=%s want=%s", got, tt.mode)
		}
	}
}
func TestReadAuthModeOnlyFixture(t *testing.T) {
	home := t.TempDir()
	mode, err := ReadAuthMode(home)
	if err != nil || mode != "unknown" {
		t.Fatal(mode, err)
	}
	if err = os.WriteFile(filepath.Join(home, "auth.json"), []byte(`{"OPENAI_API_KEY":"fixture-only"}`), 0600); err != nil {
		t.Fatal(err)
	}
	mode, err = ReadAuthMode(home)
	if err != nil || mode != "api_key" {
		t.Fatal(mode, err)
	}
}
func TestRelayUsingCCSAPIKey(t *testing.T) {
	source := []byte("model_provider='ccs'\n[model_providers.ccs]\nbase_url='https://relay.invalid/v1'\nrequires_openai_auth=true\n")
	for _, mode := range []string{"unknown", "chatgpt", "ambiguous"} {
		if _, err := ResolveWithAuth(source, "", mode); err == nil {
			t.Errorf("unsafe auth accepted: %s", mode)
		}
	}
	selected, err := ResolveWithAuth(source, "", "api_key")
	if err != nil {
		t.Fatal(err)
	}
	if selected.AuthKind != "api_key" || selected.Official || selected.Upstream != "https://relay.invalid/v1" {
		t.Fatalf("wrong selection: %#v", selected)
	}
	patched, err := PatchWithOptions(source, "http://127.0.0.1:17841/backend-api/codex", Options{AuthMode: "api_key"})
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	toml.Unmarshal(patched, &doc)
	managed := doc["model_providers"].(map[string]any)[provider].(map[string]any)
	if managed["requires_openai_auth"] != true {
		t.Fatal("CCS key-based login lookup was lost")
	}
}
func TestBuiltinOpenAIAPIKeyEndpoint(t *testing.T) {
	selected, err := ResolveWithAuth(nil, "", "api_key")
	if err != nil {
		t.Fatal(err)
	}
	if selected.Upstream != "https://api.openai.com/v1" || selected.AuthKind != "api_key" || selected.Official {
		t.Fatalf("API key routed into ChatGPT: %#v", selected)
	}
	selected, err = ResolveWithAuth(nil, "", "chatgpt")
	if err != nil {
		t.Fatal(err)
	}
	if !selected.Official || selected.AuthKind != "chatgpt" {
		t.Fatal("ChatGPT login lost")
	}
}

func TestReadAuthModeRejectsNonRegularAndOversized(t *testing.T) {
	home := t.TempDir()
	path := filepath.Join(home, "auth.json")
	if err := os.Mkdir(path, 0700); err != nil {
		t.Fatal(err)
	}
	if _, err := ReadAuthMode(home); err == nil {
		t.Fatal("directory accepted")
	}
	home = t.TempDir()
	path = filepath.Join(home, "auth.json")
	if err := os.WriteFile(path, make([]byte, (1<<20)+1), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := ReadAuthMode(home); err == nil {
		t.Fatal("oversized file accepted")
	}
}
