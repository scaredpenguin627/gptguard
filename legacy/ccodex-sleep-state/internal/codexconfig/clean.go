package codexconfig

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/fsutil"
)

type CleanConfigPreview struct {
	Exists       bool   `json:"exists"`
	ValidTOML    bool   `json:"valid_toml"`
	ConfigSHA256 string `json:"config_sha256"`
}

type CleanConfigOptions struct {
	ExpectedConfigSHA256 string
	ExpectedExists       bool
	// Replacement is a complete, explicitly selected configuration, not an
	// automatically guessed extraction from malformed TOML. It is never logged.
	Replacement []byte
	AuthMode    string
}

func cleanTarget(dir, home string) (string, error) {
	if _, err := os.Lstat(journal(dir)); err == nil {
		return "", errors.New("a managed transaction is pending; recover it before rebuilding configuration")
	} else if !os.IsNotExist(err) {
		return "", err
	}
	realHome, err := filepath.EvalSymlinks(home)
	if err != nil {
		return "", err
	}
	target := filepath.Join(realHome, "config.toml")
	if err = fsutil.RefuseLink(target); err != nil {
		return "", err
	}
	return target, nil
}

// PreviewCleanConfig never writes or tries to salvage secrets from malformed
// TOML. A missing home can be initialized by the caller's normal setup flow.
func PreviewCleanConfig(dir, home string) (CleanConfigPreview, error) {
	var result CleanConfigPreview
	target, err := cleanTarget(dir, home)
	if err != nil {
		return result, err
	}
	current, err := os.ReadFile(target)
	if err != nil && !os.IsNotExist(err) {
		return result, err
	}
	result.Exists = err == nil
	result.ConfigSHA256 = digest(current)
	_, err = document(current)
	result.ValidTOML = err == nil
	return result, nil
}

// ResetConfig is an explicit last-resort operation, separate from startup and
// Restore. The user must choose the replacement provider/authentication first.
// It backs up even invalid input verbatim and does not touch auth.json.
func ResetConfig(dir, home string, options CleanConfigOptions) (RecoveryResult, error) {
	var result RecoveryResult
	if options.AuthMode != "chatgpt" && options.AuthMode != "api_key" {
		return result, errors.New("choose a known ChatGPT or API-key login before rebuilding configuration")
	}
	replacement, err := document(options.Replacement)
	if err != nil {
		return result, err
	}
	if referencesManaged(replacement) || tableAt(replacement, "model_providers", provider) != nil {
		return result, errors.New("replacement must select an upstream, not this service's managed provider")
	}
	if _, err = ResolveWithAuth(options.Replacement, "", options.AuthMode); err != nil {
		return result, err
	}
	preview, err := PreviewCleanConfig(dir, home)
	if err != nil {
		return result, err
	}
	if options.ExpectedConfigSHA256 == "" || preview.ConfigSHA256 != options.ExpectedConfigSHA256 || preview.Exists != options.ExpectedExists {
		return result, errors.New("configuration changed; preview rebuilding again")
	}
	target, err := cleanTarget(dir, home)
	if err != nil {
		return result, err
	}
	current, err := os.ReadFile(target)
	exists := err == nil
	if err != nil && !os.IsNotExist(err) {
		return result, err
	}
	if exists != preview.Exists || digest(current) != preview.ConfigSHA256 {
		return result, errors.New("configuration changed during rebuild preview")
	}
	archiveRoot := filepath.Join(dir, "backups")
	if err = os.MkdirAll(archiveRoot, 0700); err != nil {
		return result, err
	}
	archive, err := os.MkdirTemp(archiveRoot, "rebuild-"+time.Now().UTC().Format("20060102T150405")+"-")
	if err != nil {
		return result, err
	}
	if err = fsutil.Write(filepath.Join(archive, "current.toml"), current); err != nil {
		return result, err
	}
	manifest, _ := json.Marshal(map[string]any{"mode": "explicit_rebuild", "current_existed": exists, "current_sha256": digest(current)})
	if err = fsutil.Write(filepath.Join(archive, "recovery.json"), manifest); err != nil {
		return result, err
	}
	if _, err = cleanTarget(dir, home); err != nil {
		return result, err
	}
	if err = replaceCurrent(target, current, exists, options.Replacement, false); err != nil {
		return result, err
	}
	return RecoveryResult{Archive: archive, Mode: "explicit_rebuild"}, nil
}
