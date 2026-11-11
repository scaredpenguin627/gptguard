package service

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/codexconfig"
	"github.com/gylive/ccodex-sleep-state/internal/proxyroute"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

func quickFixture(t *testing.T, source string) *control {
	t.Helper()
	c := fixtureControl(t, source, `{"tokens":{"access_token":"synthetic-quick-oauth"}}`, "")
	c.config.Direct = false
	// Explicit sources are never auto-detected or dialled during this test.
	c.config.ProxyURLs = []string{"socks5://127.0.0.1:9"}
	c.config.Model = "gpt-5.6-sol"
	c.path = filepath.Join(c.dir, "config.json")
	data, _ := json.Marshal(c.config)
	if err := os.WriteFile(c.path, data, 0600); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.stop() })
	return c
}

func TestQuickSetupPreservesProviderModelCredentialsAndExplicitSource(t *testing.T) {
	for _, source := range []string{"model='original'\n", "model_provider='relay'\n[model_providers.relay]\nname='Existing relay'\nbase_url='https://relay.invalid/v1'\nenv_key='FIXTURE_KEY'\n"} {
		c := quickFixture(t, source)
		authPath := filepath.Join(c.config.CodexHome, "auth.json")
		beforeAuth, _ := os.ReadFile(authPath)
		c.setup()
		if !c.managed {
			t.Fatal(c.setupError)
		}
		beforeURL := c.targetURL
		calls := startCapturedEngine(t, c)
		if err := c.quickSetup(context.Background()); err != nil {
			t.Fatal(err)
		}
		if !c.managed || c.engine == nil || c.setupError != "" || c.routeError != "" {
			t.Fatal("one-click reported success without readiness")
		}
		if c.targetURL != beforeURL || c.config.SelectedModel() != "gpt-5.6-sol" || c.config.Direct || len(c.config.ProxyURLs) != 1 || c.config.ProxyURLs[0] != "socks5://127.0.0.1:9" {
			t.Fatal("one-click replaced existing choices")
		}
		saved, err := settings.Load(c.path)
		if err != nil || saved.StateFallback != "passthrough" {
			t.Fatal("fallback choice not persisted", err)
		}
		afterAuth, _ := os.ReadFile(authPath)
		if !bytes.Equal(beforeAuth, afterAuth) {
			t.Fatal("credentials changed")
		}
		if calls.Load() != 0 {
			t.Fatal("quick setup called model upstream")
		}
	}
}

func TestQuickSetupKeepsCCSSwitchAndArchivesConflict(t *testing.T) {
	c := quickFixture(t, "model='old'\n")
	c.setup()
	if !c.managed {
		t.Fatal(c.setupError)
	}
	calls := startCapturedEngine(t, c)
	target := filepath.Join(c.config.CodexHome, "config.toml")
	switched := []byte("# user's new CCS selection\nmodel_provider='relay'\n[model_providers.relay]\nbase_url='https://new.invalid/v1'\nenv_key='NEW_KEY'\n")
	os.WriteFile(target, switched, 0600)
	if err := c.quickSetup(context.Background()); err != nil {
		t.Fatal(err)
	}
	if c.targetURL != "https://new.invalid/v1" || c.targetKind != "relay" {
		t.Fatal("CCS choice not retained")
	}
	archives, _ := filepath.Glob(filepath.Join(c.dir, "backups", "recovery-*", "current.toml"))
	if len(archives) != 1 {
		t.Fatal("CCS conflict was not independently archived")
	}
	saved, _ := os.ReadFile(archives[0])
	if !bytes.Equal(saved, switched) {
		t.Fatal("CCS archive differs from current original")
	}
	if calls.Load() != 0 {
		t.Fatal("stale engine made a request")
	}
}

