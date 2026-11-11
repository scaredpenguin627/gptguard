package codexconfig

import (
	"bytes"
	"os"
	"path/filepath"
	"testing"
)

func TestExplicitRebuildBacksUpMalformedConfig(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	target := filepath.Join(home, "config.toml")
	malformed := []byte("# my broken config\nmodel = [\nsecret = 'do not lose'\n")
	os.WriteFile(target, malformed, 0600)
	auth := []byte(`{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-not-a-real-token"}}`)
	authPath := filepath.Join(home, "auth.json")
	os.WriteFile(authPath, auth, 0600)
	preview, err := PreviewCleanConfig(dir, home)
	if err != nil {
		t.Fatal(err)
	}
	if preview.ValidTOML || !preview.Exists {
		t.Fatal("invalid file misidentified")
	}
	replacement := []byte("model_provider='openai'\nmodel='gpt-6-astra'\n")
	result, err := ResetConfig(dir, home, CleanConfigOptions{ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedExists: preview.Exists, Replacement: replacement, AuthMode: "chatgpt"})
	if err != nil {
		t.Fatal(err)
	}
	got, _ := os.ReadFile(target)
	if !bytes.Equal(got, replacement) {
		t.Fatal("replacement not installed")
	}
	saved, _ := os.ReadFile(filepath.Join(result.Archive, "current.toml"))
	if !bytes.Equal(saved, malformed) {
		t.Fatal("malformed backup lost")
	}
	afterAuth, _ := os.ReadFile(authPath)
	if !bytes.Equal(afterAuth, auth) {
		t.Fatal("auth changed")
	}
}

func TestRebuildRefusesPendingTransactionAndAmbiguousAuth(t *testing.T) {
	dir, target, _ := installedFixture(t, "model='original'\n", Options{})
	home := filepath.Dir(target)
	if _, err := PreviewCleanConfig(dir, home); err == nil {
		t.Fatal("pending transaction bypassed")
	}
	if err := Restore(dir); err != nil {
		t.Fatal(err)
	}
	preview, _ := PreviewCleanConfig(dir, home)
	for _, mode := range []string{"", "unknown", "ambiguous"} {
		if _, err := ResetConfig(dir, home, CleanConfigOptions{ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedExists: preview.Exists, AuthMode: mode}); err == nil {
			t.Fatal("ambiguous auth accepted")
		}
	}
	relay := []byte("model_provider='relay'\n[model_providers.relay]\nbase_url='https://relay.invalid/v1'\nrequires_openai_auth=true\n")
	if _, err := ResetConfig(dir, home, CleanConfigOptions{ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedExists: preview.Exists, Replacement: relay, AuthMode: "chatgpt"}); err == nil {
		t.Fatal("account token could reach relay")
	}
}

func TestRebuildRefusesStaleConfirmationAndInvalidReplacement(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	target := filepath.Join(home, "config.toml")
	os.WriteFile(target, []byte("# original\n"), 0600)
	preview, _ := PreviewCleanConfig(dir, home)
	opts := CleanConfigOptions{ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedExists: true, AuthMode: "chatgpt", Replacement: []byte("model=[")}
	if _, err := ResetConfig(dir, home, opts); err == nil {
		t.Fatal("invalid replacement accepted")
	}
	opts.Replacement = []byte("model_provider='openai'\n")
	changed := []byte("# user changed it\n")
	os.WriteFile(target, changed, 0600)
	if _, err := ResetConfig(dir, home, opts); err == nil {
		t.Fatal("stale preview accepted")
	}
	got, _ := os.ReadFile(target)
	if !bytes.Equal(got, changed) {
		t.Fatal("new changes lost")
	}
}

func TestRebuildAbsentConfigAndSymlink(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	preview, err := PreviewCleanConfig(dir, home)
	if err != nil {
		t.Fatal(err)
	}
	if preview.Exists {
		t.Fatal("absent file reported present")
	}
	_, err = ResetConfig(dir, home, CleanConfigOptions{ExpectedConfigSHA256: preview.ConfigSHA256, ExpectedExists: false, AuthMode: "chatgpt", Replacement: []byte("model='gpt-6-astra'\n")})
	if err != nil {
		t.Fatal(err)
	}
	other := filepath.Join(t.TempDir(), "real.toml")
	os.WriteFile(other, []byte("# keep"), 0600)
	linkHome := t.TempDir()
	if err = os.Symlink(other, filepath.Join(linkHome, "config.toml")); err != nil {
		t.Skip("symlink privilege unavailable")
	}
	if _, err = PreviewCleanConfig(t.TempDir(), linkHome); err == nil {
		t.Fatal("symlink accepted")
	}
}
