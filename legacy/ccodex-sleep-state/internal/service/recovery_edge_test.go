package service

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/gylive/ccodex-sleep-state/internal/codexconfig"
	"github.com/gylive/ccodex-sleep-state/internal/proxyroute"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

type recoveryEdgeTransport func(*http.Request) (*http.Response, error)

func (f recoveryEdgeTransport) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

func startCapturedEngine(t *testing.T, c *control) *atomic.Int32 {
	t.Helper()
	calls := new(atomic.Int32)
	transport := &http.Transport{}
	transport.RegisterProtocol("https", recoveryEdgeTransport(func(r *http.Request) (*http.Response, error) {
		calls.Add(1)
		return &http.Response{StatusCode: 200, Header: http.Header{"Content-Type": []string{"application/json"}}, Body: io.NopCloser(strings.NewReader(`{"data":[]}`)), Request: r}, nil
	}))
	c.start([]proxyroute.Route{{ID: "fixture", Transport: transport}})
	t.Cleanup(func() { c.stop() })
	return calls
}

func edgeForward(c *control) *httptest.ResponseRecorder {
	req := httptest.NewRequest("GET", "http://127.0.0.1:17841/backend-api/codex/models", nil)
	req.Header.Set("Authorization", "Bearer synthetic-new-provider-key")
	w := httptest.NewRecorder()
	c.ServeHTTP(w, req)
	return w
}

func TestRecoveryRouteFailureCannotForwardThroughPreviousProvider(t *testing.T) {
	for _, mode := range []string{"rebuild", "recover_current"} {
		t.Run(mode, func(t *testing.T) {
			source := "model_provider='old'\n[model_providers.old]\nbase_url='https://old-provider.invalid/v1'\nenv_key='OLD_FIXTURE_KEY'\n"
			if mode == "rebuild" {
				source = "model=["
			}
			c := fixtureControl(t, source, `{"OPENAI_API_KEY":"synthetic-fixture-key"}`, "")
			c.config.InjectionDisabled = true
			if mode == "rebuild" {
				c.config.UpstreamKind = "relay"
				c.config.Upstream = "https://old-provider.invalid/v1"
			} else {
				c.setup()
				if !c.managed {
					t.Fatal(c.setupError)
				}
			}
			calls := startCapturedEngine(t, c)
			// A local source may disappear while the service is running. No live
			// network is used: both the old upstream and subscription failure are fake.
			c.config.Subscriptions = []settings.Source{{File: filepath.Join(t.TempDir(), "missing.yaml")}}
			h := controlHandler(c.config.Listen, panelTestToken, c)
			var response *httptest.ResponseRecorder
			if mode == "rebuild" {
				preview, err := c.cleanPreview()
				if err != nil {
					t.Fatal(err)
				}
				body, _ := json.Marshal(rebuildRequest{Kind: "relay", Upstream: "https://new-provider.invalid/v1", EnvKey: "NEW_FIXTURE_KEY", SHA: preview.ConfigSHA256, Exists: preview.Exists})
				response = panelPost(h, "codex-config/rebuild", string(body))
			} else {
				switched := []byte("model_provider='new'\n[model_providers.new]\nbase_url='https://new-provider.invalid/v1'\nenv_key='NEW_FIXTURE_KEY'\n")
				os.WriteFile(filepath.Join(c.config.CodexHome, "config.toml"), switched, 0600)
				preview, err := codexconfig.PreviewRecovery(c.dir)
				if err != nil {
					t.Fatal(err)
				}
				body, _ := json.Marshal(map[string]string{"mode": "keep_current", "expected_config_sha256": preview.ConfigSHA256, "expected_transaction_sha256": preview.TransactionSHA256})
				response = panelPost(h, "recovery/apply", string(body))
			}
			if response.Code != 400 {
				t.Fatalf("expected local route load failure, got %d: %s", response.Code, response.Body.String())
			}
			if c.targetURL != "https://new-provider.invalid/v1" {
				t.Fatal("test did not change the selected provider")
			}
			forwarded := edgeForward(c)
			if forwarded.Code != 503 || calls.Load() != 0 {
				t.Fatalf("new provider failed to load but stale upstream received request: HTTP=%d calls=%d route_error=%q", forwarded.Code, calls.Load(), c.routeError)
			}
			if c.cancel != nil {
				t.Fatal("stale background worker resumed after changed-provider load failure")
			}
		})
	}
}

func TestRescueResetDoesNotEnableUnchosenDirectRoute(t *testing.T) {
	c, _ := panelControl(t, nil)
	c.rescue = true
	original := []byte(`{"proxy_urls": ["broken private proxy"],`)
	os.WriteFile(c.path, original, 0600)
	preview, err := c.rescuePreview()
	if err != nil {
		t.Fatal(err)
	}
	if err = c.resetServiceConfig(preview["config_sha256"].(string)); err != nil {
		t.Fatal(err)
	}
	cfg, err := settings.Load(c.path)
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Direct {
		t.Fatal("reset silently enabled direct egress after losing the user's proxy settings")
	}
}

