package service

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

func TestQuickSetupModePreservesExplicitStrict(t *testing.T) {
	c := quickFixture(t, "# original\n")
	c.config.StateFallback = "strict"
	data, _ := json.Marshal(c.config)
	os.WriteFile(c.path, data, 0600)
	if err := c.quickSetupMode(context.Background(), false); err != nil {
		t.Fatal(err)
	}
	saved, err := settings.Load(c.path)
	if err != nil {
		t.Fatal(err)
	}
	if saved.StateFallback != "strict" || c.config.StateFallback != "strict" {
		t.Fatal("automatic startup changed explicit strict policy")
	}
	if !c.managed || c.engine == nil {
		t.Fatal("strict startup did not connect")
	}
}

func TestQuickSetupModeFirstBlankChoosesFallback(t *testing.T) {
	for _, tc := range []struct {
		name, initial string
		choose        bool
		want          string
	}{
		{"first startup", "", false, "passthrough"},
		{"existing passthrough", "passthrough", false, "passthrough"},
		{"explicit button overrides strict", "strict", true, "passthrough"},
		{"explicit button on blank", "", true, "passthrough"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := quickFixture(t, "# original\n")
			c.config.StateFallback = tc.initial
			data, _ := json.Marshal(c.config)
			os.WriteFile(c.path, data, 0600)
			if err := c.quickSetupMode(context.Background(), tc.choose); err != nil {
				t.Fatal(err)
			}
			saved, err := settings.Load(c.path)
			if err != nil {
				t.Fatal(err)
			}
			if saved.StateFallback != tc.want || c.config.StateFallback != tc.want {
				t.Fatalf("got disk=%q memory=%q, want %q", saved.StateFallback, c.config.StateFallback, tc.want)
			}
		})
	}
}

func TestServeDoesNotRunAutomaticSetupOrIssueLaunchTicket(t *testing.T) {
	dir := t.TempDir()
	cfg := settings.Default()
	cfg.Listen = freeAddress(t)
	cfg.CodexHome = filepath.Join(t.TempDir(), "untouched-codex")
	path := filepath.Join(dir, "config.json")
	original, _ := json.Marshal(cfg)
	os.WriteFile(path, original, 0600)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- RunWithConfig(ctx, dir, path, cfg, false, io.Discard) }()
	awaitStatus(t, dir, done)
	defer func() {
		cancel()
		select {
		case err := <-done:
			if err != nil {
				t.Error(err)
			}
		case <-time.After(5 * time.Second):
			t.Error("serve shutdown timed out")
		}
	}()
	after, _ := os.ReadFile(path)
	if !bytes.Equal(original, after) {
		t.Fatal("serve ran automatic setup")
	}
	if _, err := os.Stat(cfg.CodexHome); !os.IsNotExist(err) {
		t.Fatal("serve --no-config created Codex home")
	}
	client := &http.Client{Transport: &http.Transport{Proxy: nil}, Timeout: time.Second}
	defer client.CloseIdleConnections()
	req, _ := http.NewRequest("POST", "http://"+cfg.Listen+"/admin/api/launch", strings.NewReader(`{"ticket":"synthetic"}`))
	response, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if response.StatusCode != 401 {
		t.Fatal("ordinary serve exposes unauthenticated launch exchange", response.StatusCode)
	}
}

func TestLaunchTicketConcurrentExchangeSucceedsExactlyOnce(t *testing.T) {
	ticket, err := newBrowserTicket()
	if err != nil {
		t.Fatal(err)
	}
	body, _ := json.Marshal(map[string]string{"ticket": ticket.value})
	var accepted, rejected, unexpected atomic.Int32
	var workers sync.WaitGroup
	for i := 0; i < 24; i++ {
		workers.Add(1)
		go func() {
			defer workers.Done()
			w := httptest.NewRecorder()
			ticket.exchange(w, httptest.NewRequest("POST", "/admin/api/launch", bytes.NewReader(body)), panelTestToken)
			switch w.Code {
			case 200:
				accepted.Add(1)
			case 401:
				rejected.Add(1)
				if strings.Contains(w.Body.String(), panelTestToken) {
					unexpected.Add(1)
				}
			default:
				unexpected.Add(1)
			}
		}()
	}
	workers.Wait()
	if accepted.Load() != 1 || rejected.Load() != 23 || unexpected.Load() != 0 {
		t.Fatalf("accepted=%d rejected=%d unexpected=%d", accepted.Load(), rejected.Load(), unexpected.Load())
	}
}

func TestLaunchTicketInvalidRequestsDoNotConsumeValidTicket(t *testing.T) {
	c, _ := panelControl(t, nil)
	ticket, err := newBrowserTicket()
	if err != nil {
		t.Fatal(err)
	}
	h := controlHandler(c.config.Listen, panelTestToken, c, ticket)
	valid, _ := json.Marshal(map[string]string{"ticket": ticket.value})
	for _, tc := range []struct {
		method, body, host, origin, site string
		want                             int
	}{
		{"GET", "", "127.0.0.1:17841", "", "", 405},
		{"POST", string(valid), "attacker.invalid", "", "", 403},
		{"POST", string(valid), "127.0.0.1:17841", "null", "", 403},
		{"POST", string(valid), "127.0.0.1:17841", "", "cross-site", 403},
		{"POST", `{"ticket":"wrong"}`, "127.0.0.1:17841", "", "", 401},
		{"POST", `{"ticket":"` + strings.Repeat("x", 300) + `"}`, "127.0.0.1:17841", "", "", 400},
		{"POST", string(valid) + ` {}`, "127.0.0.1:17841", "", "", 400},
	} {
		req := httptest.NewRequest(tc.method, "http://127.0.0.1:17841/admin/api/launch", strings.NewReader(tc.body))
		req.Host = tc.host
		req.Header.Set("Origin", tc.origin)
		req.Header.Set("Sec-Fetch-Site", tc.site)
		w := httptest.NewRecorder()
		h.ServeHTTP(w, req)
		if w.Code != tc.want || strings.Contains(w.Body.String(), panelTestToken) {
			t.Fatalf("invalid request leaked or consumed token: %d %s", w.Code, w.Body.String())
		}
	}
	w := panelRequest(h, "POST", "/admin/api/launch", string(valid), map[string]string{"Origin": "http://127.0.0.1:17841", "Sec-Fetch-Site": "same-origin"})
	if w.Code != 200 || !strings.Contains(w.Body.String(), panelTestToken) {
		t.Fatal("valid ticket was consumed by an invalid request")
	}
	for _, header := range []string{"Cache-Control", "Referrer-Policy", "Content-Security-Policy"} {
		if w.Header().Get(header) == "" {
			t.Fatal("launch response lacks browser security header", header)
		}
	}
	for _, header := range []string{"Location", "Set-Cookie"} {
		if w.Header().Get(header) != "" {
			t.Fatal("launch credential leaked into navigation or cookie")
		}
	}
}

func TestLaunchTicketNeverEmbeddedInStaticPanel(t *testing.T) {
	c, _ := panelControl(t, nil)
	ticket, _ := newBrowserTicket()
	h := controlHandler(c.config.Listen, panelTestToken, c, ticket)
	for _, path := range []string{"/admin/", "/admin/app.js", "/admin/style.css"} {
		w := panelRequest(h, "GET", path, "", nil)
		if w.Code != 200 {
			t.Fatal(path, w.Code)
		}
		if strings.Contains(w.Body.String(), ticket.value) || strings.Contains(w.Body.String(), panelTestToken) {
			t.Fatal("static panel disclosed live management credential")
		}
	}
}
