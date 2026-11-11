package service

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/gylive/ccodex-sleep-state/internal/proxyroute"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

const panelTestToken = "synthetic-management-token-for-tests"

func panelControl(t *testing.T, change func(*settings.Config)) (*control, http.Handler) {
	t.Helper()
	dir := t.TempDir()
	cfg := settings.Default()
	if change != nil {
		change(&cfg)
	}
	data, err := json.Marshal(cfg)
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "config.json")
	if err = os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	c := &control{ctx: context.Background(), config: cfg, path: path, dir: dir, log: slog.New(slog.NewTextHandler(io.Discard, nil))}
	routes, err := proxyroute.Load(context.Background(), cfg)
	if err != nil {
		t.Fatal(err)
	}
	c.start(routes)
	t.Cleanup(func() { c.mu.Lock(); defer c.mu.Unlock(); c.stop() })
	return c, controlHandler(cfg.Listen, panelTestToken, c)
}
func panelRequest(h http.Handler, method, path, body string, headers map[string]string) *httptest.ResponseRecorder {
	r := httptest.NewRequest(method, "http://127.0.0.1:17841"+path, strings.NewReader(body))
	for k, v := range headers {
		r.Header.Set(k, v)
	}
	w := httptest.NewRecorder()
	h.ServeHTTP(w, r)
	return w
}
func panelPost(h http.Handler, path, body string) *httptest.ResponseRecorder {
	return panelRequest(h, "POST", "/admin/api/"+path, body, map[string]string{"Authorization": "Bearer " + panelTestToken, "Origin": "http://127.0.0.1:17841", "Sec-Fetch-Site": "same-origin", "Content-Type": "application/json"})
}
func diskConfig(t *testing.T, c *control) string {
	t.Helper()
	b, e := os.ReadFile(c.path)
	if e != nil {
		t.Fatal(e)
	}
	return string(b)
}

func TestPanelHostOriginAndToken(t *testing.T) {
	_, h := panelControl(t, nil)
	for _, tc := range []struct {
		name, host, origin, site, token string
		want                            int
	}{
		{"valid", "127.0.0.1:17841", "http://127.0.0.1:17841", "same-origin", panelTestToken, 200},
		{"rebind", "attacker.example:17841", "", "", panelTestToken, 403},
		{"cross-origin", "127.0.0.1:17841", "https://attacker.example", "", panelTestToken, 403},
		{"null-origin", "127.0.0.1:17841", "null", "", panelTestToken, 403},
		{"cross-site", "127.0.0.1:17841", "", "cross-site", panelTestToken, 403},
		{"missing-token", "127.0.0.1:17841", "", "", "", 401},
		{"wrong-token", "127.0.0.1:17841", "", "", "wrong", 401},
	} {
		t.Run(tc.name, func(t *testing.T) {
			r := httptest.NewRequest("GET", "http://127.0.0.1:17841/admin/api/status", nil)
			r.Host = tc.host
			r.Header.Set("Origin", tc.origin)
			r.Header.Set("Sec-Fetch-Site", tc.site)
			r.Header.Set("Authorization", "Bearer "+tc.token)
			w := httptest.NewRecorder()
			h.ServeHTTP(w, r)
			if w.Code != tc.want {
				t.Fatalf("%d %s", w.Code, w.Body.String())
			}
			if strings.Contains(w.Body.String(), panelTestToken) {
				t.Fatal("management token leaked")
			}
		})
	}
	w := panelRequest(h, "POST", "/backend-api/codex/responses", `{}`, map[string]string{"Origin": "http://127.0.0.1:17841", "Authorization": "Bearer test-api-key"})
	if w.Code != 403 {
		t.Fatal("browser reached gateway", w.Code)
	}
}

func TestPanelStaticAssetsNeedNoTokenButStayIsolated(t *testing.T) {
	_, h := panelControl(t, nil)
	for path, mime := range map[string]string{"/admin/": "text/html", "/admin/app.js": "text/javascript", "/admin/style.css": "text/css"} {
		w := panelRequest(h, "GET", path, "", nil)
		if w.Code != 200 || !strings.Contains(w.Header().Get("Content-Type"), mime) || w.Body.Len() == 0 {
			t.Fatalf("%s: %d", path, w.Code)
		}
		if w.Header().Get("Content-Security-Policy") == "" || w.Header().Get("X-Frame-Options") != "DENY" || w.Header().Get("Cache-Control") != "no-store" {
			t.Fatal("missing browser security headers")
		}
		if strings.Contains(w.Body.String(), panelTestToken) {
			t.Fatal("static secret")
		}
	}
	for _, path := range []string{"/admin/runtime.json", "/admin/../config.json"} {
		if w := panelRequest(h, "GET", path, "", nil); w.Code != 404 {
			t.Fatal(path, w.Code)
		}
	}
}

func TestPanelInjectionPersistsAndBacksUp(t *testing.T) {
	c, h := panelControl(t, nil)
	before := diskConfig(t, c)
	w := panelPost(h, "injection", `{"enabled":false}`)
	if w.Code != 200 {
		t.Fatalf("%d %s", w.Code, w.Body.String())
	}
	cfg, e := settings.Load(c.path)
	if e != nil || !cfg.InjectionDisabled || c.engine.Status()["injection_enabled"] != false {
		t.Fatal("toggle not persisted", e)
	}
	files, e := filepath.Glob(filepath.Join(c.dir, "backups", "*.json"))
	if e != nil || len(files) != 1 {
		t.Fatal("no independent backup")
	}
	b, e := os.ReadFile(files[0])
	if e != nil || string(b) != before {
		t.Fatal("backup mismatch")
	}
}