func TestQuickSetupCannotForceUnmergeableManagedEdits(t *testing.T) {
	c := quickFixture(t, "model='old'\n")
	c.setup()
	if !c.managed {
		t.Fatal(c.setupError)
	}
	calls := startCapturedEngine(t, c)
	target := filepath.Join(c.config.CodexHome, "config.toml")
	current, _ := os.ReadFile(target)
	changed := bytes.Replace(current, []byte("name = 'OpenAI'"), []byte("name = 'User changed this'"), 1)
	os.WriteFile(target, changed, 0600)
	beforeService, _ := os.ReadFile(c.path)
	if err := c.quickSetup(context.Background()); err == nil {
		t.Fatal("conflicting provider was force-restored")
	}
	got, _ := os.ReadFile(target)
	if !bytes.Equal(got, changed) {
		t.Fatal("dirty current config overwritten")
	}
	afterService, _ := os.ReadFile(c.path)
	if !bytes.Equal(beforeService, afterService) {
		t.Fatal("failed quick setup changed preferences")
	}
	if c.engine != nil || calls.Load() != 0 {
		t.Fatal("stale engine survived unsafe merge")
	}
	if _, err := os.Stat(filepath.Join(c.dir, "config-transaction.json")); err != nil {
		t.Fatal("receipt lost")
	}
}

func TestQuickSetupDirtyTOMLRemainsUntouched(t *testing.T) {
	original := []byte("# do not guess\nmodel = [\n")
	c := quickFixture(t, string(original))
	if err := c.quickSetup(context.Background()); err == nil {
		t.Fatal("bad TOML reported success")
	}
	got, _ := os.ReadFile(filepath.Join(c.config.CodexHome, "config.toml"))
	if !bytes.Equal(got, original) {
		t.Fatal("bad TOML rewritten")
	}
	if c.engine != nil {
		t.Fatal("dirty config has active engine")
	}
}

func TestQuickSetupReadOnlyAndRescueRefuseBeforeChanges(t *testing.T) {
	for _, mode := range []string{"read-only", "rescue"} {
		c := quickFixture(t, "# unchanged\n")
		if mode == "read-only" {
			c.configure = false
		} else {
			c.rescue = true
		}
		before, _ := os.ReadFile(c.path)
		if err := c.quickSetup(context.Background()); err == nil {
			t.Fatal(mode, "was accepted")
		}
		after, _ := os.ReadFile(c.path)
		if !bytes.Equal(before, after) {
			t.Fatal(mode, "changed service file")
		}
		if _, err := os.Stat(filepath.Join(c.dir, "config-transaction.json")); !os.IsNotExist(err) {
			t.Fatal(mode, "created a receipt")
		}
	}
}

func TestQuickSetupPersistConflictLeavesForwardingStopped(t *testing.T) {
	c := quickFixture(t, "# old\n")
	c.setup()
	if !c.managed {
		t.Fatal(c.setupError)
	}
	calls := startCapturedEngine(t, c)
	external := c.config
	external.AccountMode = "team"
	data, _ := json.Marshal(external)
	os.WriteFile(c.path, data, 0600)
	if err := c.quickSetup(context.Background()); err == nil {
		t.Fatal("external settings overwritten")
	}
	got, _ := os.ReadFile(c.path)
	if !bytes.Equal(got, data) {
		t.Fatal("external changes lost")
	}
	if c.engine != nil || edgeForward(c).Code != 503 || calls.Load() != 0 {
		t.Fatal("save failure did not stop forwarding")
	}
}

func TestQuickSetupBadPinnedRouteIsNotSuccess(t *testing.T) {
	c := quickFixture(t, "# original\n")
	c.config.PinnedRoute = "no-longer-present"
	if err := c.quickSetup(context.Background()); err == nil {
		t.Fatal("missing pinned route reported success")
	}
	if _, err := os.Stat(filepath.Join(c.dir, "config-transaction.json")); !os.IsNotExist(err) {
		t.Fatal("bad route changed Codex")
	}
}

