package service

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/fsutil"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

// RunRescue keeps a malformed service config from hiding the repair UI.
// It never takes over Codex or loads proxies from the broken document.
func RunRescue(ctx context.Context, dir, path string, out io.Writer) error {
	cfg := settings.Default()
	cfg.Direct = false
	cfg.InjectionDisabled = true
	return run(ctx, dir, path, cfg, false, true, true, out)
}
func contentSHA(b []byte) string { sum := sha256.Sum256(b); return hex.EncodeToString(sum[:]) }
func (c *control) rescuePreview() (map[string]any, error) {
	if !c.rescue {
		return nil, errors.New("当前不在服务配置修复模式")
	}
	if err := fsutil.RefuseLink(c.path); err != nil {
		return nil, err
	}
	data, err := os.ReadFile(c.path)
	if err != nil {
		return nil, errors.New("无法读取待修复的服务配置")
	}
	if len(data) > 1<<20 {
		return nil, errors.New("配置超过 1 MiB，请先在本机检查文件")
	}
	return map[string]any{"config_sha256": contentSHA(data), "message": "服务配置无法读取。确认后会先备份原文件，再创建关闭注入的默认配置；不会修改 Codex、登录信息或系统代理。"}, nil
}
func (c *control) resetServiceConfig(expected string) error {
	preview, err := c.rescuePreview()
	if err != nil {
		return err
	}
	if expected == "" || preview["config_sha256"] != expected {
		return errors.New("配置已经改变，请重新检查后确认")
	}
	original, err := os.ReadFile(c.path)
	if err != nil || contentSHA(original) != expected {
		return errors.New("配置已改变，未覆盖")
	}
	backup := filepath.Join(c.dir, "backups", "invalid-service-"+time.Now().UTC().Format("20060102T150405.000000000")+".json")
	if err = fsutil.Create(backup, original); err != nil {
		return errors.New("无法创建独立备份，未覆盖")
	}
	cfg := settings.Default()
	cfg.Direct = false
	cfg.InjectionDisabled = true
	data, _ := json.MarshalIndent(cfg, "", "  ")
	fresh, err := os.ReadFile(c.path)
	if err != nil || contentSHA(fresh) != expected {
		return errors.New("备份后配置又被修改，未覆盖")
	}
	if err = fsutil.RefuseLink(c.path); err != nil {
		return err
	}
	return fsutil.Write(c.path, append(data, '\n'))
}
