package service

import (
	"encoding/json"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/gylive/ccodex-sleep-state/internal/codexconfig"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

func TestPreferencesPersistAndKeepReadOnlyCodex(t *testing.T) {
	c, h := panelControl(t, nil)
	w := panelPost(h, "preferences", `{"model":"gpt-5.6-sol","account_mode":"team"}`)
	if w.Code != 200 {
		t.Fatal(w.Code, w.Body.String())
	}
	cfg, err := settings.Load(c.path)
	if err != nil || cfg.Model != "gpt-5.6-sol" || cfg.AccountMode != "team" {
		t.Fatal(cfg, err)
	}
	if c.status()["configuration_writable"] != false {
		t.Fatal("read-only mode changed")
	}
	before := diskConfig(t, c)
	w = panelPost(h, "preferences", `{"model":"not-supported","account_mode":"team"}`)
	if w.Code != 400 || diskConfig(t, c) != before {
		t.Fatal("invalid settings accepted")
	}
}
func TestReadOnlyRecoveryDoesNotPretendSuccess(t *testing.T) {
	_, h := panelControl(t, nil)
	for _, path := range []string{"recover", "recovery/preview", "recovery/apply"} {
		w := panelPost(h, path, `{}`)
		if w.Code != 400 {
			t.Fatal(path, w.Code)
		}
	}
}
func TestTrafficCountsRejectedRequestsWithoutSession(t *testing.T) {
	c, _ := panelControl(t, nil)
	w := httptest.NewRecorder()
	c.ServeHTTP(w, httptest.NewRequest("POST", "http://127.0.0.1:17841/backend-api/codex/responses", strings.NewReader(`{"model":"unsupported"}`)))
	traffic := c.status()["traffic"].(map[string]any)
	if traffic["total"] != uint64(1) || traffic["failed"] != uint64(1) {
		t.Fatal(traffic)
	}
	data, _ := json.Marshal(traffic)
	if strings.Contains(string(data), "unsupported") {
		t.Fatal("request body recorded")
	}
}

func TestNotReadyResponseExplainsOneClickRecovery(t *testing.T) {
	c := &control{config: settings.Config{Listen: "127.0.0.1:17841"}, setupError: "Codex 配置未接管。"}
	w := httptest.NewRecorder()
	c.ServeHTTP(w, httptest.NewRequest("POST", "http://127.0.0.1:17841/backend-api/codex/responses", strings.NewReader(`{"model":"gpt-6-astra"}`)))
	if w.Code != 503 {
		t.Fatalf("status=%d body=%s", w.Code, w.Body.String())
	}
	var body map[string]string
	if err := json.Unmarshal(w.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if body["error"] != "service_not_ready" || body["admin_url"] != "http://127.0.0.1:17841/admin/" || !strings.Contains(body["message"], "检查与修复配置") || !strings.Contains(body["message"], body["admin_url"]) || !strings.Contains(body["next_action"], "不要在 CCS") {
		t.Fatalf("not actionable: %#v", body)
	}
}

func TestInjectionStatusNeverKeepsStaleExplanation(t *testing.T) {
	c, h := panelControl(t, nil)
	panelPost(h, "injection", `{"enabled":false}`)
	if !strings.Contains(c.status()["injection_reason"].(string), "已关闭") {
		t.Fatal("off text")
	}
	panelPost(h, "injection", `{"enabled":true}`)
	st := c.status()
	if strings.Contains(st["injection_reason"].(string), "已关闭") || st["injection_effective"] != false {
		t.Fatal(st)
	}
}
func TestRescueBackupRequiresMatchingPreview(t *testing.T) {
	c, _ := panelControl(t, nil)
	c.rescue = true
	damaged := []byte(`{"proxy_urls": this-is-a-broken-private-subscription}`)
	if err := os.WriteFile(c.path, damaged, 0600); err != nil {
		t.Fatal(err)
	}
	p, err := c.rescuePreview()
	if err != nil {
		t.Fatal(err)
	}
	if err = c.resetServiceConfig("outdated"); err == nil {
		t.Fatal("accepted stale preview")
	}
	current, _ := os.ReadFile(c.path)
	if string(current) != string(damaged) {
		t.Fatal("stale preview overwrote file")
	}
	if err = c.resetServiceConfig(p["config_sha256"].(string)); err != nil {
		t.Fatal(err)
	}
	cfg, err := settings.Load(c.path)
	if err != nil || !cfg.InjectionDisabled {
		t.Fatal(err)
	}
	backups, _ := filepath.Glob(filepath.Join(c.dir, "backups", "invalid-service-*.json"))
	if len(backups) != 1 {
		t.Fatal("missing backup")
	}
	got, _ := os.ReadFile(backups[0])
	if string(got) != string(damaged) {
		t.Fatal("backup not original")
	}
	w := httptest.NewRecorder()
	c.ServeHTTP(w, httptest.NewRequest("POST", "http://127.0.0.1:17841/backend-api/codex/responses", strings.NewReader(`{}`)))
	if w.Code != 503 {
		t.Fatal("rescue forwarded traffic")
	}
}
func TestRecoveryPanelPreservesCCSAndReattaches(t *testing.T) {
	c := fixtureControl(t, "", `{"tokens":{"access_token":"fixture-oauth"}}`, "")
	c.path = filepath.Join(c.dir, "config.json")
	b, _ := json.Marshal(c.config)
	os.WriteFile(c.path, b, 0600)
	c.setup()
	if !c.managed {
		t.Fatal(c.setupError)
	}
	target := filepath.Join(c.config.CodexHome, "config.toml")
	switched := []byte("# CCS switched to independently authenticated relay\nmodel_provider='relay'\n[model_providers.relay]\nname='Custom relay'\nbase_url='https://relay.invalid/v1'\nenv_key='FIXTURE_KEY'\n")
	os.WriteFile(target, switched, 0600)
	h := controlHandler(c.config.Listen, panelTestToken, c)
	preview := panelPost(h, "recovery/preview", `{}`)
	if preview.Code != 200 {
		t.Fatal(preview.Body.String())
	}
	var p map[string]any
	json.Unmarshal(preview.Body.Bytes(), &p)
	if p["can_keep_current"] != true {
		t.Fatal(p)
	}
	body, _ := json.Marshal(map[string]any{"mode": "keep_current", "expected_config_sha256": p["config_sha256"], "expected_transaction_sha256": p["transaction_sha256"]})
	w := panelPost(h, "recovery/apply", string(body))
	defer c.stop()
	if w.Code != 200 || !c.managed || c.targetKind != "relay" {
		t.Fatal(w.Code, w.Body.String())
	}
	if err := codexconfig.Restore(c.dir); err != nil {
		t.Fatal(err)
	}
	restored, _ := os.ReadFile(target)
	if !strings.Contains(string(restored), "relay.invalid") || !strings.Contains(string(restored), "FIXTURE_KEY") {
		t.Fatal("CCS config lost")
	}
}
