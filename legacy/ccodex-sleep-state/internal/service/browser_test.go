package service

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestLaunchTicketSingleUseExpiryAndCrossOrigin(t *testing.T) {
	c, _ := panelControl(t, nil)
	ticket, err := newBrowserTicket()
	if err != nil {
		t.Fatal(err)
	}
	h := controlHandler(c.config.Listen, panelTestToken, c, ticket)
	body, _ := json.Marshal(map[string]string{"ticket": ticket.value})
	w := panelRequest(h, "POST", "/admin/api/launch", string(body), map[string]string{"Origin": "https://attacker.invalid"})
	if w.Code != 403 {
		t.Fatal("cross origin exchange")
	}
	w = panelRequest(h, "POST", "/admin/api/launch", string(body), map[string]string{"Origin": "http://127.0.0.1:17841"})
	if w.Code != 200 || !strings.Contains(w.Body.String(), panelTestToken) {
		t.Fatal("launch login failed", w.Code)
	}
	w = panelRequest(h, "POST", "/admin/api/launch", string(body), nil)
	if w.Code != 401 {
		t.Fatal("launch ticket reused")
	}
	ticket.used = false
	ticket.expires = time.Now().Add(-time.Second)
	w = panelRequest(h, "POST", "/admin/api/launch", string(body), nil)
	if w.Code != 401 {
		t.Fatal("expired launch allowed")
	}
}
func TestBrowserCommandsDoNotUseShell(t *testing.T) {
	url := "http://127.0.0.1:17841/admin/#launch=synthetic"
	for _, goos := range []string{"windows", "darwin", "linux"} {
		cmd := browserCommand(goos, url)
		if cmd.Args[len(cmd.Args)-1] != url {
			t.Fatal(cmd.Args)
		}
		for _, a := range cmd.Args {
			if a == "-c" || a == "/c" {
				t.Fatal("shell evaluation")
			}
		}
	}
}
func TestLaunchTicketUnknownAndMalformed(t *testing.T) {
	ticket, _ := newBrowserTicket()
	for _, body := range []string{`{"ticket":"wrong"}`, `{"ticket":"wrong","extra":true}`, `{}`, `{"ticket":"wrong"} {}`} {
		w := httptest.NewRecorder()
		ticket.exchange(w, httptest.NewRequest("POST", "/admin/api/launch", strings.NewReader(body)), panelTestToken)
		if w.Code < 400 || strings.Contains(w.Body.String(), panelTestToken) {
			t.Fatal(w.Code)
		}
	}
}

func TestExistingPanelRenewsOnlyWithAuthentication(t *testing.T) {
	c, _ := panelControl(t, nil)
	ticket, _ := newBrowserTicket()
	old := ticket.value
	server := httptest.NewUnstartedServer(nil)
	address := server.Listener.Addr().String()
	server.Config.Handler = controlHandler(address, panelTestToken, c, ticket)
	server.Start()
	defer server.Close()
	data, _ := json.Marshal(Runtime{Address: address, Token: panelTestToken})
	if err := os.WriteFile(filepath.Join(c.dir, "runtime.json"), data, 0600); err != nil {
		t.Fatal(err)
	}
	url, err := existingPanelURL(context.Background(), c.dir)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(url, server.URL+"/admin/#launch=") || strings.Contains(url, panelTestToken) {
		t.Fatal("unsafe launch URL")
	}
	if strings.HasSuffix(url, old) {
		t.Fatal("ticket not rotated")
	}
	// An old bookmark no longer grants access after a new launch.
	w := httptest.NewRecorder()
	ticket.exchange(w, httptest.NewRequest("POST", "/admin/api/launch", strings.NewReader(`{"ticket":"`+old+`"}`)), panelTestToken)
	if w.Code != 401 {
		t.Fatal("old ticket still accepted")
	}
	req, _ := http.NewRequest("POST", server.URL+"/admin/api/new-launch", nil)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 401 {
		t.Fatal("anonymous renewal accepted")
	}
}
func TestExistingPanelRejectsRemoteRuntime(t *testing.T) {
	dir := t.TempDir()
	data, _ := json.Marshal(Runtime{Address: "remote.invalid:443", Token: panelTestToken})
	os.WriteFile(filepath.Join(dir, "runtime.json"), data, 0600)
	if _, err := existingPanelURL(context.Background(), dir); err == nil {
		t.Fatal("remote host accepted")
	}
}