func TestQuickSetupCannotClearQuotaRejection(t *testing.T) {
	c := quickFixture(t, "# original\n")
	c.config.InjectionDisabled = true
	c.setup()
	if !c.managed {
		t.Fatal(c.setupError)
	}
	var calls atomic.Int32
	transport := &http.Transport{}
	transport.RegisterProtocol("https", recoveryEdgeTransport(func(r *http.Request) (*http.Response, error) {
		calls.Add(1)
		return &http.Response{StatusCode: 429, Header: http.Header{"Retry-After": []string{"180"}}, Body: io.NopCloser(strings.NewReader(`{}`)), Request: r}, nil
	}))
	c.start([]proxyroute.Route{{ID: "fixture", Transport: transport}})
	req := httptest.NewRequest("POST", "http://127.0.0.1:17841/backend-api/codex/responses", strings.NewReader(`{"model":"gpt-5.6-sol","input":"fixture"}`))
	req.Header.Set("Authorization", "Bearer synthetic-fixture-key")
	w := httptest.NewRecorder()
	c.engine.ServeHTTP(w, req)
	if w.Code != 429 {
		t.Fatal("fixture did not reject")
	}
	old := c.engine
	before, _ := os.ReadFile(c.path)
	if err := c.quickSetup(context.Background()); err == nil {
		t.Fatal("quota restriction bypassed")
	}
	after, _ := os.ReadFile(c.path)
	if old != c.engine || !old.Restricted() || c.cancel == nil || !bytes.Equal(before, after) || calls.Load() != 1 {
		t.Fatal("one-click discarded restriction state")
	}
}

func TestDiscoverLocalSOCKSRequiresNoAuthGreeting(t *testing.T) {
	var calls []string
	dial := func(ctx context.Context, network, address string) (net.Conn, error) {
		calls = append(calls, address)
		if address == "127.0.0.1:1" {
			return nil, errors.New("not listening")
		}
		client, server := net.Pipe()
		go func() {
			defer server.Close()
			_ = server.SetDeadline(time.Now().Add(time.Second))
			var request [3]byte
			if _, err := io.ReadFull(server, request[:]); err != nil {
				return
			}
			if request != [3]byte{5, 1, 0} {
				return
			}
			if address == "127.0.0.1:2" {
				_, _ = server.Write([]byte{5, 2})
			} else {
				_, _ = server.Write([]byte{5, 0})
			}
		}()
		return client, nil
	}
	result, err := discoverLocalSOCKS(context.Background(), []string{"remote.invalid:1", "127.0.0.1:1", "127.0.0.1:2", "127.0.0.1:3"}, dial)
	if err != nil || result != "socks5://127.0.0.1:3" {
		t.Fatal(result, err)
	}
	if strings.Join(calls, ",") != "127.0.0.1:1,127.0.0.1:2,127.0.0.1:3" {
		t.Fatal("discovery left literal loopback")
	}
}

func TestDiscoverLocalSOCKSCancelAndTimeout(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	called := false
	_, err := discoverLocalSOCKS(ctx, []string{"127.0.0.1:1"}, func(context.Context, string, string) (net.Conn, error) {
		called = true
		return nil, errors.New("unexpected")
	})
	if !errors.Is(err, context.Canceled) || called {
		t.Fatal("canceled probe dialled")
	}
	started := time.Now()
	result, err := discoverLocalSOCKS(context.Background(), []string{"127.0.0.1:1"}, func(ctx context.Context, _, _ string) (net.Conn, error) {
		client, server := net.Pipe()
		go func() { <-ctx.Done(); server.Close() }()
		return client, nil
	})
	if err != nil || result != "" || time.Since(started) > time.Second {
		t.Fatal("unresponsive service was accepted or probe hung")
	}
}

func TestQuickSetupCanStillRestoreOriginalAfterSuccess(t *testing.T) {
	original := "# user's original\nmodel='previous'\n"
	c := quickFixture(t, original)
	if err := c.quickSetup(context.Background()); err != nil {
		t.Fatal(err)
	}
	c.stop()
	if err := codexconfig.Restore(c.dir); err != nil {
		t.Fatal(err)
	}
	restored, _ := os.ReadFile(filepath.Join(c.config.CodexHome, "config.toml"))
	if string(restored) != original {
		t.Fatal("one-click lost normal rollback")
	}
}