func TestPanelSourceFailuresPreserveConfigurationAndEngine(t *testing.T) {
	c, h := panelControl(t, nil)
	before, engine := diskConfig(t, c), c.engine
	for _, endpoint := range []string{"sources/test", "sources/apply"} {
		for _, body := range []string{`{"mode":"proxy","value":"not-a-proxy"}`, `{"mode":"file","value":"/missing/synthetic-subscription.yaml"}`, `{"mode":"subscription","value":"http://remote.invalid/private-secret"}`} {
			w := panelPost(h, endpoint, body)
			if w.Code != 400 {
				t.Fatalf("%s %d %s", endpoint, w.Code, w.Body.String())
			}
			if strings.Contains(w.Body.String(), "private-secret") {
				t.Fatal("source secret echoed")
			}
			if diskConfig(t, c) != before || c.engine != engine {
				t.Fatal("failed operation changed state")
			}
		}
	}
	// Persist must not overwrite an external editor's changes after a valid test.
	edited := c.config
	edited.InjectionDisabled = true
	b, _ := json.Marshal(edited)
	if e := os.WriteFile(c.path, b, 0600); e != nil {
		t.Fatal(e)
	}
	w := panelPost(h, "sources/apply", `{"mode":"direct"}`)
	if w.Code != 400 || c.engine != engine || diskConfig(t, c) != string(b) {
		t.Fatal("external edit overwritten")
	}
	if c.cancel == nil {
		t.Fatal("old engine was not resumed on failure")
	}
}

func TestPanelLocalSubscriptionTestAndApply(t *testing.T) {
	c, h := panelControl(t, nil)
	before, engine := diskConfig(t, c), c.engine
	path := filepath.Join(c.dir, "local-subscription.yaml")
	contents := "proxies:\n  - name: local-fixture\n    type: socks5\n    server: 127.0.0.1\n    port: 7897\n"
	if e := os.WriteFile(path, []byte(contents), 0600); e != nil {
		t.Fatal(e)
	}
	body, _ := json.Marshal(sourceRequest{Mode: "file", Value: path})
	w := panelPost(h, "sources/test", string(body))
	if w.Code != 200 {
		t.Fatalf("%d %s", w.Code, w.Body.String())
	}
	if diskConfig(t, c) != before || c.engine != engine {
		t.Fatal("test applied configuration")
	}
	w = panelPost(h, "sources/apply", string(body))
	if w.Code != 200 {
		t.Fatalf("%d %s", w.Code, w.Body.String())
	}
	cfg, e := settings.Load(c.path)
	if e != nil || len(cfg.Subscriptions) != 1 || cfg.Subscriptions[0].File != path || cfg.Direct {
		t.Fatal("local source not persisted", e)
	}
	b, e := os.ReadFile(path)
	if e != nil || string(b) != contents {
		t.Fatal("source file changed")
	}
}

func TestPanelRelayCannotEnableInjection(t *testing.T) {
	c, h := panelControl(t, func(cfg *settings.Config) { cfg.UpstreamKind = "relay"; cfg.Upstream = "https://example.invalid/v1" })
	before := diskConfig(t, c)
	w := panelPost(h, "injection", `{"enabled":true}`)
	if w.Code != 400 || diskConfig(t, c) != before {
		t.Fatal("relay injection accepted")
	}
	if c.status()["injection_enabled"] != false || c.status()["upstream_kind"] != "relay" {
		t.Fatal("relay status misleading")
	}
}

func TestPanelPinAndApplyCannotClearQuotaRejection(t *testing.T) {
	var calls atomic.Int32
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls.Add(1)
		w.Header().Set("Retry-After", "180")
		w.WriteHeader(429)
	}))
	defer upstream.Close()
	c, h := panelControl(t, func(cfg *settings.Config) {
		cfg.Upstream = upstream.URL + "/backend-api/codex"
		cfg.InjectionDisabled = true
	})
	engine, before := c.engine, diskConfig(t, c)
	reject := func() {
		r := httptest.NewRequest("POST", "http://127.0.0.1:17841/backend-api/codex/responses", strings.NewReader(`{"model":"gpt-6-astra","input":"test"}`))
		r.Header.Set("Authorization", "Bearer synthetic-api-key")
		w := httptest.NewRecorder()
		engine.ServeHTTP(w, r)
		if w.Code != 429 {
			t.Errorf("expected rejection, got %d", w.Code)
		}
	}
	// Model the final background result arriving during pause. The old engine
	// must remain alive until done closes, then be checked before replacement.
	c.pause()
	ctx, cancel := context.WithCancel(context.Background())
	c.cancel = cancel
	c.done = make(chan struct{})
	done := c.done
	go func() { <-ctx.Done(); reject(); close(done) }()
	w := panelPost(h, "routes/pin", `{"id":""}`)
	if w.Code != 409 {
		t.Fatalf("late rejection discarded: %d %s", w.Code, w.Body.String())
	}
	w = panelPost(h, "sources/apply", `{"mode":"direct"}`)
	if w.Code != 409 {
		t.Fatalf("quota cleared by apply: %d", w.Code)
	}
	w = panelPost(h, "timing", `{"probe_timeout_seconds":15,"probe_cooldown_seconds":600,"state_ttl_seconds":1800,"refresh_before_seconds":300,"max_probes_per_round":2}`)
	if w.Code != 400 {
		t.Fatalf("quota cleared by timing: %d", w.Code)
	}
	if c.engine != engine || !engine.Restricted() || diskConfig(t, c) != before || c.cancel == nil || calls.Load() != 1 {
		t.Fatal("restricted generation not retained/resumed")
	}
}
