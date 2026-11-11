package proxyroute

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

func TestLocalSubscriptionReadBoundaries(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "nodes.yaml")
	content := "proxies: []\n"
	if err := os.WriteFile(file, []byte(content), 0600); err != nil {
		t.Fatal(err)
	}
	got, err := readLocal(file)
	if err != nil || string(got) != content {
		t.Fatalf("regular file: %q, %v", got, err)
	}
	for _, path := range []string{dir, filepath.Join(dir, "missing")} {
		if _, err := readLocal(path); err == nil {
			t.Fatal("accepted non-file")
		}
	}
	link := filepath.Join(dir, "link.yaml")
	if err := os.Symlink(file, link); err == nil {
		if _, err := readLocal(link); err == nil {
			t.Fatal("accepted symbolic link")
		}
	}
	if err := os.WriteFile(file, make([]byte, MaxSubscriptionBytes+1), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := readLocal(file); err == nil {
		t.Fatal("accepted oversized file")
	}
}

func TestStableNodeIdentity(t *testing.T) {
	a := map[string]any{"name": "private-label", "type": "http", "server": "private.example", "port": 8080, "password": "private-secret"}
	b := map[string]any{"password": "private-secret", "port": 8080, "server": "private.example", "type": "http", "name": "renamed"}
	aid, err := nodeIdentity(a)
	if err != nil {
		t.Fatal(err)
	}
	bid, err := nodeIdentity(b)
	if err != nil {
		t.Fatal(err)
	}
	if aid != bid {
		t.Fatal("display labels changed identity")
	}
	if strings.Contains(aid, "private") || len(aid) != len("route-")+32 {
		t.Fatal("identity is not an opaque digest")
	}
	b["password"] = "rotated"
	changed, err := nodeIdentity(b)
	if err != nil {
		t.Fatal(err)
	}
	if aid == changed {
		t.Fatal("credential change did not change identity")
	}
	b["invalid"] = make(chan int)
	if _, err := nodeIdentity(b); err == nil {
		t.Fatal("accepted noncanonical settings")
	}
}

func TestLoadStableRoutesAcrossReordering(t *testing.T) {
	urls := []string{"http://127.0.0.1:18001", "socks5://127.0.0.1:18002"}
	load := func(urls []string) []string {
		t.Helper()
		routes, err := Load(context.Background(), settings.Config{Direct: true, ProxyURLs: urls})
		if err != nil {
			t.Fatal(err)
		}
		var ids []string
		for _, r := range routes {
			ids = append(ids, r.ID)
			r.Close()
		}
		return ids
	}
	a := load(urls)
	b := load([]string{urls[1], urls[0], urls[1]})
	if len(a) != 3 || len(b) != 3 || a[0] != "direct" || b[0] != "direct" || a[1] != b[2] || a[2] != b[1] {
		t.Fatalf("route order changed identities or duplicated endpoints: %v vs %v", a, b)
	}
}

func TestLoadLocalSubscription(t *testing.T) {
	file := filepath.Join(t.TempDir(), "nodes.txt")
	if err := os.WriteFile(file, []byte("http://127.0.0.1:18001\n"), 0600); err != nil {
		t.Fatal(err)
	}
	routes, err := Load(context.Background(), settings.Config{Subscriptions: []settings.Source{{File: file}}})
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		for _, r := range routes {
			r.Close()
		}
	}()
	if len(routes) != 1 || routes[0].ID != routes[0].StableID {
		t.Fatal("local subscription was not loaded with stable identity")
	}
	_, err = Load(context.Background(), settings.Config{Subscriptions: []settings.Source{{File: file, URL: "http://127.0.0.1/sub"}}})
	if err == nil {
		t.Fatal("accepted both file and URL")
	}
}

func TestLocalSubscriptionRejectsNamedPipe(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix FIFO fixture")
	}
	mkfifo, err := exec.LookPath("mkfifo")
	if err != nil {
		t.Skip("mkfifo unavailable")
	}
	path := filepath.Join(t.TempDir(), "nodes.pipe")
	if err := exec.Command(mkfifo, path).Run(); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { _, err := readLocal(path); done <- err }()
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("accepted named pipe")
		}
	case <-time.After(time.Second):
		t.Fatal("subscription reader blocked on named pipe")
	}
}

func TestNodeDisplayNameSanitizesUntrustedLabels(t *testing.T) {
	for _, tc := range []struct{ name, want string }{
		{"日本 01", "日本 01"},
		{"\x1b日本\n\t\u202e 01", "日本 01"},
		{"http://user:secret@example.invalid:8080", "http 线路"},
		{"user@example.invalid", "http 线路"},
		{"http:\n//private", "http 线路"},
		{"", "http 线路"},
		{"imported", "http 线路"},
		{"server.example", "http 线路"},
		{"private-secret label", "http 线路"},
		{strings.Repeat("中", 81), strings.Repeat("中", 80)},
	} {
		node := map[string]any{"name": tc.name, "server": "server.example", "password": "private-secret"}
		if got := nodeDisplayName(node, "http"); got != tc.want {
			t.Errorf("name %q: got %q want %q", tc.name, got, tc.want)
		}
	}
	node := map[string]any{"name": "日本 01", "type": "http", "server": "127.0.0.1", "port": 18001}
	route, err := Build(node, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer route.Close()
	if route.DisplayName != "日本 01" || route.Protocol != "http" {
		t.Fatal("Build lost display metadata")
	}
	routes, err := Load(context.Background(), settings.Config{Direct: true})
	if err != nil {
		t.Fatal(err)
	}
	defer routes[0].Close()
	if routes[0].DisplayName != "直连" || routes[0].Protocol != "direct" {
		t.Fatal("direct display metadata missing")
	}
}
