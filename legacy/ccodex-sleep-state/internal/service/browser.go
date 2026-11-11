package service

import (
	"context"
	"crypto/rand"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"
)

// A launch ticket is not the control token. It expires after one minute and is
// consumed once. The fragment is stripped before any API request or navigation.
type browserTicket struct {
	mu      sync.Mutex
	value   string
	expires time.Time
	used    bool
}

func newBrowserTicket() (*browserTicket, error) {
	b := make([]byte, 32)
	if _, err := rand.Read(b); err != nil {
		return nil, err
	}
	return &browserTicket{value: hex.EncodeToString(b), expires: time.Now().Add(time.Minute)}, nil
}
func (t *browserTicket) exchange(w http.ResponseWriter, r *http.Request, token string) {
	if r.Method != "POST" {
		w.WriteHeader(405)
		return
	}
	var v struct {
		Ticket string `json:"ticket"`
	}
	d := json.NewDecoder(http.MaxBytesReader(w, r.Body, 256))
	d.DisallowUnknownFields()
	if d.Decode(&v) != nil || d.Decode(new(any)) != io.EOF {
		reply(w, 400, map[string]string{"error": "启动凭证格式无效"})
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.used || time.Now().After(t.expires) || subtle.ConstantTimeCompare([]byte(v.Ticket), []byte(t.value)) != 1 {
		reply(w, 401, map[string]string{"error": "启动链接已过期，请使用终端显示的管理口令"})
		return
	}
	t.used = true
	reply(w, 200, map[string]string{"control_token": token})
}
func browserCommand(goos, url string) *exec.Cmd {
	switch goos {
	case "windows":
		return exec.Command("rundll32.exe", "url.dll,FileProtocolHandler", url)
	case "darwin":
		return exec.Command("open", url)
	default:
		return exec.Command("xdg-open", url)
	}
}
func openPanel(url string) error {
	cmd := browserCommand(runtime.GOOS, url)
	if err := cmd.Start(); err != nil {
		return err
	}
	go func() { _ = cmd.Wait() }()
	return nil
}

func (t *browserTicket) renew() (string, error) {
	next, err := newBrowserTicket()
	if err != nil {
		return "", err
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	t.value, t.expires, t.used = next.value, next.expires, false
	return t.value, nil
}

// ReopenPanel authenticates to an already-running local service. It never puts
// the persistent control token into a browser URL or starts a second service.
func ReopenPanel(ctx context.Context, dir string) error {
	url, err := existingPanelURL(ctx, dir)
	if err != nil {
		return err
	}
	return openPanel(url)
}
func existingPanelURL(ctx context.Context, dir string) (string, error) {
	data, err := os.ReadFile(filepath.Join(dir, "runtime.json"))
	if err != nil {
		return "", errors.New("无法读取已运行服务的状态")
	}
	var rt Runtime
	if json.Unmarshal(data, &rt) != nil {
		return "", errors.New("本地服务状态无效")
	}
	host, _, err := net.SplitHostPort(rt.Address)
	if err != nil || !settings.IsLoopback(host) {
		return "", errors.New("拒绝非本机服务地址")
	}
	req, err := http.NewRequestWithContext(ctx, "POST", "http://"+rt.Address+"/admin/api/new-launch", strings.NewReader("{}"))
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+rt.Token)
	req.Header.Set("Content-Type", "application/json")
	client := &http.Client{Timeout: 3 * time.Second, Transport: &http.Transport{Proxy: nil}, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	defer client.CloseIdleConnections()
	resp, err := client.Do(req)
	if err != nil {
		return "", errors.New("无法连接现有管理面板")
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return "", errors.New("现有服务不支持免口令打开，请使用原服务窗口显示的面板地址和口令")
	}
	var reply struct {
		Ticket string `json:"ticket"`
	}
	if json.NewDecoder(io.LimitReader(resp.Body, 512)).Decode(&reply) != nil || len(reply.Ticket) != 64 {
		return "", errors.New("启动凭证无效")
	}
	if _, err = hex.DecodeString(reply.Ticket); err != nil {
		return "", errors.New("启动凭证无效")
	}
	return "http://" + rt.Address + "/admin/#launch=" + reply.Ticket, nil
}
