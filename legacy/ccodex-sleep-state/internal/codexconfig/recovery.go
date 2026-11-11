package codexconfig

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/fsutil"
)

// RecoveryPreview contains no configuration values, endpoint URLs or secrets.
// Both fingerprints must be returned unchanged when confirming a recovery.
type RecoveryPreview struct {
	Needed            bool   `json:"needed"`
	CanKeepCurrent    bool   `json:"can_keep_current"`
	CanRestoreBackup  bool   `json:"can_restore_backup"`
	ConfigSHA256      string `json:"config_sha256,omitempty"`
	TransactionSHA256 string `json:"transaction_sha256,omitempty"`
	Reason            string `json:"reason,omitempty"`
}

type RecoveryOptions struct {
	// keep_current selectively removes unchanged managed fields. restore_backup
	// restores the complete pre-install file and must be explicitly confirmed.
	Mode                      string
	ExpectedConfigSHA256      string
	ExpectedTransactionSHA256 string
}

type RecoveryResult struct {
	Archive string `json:"archive"`
	Mode    string `json:"mode"`
}

func PreviewRecovery(dir string) (RecoveryPreview, error) {
	var preview RecoveryPreview
	raw, err := os.ReadFile(journal(dir))
	if os.IsNotExist(err) {
		return preview, nil
	}
	if err != nil {
		return preview, err
	}
	r, original, installed, current, _, err := transaction(dir)
	// A legacy patch may no longer be reconstructible after CCS replaced it.
	// The original backup is still usable, but only via explicit full recovery.
	if err != nil && !(r.Snapshot == "" && original != nil && installed == nil) {
		return preview, err
	}
	preview.Needed = true
	preview.ConfigSHA256 = digest(current)
	preview.TransactionSHA256 = digest(raw)
	preview.CanRestoreBackup = true
	if installed != nil {
		_, mergeErr := mergeRestore(original, installed, current, r.Profile, true)
		preview.CanKeepCurrent = mergeErr == nil
		if mergeErr != nil {
			preview.Reason = mergeErr.Error()
		}
	} else {
		preview.Reason = "旧版配置已被替换，无法验证局部恢复；可先留档当前配置，再恢复接管前备份。"
	}
	return preview, nil
}

// Recover is never called by ordinary shutdown. It requires a user-confirmed
// mode and optimistic concurrency fingerprints from PreviewRecovery. Every
// attempt archives the current file and receipt before making any change.
func Recover(dir string, options RecoveryOptions) (RecoveryResult, error) {
	var result RecoveryResult
	if options.Mode != "keep_current" && options.Mode != "restore_backup" {
		return result, errors.New("choose keep_current or restore_backup recovery")
	}
	preview, err := PreviewRecovery(dir)
	if err != nil {
		return result, err
	}
	if !preview.Needed {
		return result, errors.New("no pending configuration transaction")
	}
	if options.ExpectedConfigSHA256 == "" || options.ExpectedTransactionSHA256 == "" || options.ExpectedConfigSHA256 != preview.ConfigSHA256 || options.ExpectedTransactionSHA256 != preview.TransactionSHA256 {
		return result, errors.New("configuration or receipt changed; preview recovery again")
	}
	r, original, installed, current, exists, err := transaction(dir)
	if err != nil && !(r.Snapshot == "" && original != nil && installed == nil) {
		return result, err
	}
	updated := original
	remove := !r.Existed
	if options.Mode == "keep_current" {
		if !preview.CanKeepCurrent {
			return result, errors.New("cannot safely preserve the changed managed fields; review the backup recovery option")
		}
		updated, err = mergeRestore(original, installed, current, r.Profile, true)
		if err != nil {
			return result, err
		}
		remove = false
	}
	raw, err := os.ReadFile(journal(dir))
	if err != nil {
		return result, err
	}
	if digest(current) != options.ExpectedConfigSHA256 || digest(raw) != options.ExpectedTransactionSHA256 {
		return result, errors.New("configuration changed during recovery preview")
	}
	archiveRoot := filepath.Join(dir, "backups")
	if err = os.MkdirAll(archiveRoot, 0700); err != nil {
		return result, err
	}
	archive, err := os.MkdirTemp(archiveRoot, "recovery-"+time.Now().UTC().Format("20060102T150405")+"-")
	if err != nil {
		return result, err
	}
	for name, data := range map[string][]byte{"current.toml": current, "transaction.json": raw, "before.toml": original} {
		if err = fsutil.Write(filepath.Join(archive, name), data); err != nil {
			return result, err
		}
	}
	manifest, _ := json.Marshal(map[string]any{"mode": options.Mode, "current_existed": exists, "current_sha256": digest(current), "before_sha256": r.Before})
	if err = fsutil.Write(filepath.Join(archive, "recovery.json"), manifest); err != nil {
		return result, err
	}
	// Check the receipt once more as another running service may own it now.
	fresh, err := os.ReadFile(journal(dir))
	if err != nil || digest(fresh) != options.ExpectedTransactionSHA256 {
		return result, errors.New("configuration transaction changed during recovery; nothing overwritten")
	}
	if err = replaceCurrent(r.Target, current, exists, updated, remove); err != nil {
		return result, err
	}
	if err = os.Remove(journal(dir)); err != nil {
		return result, err
	}
	return RecoveryResult{Archive: archive, Mode: options.Mode}, nil
}