func TestPreferencesPersistenceFailureRollsBackManagedModel(t *testing.T) {
	c := fixtureControl(t, "model='original'\n", `{"tokens":{"access_token":"synthetic-oauth"}}`, "")
	c.path = filepath.Join(c.dir, "config.json")
	initial, _ := json.Marshal(c.config)
	os.WriteFile(c.path, initial, 0600)
	c.setup()
	if !c.managed {
		t.Fatal(c.setupError)
	}
	calls := startCapturedEngine(t, c)
	beforeEngine := c.engine
	target := filepath.Join(c.config.CodexHome, "config.toml")
	before, _ := os.ReadFile(target)
	external := c.config
	external.Direct = false
	edited, _ := json.Marshal(external)
	os.WriteFile(c.path, edited, 0600)
	h := controlHandler(c.config.Listen, panelTestToken, c)
	w := panelPost(h, "preferences", `{"model":"gpt-5.6-sol","account_mode":"auto","state_fallback":"strict"}`)
	if w.Code != 400 {
		t.Fatalf("expected persist conflict: %d %s", w.Code, w.Body.String())
	}
	after, _ := os.ReadFile(target)
	if !bytes.Equal(before, after) {
		t.Fatal("managed model was not rolled back")
	}
	if c.config.SelectedModel() != settings.Model || c.engine != beforeEngine || !c.managed || c.setupError != "" {
		t.Fatal("rollback lost old working generation")
	}
	disk, _ := os.ReadFile(c.path)
	if !bytes.Equal(disk, edited) {
		t.Fatal("outside service settings were overwritten")
	}
	if calls.Load() != 0 {
		t.Fatal("preferences made an unexpected network request")
	}
}

func TestReadOnlyCannotRebuildCodexOrCreateTransaction(t *testing.T) {
	c, h := panelControl(t, nil)
	home := t.TempDir()
	c.config.CodexHome = home
	original := []byte("model=[")
	target := filepath.Join(home, "config.toml")
	os.WriteFile(target, original, 0600)
	for _, endpoint := range []string{"codex-config/preview", "codex-config/rebuild", "recovery/preview", "recovery/apply", "recover"} {
		w := panelPost(h, endpoint, `{}`)
		if w.Code != 400 {
			t.Fatalf("%s: %d", endpoint, w.Code)
		}
	}
	got, _ := os.ReadFile(target)
	if !bytes.Equal(got, original) {
		t.Fatal("read-only operation changed Codex")
	}
	if _, err := os.Stat(filepath.Join(c.dir, "config-transaction.json")); !os.IsNotExist(err) {
		t.Fatal("read-only operation created transaction")
	}
}

func TestPreferencesFailedSaveCannotRetargetGuardWhileKeepingOldEngine(t *testing.T) {
	c := fixtureControl(t, "model='original'\n", `{"tokens":{"access_token":"synthetic-old-oauth"}}`, "")
	c.config.InjectionDisabled = true
	c.path = filepath.Join(c.dir, "config.json")
	initial, _ := json.Marshal(c.config)
	os.WriteFile(c.path, initial, 0600)
	c.setup()
	if !c.managed {
		t.Fatal(c.setupError)
	}
	calls := startCapturedEngine(t, c)
	// Auth changed outside this service before the operation. Under the normal
	// request guard this is blocked; a failed model preference save must not make
	// the new API key look compatible with the surviving old ChatGPT engine.
	os.WriteFile(filepath.Join(c.config.CodexHome, "auth.json"), []byte(`{"OPENAI_API_KEY":"synthetic-new-api-key"}`), 0600)
	outside := c.config
	outside.Direct = false
	edited, _ := json.Marshal(outside)
	os.WriteFile(c.path, edited, 0600)
	h := controlHandler(c.config.Listen, panelTestToken, c)
	w := panelPost(h, "preferences", `{"model":"gpt-5.6-sol","account_mode":"auto","state_fallback":"strict"}`)
	if w.Code != 400 && w.Code != 409 {
		t.Fatalf("expected local configuration conflict: %d %s", w.Code, w.Body.String())
	}
	forwarded := edgeForward(c)
	if calls.Load() != 0 || (forwarded.Code != 409 && forwarded.Code != 503) {
		t.Fatalf("failed preference operation bypassed changed-auth guard: HTTP=%d old_upstream_calls=%d selected=%s", forwarded.Code, calls.Load(), c.targetKind)
	}
}

func TestPreferencesNeverReplaceServiceConfigSymlink(t *testing.T) {
	c, h := panelControl(t, nil)
	original, err := os.ReadFile(c.path)
	if err != nil {
		t.Fatal(err)
	}
	real := filepath.Join(t.TempDir(), "real-service.json")
	if err = os.WriteFile(real, original, 0600); err != nil {
		t.Fatal(err)
	}
	if err = os.Remove(c.path); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(real, c.path); err != nil {
		t.Skip("symlink privilege unavailable")
	}
	w := panelPost(h, "preferences", `{"model":"gpt-5.6-sol","account_mode":"auto","state_fallback":"strict"}`)
	if w.Code != 400 {
		t.Fatalf("symlink configuration unexpectedly replaced: HTTP %d", w.Code)
	}
	info, err := os.Lstat(c.path)
	if err != nil || info.Mode()&os.ModeSymlink == 0 {
		t.Fatal("service configuration symlink was replaced")
	}
	after, _ := os.ReadFile(real)
	if !bytes.Equal(after, original) {
		t.Fatal("linked source configuration changed")
	}
}
