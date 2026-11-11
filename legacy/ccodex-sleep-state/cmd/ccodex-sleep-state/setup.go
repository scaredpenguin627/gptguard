package main

import (
	"encoding/json"
	"fmt"
	"io"
	"os"

	"github.com/gylive/ccodex-sleep-state/internal/fsutil"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

type setupConfigError struct{ err error }

func (e *setupConfigError) Error() string {
	return "配置未通过检查，原文件已保留：" + e.err.Error()
}
func (e *setupConfigError) Unwrap() error { return e.err }

// prepareSetup is deliberately idempotent: a second launch uses the user's
// existing choices, even when they are invalid. Repair must never mean reset.
func prepareSetup(path string, out io.Writer) (settings.Config, error) {
	if err := fsutil.RefuseLink(path); err != nil {
		return settings.Config{}, err
	}
	_, err := os.Lstat(path)
	if os.IsNotExist(err) {
		data, marshalErr := json.MarshalIndent(settings.Default(), "", "  ")
		if marshalErr != nil {
			return settings.Config{}, marshalErr
		}
		if err = fsutil.Create(path, append(data, '\n')); err != nil {
			return settings.Config{}, fmt.Errorf("创建配置失败，未覆盖已有文件：%w", err)
		}
		fmt.Fprintln(out, "首次启动，已创建服务配置：", path)
	} else if err != nil {
		return settings.Config{}, err
	} else {
		fmt.Fprintln(out, "使用已有配置，不重置订阅和代理：", path)
	}
	config, err := settings.Load(path)
	if err != nil {
		return config, &setupConfigError{err: err}
	}
	return config, nil
}

// Repair UI is only appropriate for an existing readable regular file, not a
// missing path, symlink, device, or permission failure.
func repairableConfig(path string) bool {
	if err := fsutil.RefuseLink(path); err != nil {
		return false
	}
	file, err := os.Open(path)
	if err != nil {
		return false
	}
	defer file.Close()
	return true
}
