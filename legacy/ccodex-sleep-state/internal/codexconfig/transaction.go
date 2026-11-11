package codexconfig

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/fsutil"
	"github.com/pelletier/go-toml/v2"
)

type receipt struct {
	Target    string `json:"target"`
	Backup    string `json:"backup"`
	Existed   bool   `json:"existed"`
	Before    string `json:"before_sha256"`
	Installed string `json:"installed_sha256"`
	Snapshot  string `json:"installed_snapshot,omitempty"`
	Profile   string `json:"profile,omitempty"`
}

func digest(b []byte) string    { h := sha256.Sum256(b); return hex.EncodeToString(h[:]) }
func journal(dir string) string { return filepath.Join(dir, "config-transaction.json") }

func Install(dir, codexHome, baseURL string) error {
	return InstallWithOptions(dir, codexHome, baseURL, Options{})
}

func InstallWithOptions(dir, codexHome, baseURL string, options Options) error {
	if _, err := os.Stat(journal(dir)); err == nil {
		return errors.New("unfinished config transaction; run restore before starting again")
	} else if !os.IsNotExist(err) {
		return err
	}
	if err := os.MkdirAll(codexHome, 0700); err != nil {
		return err
	}
	realHome, err := filepath.EvalSymlinks(codexHome)
	if err != nil {
		return err
	}
	target := filepath.Join(realHome, "config.toml")
	if err = fsutil.RefuseLink(target); err != nil {
		return err
	}
	original, err := os.ReadFile(target)
	existed := err == nil
	if err != nil && !os.IsNotExist(err) {
		return err
	}
	updated, err := PatchWithOptions(original, baseURL, options)
	if err != nil {
		return err
	}
	backup, err := os.CreateTemp(realHome, "config.toml.sleep-state-"+time.Now().UTC().Format("20060102T150405")+"-*.bak")
	if err != nil {
		return err
	}
	if err = backup.Chmod(0600); err == nil {
		_, err = backup.Write(original)
	}
	if err == nil {
		err = backup.Sync()
	}
	err = errors.Join(err, backup.Close())
	if err != nil {
		return err
	}
	selection, err := ResolveWithAuth(original, options.Profile, options.AuthMode)
	if err != nil {
		return err
	}
	snapshot := backup.Name() + ".installed"
	if err = fsutil.Write(snapshot, updated); err != nil {
		return err
	}
	record := receipt{Target: target, Backup: backup.Name(), Existed: existed, Before: digest(original), Installed: digest(updated), Snapshot: snapshot, Profile: selection.Profile}
	data, _ := json.MarshalIndent(record, "", "  ")
	if err = fsutil.Write(journal(dir), data); err != nil {
		return err
	}
	// Detect edits made during backup creation. Never clobber them.
	current, readErr := os.ReadFile(target)
	if (readErr != nil && !os.IsNotExist(readErr)) || (readErr == nil) != existed || digest(current) != record.Before {
		return errors.New("Codex config changed during setup; left unchanged, backup retained")
	}
	return fsutil.Write(target, updated)
}

// transaction reads private snapshots only after validating their location and
// checksum. Legacy receipts can be upgraded in memory without rewriting them.
func transaction(dir string) (receipt, []byte, []byte, []byte, bool, error) {
	var r receipt
	data, err := os.ReadFile(journal(dir))
	if err != nil {
		return r, nil, nil, nil, false, err
	}
	if json.Unmarshal(data, &r) != nil || !filepath.IsAbs(r.Target) || filepath.Base(r.Target) != "config.toml" || filepath.Dir(r.Target) != filepath.Dir(r.Backup) || r.Before == "" || r.Installed == "" {
		return r, nil, nil, nil, false, errors.New("invalid config transaction; manual recovery required")
	}
	for _, path := range []string{r.Target, r.Backup} {
		if err = fsutil.RefuseLink(path); err != nil {
			return r, nil, nil, nil, false, err
		}
	}
	original, err := os.ReadFile(r.Backup)
	if err != nil {
		return r, nil, nil, nil, false, err
	}
	if digest(original) != r.Before {
		return r, nil, nil, nil, false, errors.New("backup checksum mismatch; refusing to restore")
	}
	current, err := os.ReadFile(r.Target)
	exists := err == nil
	if err != nil && !os.IsNotExist(err) {
		return r, nil, nil, nil, false, err
	}
	var installed []byte
	if r.Snapshot != "" {
		if filepath.Dir(r.Snapshot) != filepath.Dir(r.Target) {
			return r, nil, nil, nil, false, errors.New("invalid installed snapshot path")
		}
		if err = fsutil.RefuseLink(r.Snapshot); err != nil {
			return r, nil, nil, nil, false, err
		}
		installed, err = os.ReadFile(r.Snapshot)
		if err != nil || digest(installed) != r.Installed {
			return r, nil, nil, nil, false, errors.New("installed snapshot checksum mismatch; refusing recovery")
		}
	} else if digest(current) == r.Installed {
		installed = bytes.Clone(current)
		r.Profile = findManagedProfile(current)
	} else if digest(current) != r.Before || exists != r.Existed {
		installed, r.Profile, err = reconstructLegacy(original, current, r.Installed)
		if err != nil {
			return r, original, nil, current, exists, err
		}
	}
	return r, original, installed, current, exists, nil
}

func Restore(dir string) error {
	r, original, installed, current, exists, err := transaction(dir)
	if os.IsNotExist(err) {
		if _, e := os.Stat(journal(dir)); os.IsNotExist(e) {
			return nil
		}
	}
	if err != nil {
		return err
	}
	if digest(current) == r.Before && exists == r.Existed {
		return os.Remove(journal(dir))
	}
	if !exists {
		return errors.New("Codex 配置文件不见了，出于安全原因暂不覆盖；备份和恢复记录已保留。请在管理面板重新检查配置")
	}
	updated := original
	if digest(current) != r.Installed {
		if err = checkOwnership(original, installed, current, r.Profile); err != nil {
			return errors.New("Codex 配置自接管后发生了变化，出于安全原因暂不覆盖；备份和恢复记录已保留。请在管理面板确认当前 provider、认证方式或选择恢复备份。原因：" + err.Error())
		}
		updated, err = mergeRestore(original, installed, current, r.Profile, false)
		if err != nil {
			return err
		}
	}
	// A byte-identical install can restore its exact original, including absence.
	remove := !r.Existed && digest(current) == r.Installed
	if err = replaceCurrent(r.Target, current, exists, updated, remove); err != nil {
		return err
	}
	return os.Remove(journal(dir))
}

func replaceCurrent(target string, expected []byte, existed bool, updated []byte, remove bool) error {
	if err := fsutil.RefuseLink(target); err != nil {
		return err
	}
	now, err := os.ReadFile(target)
	if (err != nil && !os.IsNotExist(err)) || (err == nil) != existed || !bytes.Equal(now, expected) {
		return errors.New("Codex config changed during recovery; retry without overwriting the new changes")
	}
	if remove {
		if !existed {
			return nil
		}
		return os.Remove(target)
	}
	return fsutil.Write(target, updated)
}

// CheckManaged is a read-only semantic guard. It allows comments and unrelated
// preferences while rejecting routing/authentication changes by CCS or Codex.
func CheckManaged(dir string) error {
	r, original, installed, current, exists, err := transaction(dir)
	if err != nil {
		return err
	}
	if !exists {
		return errors.New("managed Codex configuration is missing; reconnect Codex")
	}
	if digest(current) == r.Installed {
		return nil
	}
	if len(installed) == 0 {
		return errors.New("managed Codex configuration is no longer selected; reconnect Codex")
	}
	return checkOwnership(original, installed, current, r.Profile)
}

func findManagedProfile(data []byte) string {
	doc, _ := document(data)
	for name, v := range tableAt(doc, "profiles") {
		if p, ok := v.(map[string]any); ok && p["model_provider"] == provider {
			return name
		}
	}
	return ""
}

// Version 0.1/0.2 receipts did not store the installed document. Rebuild only a
// byte-exact, checksum-verified historical patch. Never trust current contents
// as a baseline simply because they point at localhost.
func reconstructLegacy(original, current []byte, want string) ([]byte, string, error) {
	doc, err := document(current)
	if err != nil {
		return nil, "", err
	}
	base, _ := tableAt(doc, "model_providers", provider)["base_url"].(string)
	if base == "" {
		return nil, "", errors.New("legacy config changed; refusing to overwrite it; use explicit backup recovery")
	}
	profiles := []string{""}
	before, _ := document(original)
	for name := range tableAt(before, "profiles") {
		profiles = append(profiles, name)
	}
	for _, profile := range profiles {
		for _, auth := range []string{"unknown", "api_key", "chatgpt"} {
			data, e := PatchWithOptions(original, base, Options{Profile: profile, AuthMode: auth})
			if e != nil {
				continue
			}
			selected, e := ResolveWithAuth(original, profile, auth)
			if e != nil {
				continue
			}
			if digest(data) == want {
				return data, selected.Profile, nil
			}
			// The historical release always named this provider Sleep State (local).
			var parsed map[string]any
			toml.Unmarshal(data, &parsed)
			name, _ := tableAt(parsed, "model_providers", provider)["name"].(string)
			encoded, _ := toml.Marshal(map[string]any{"name": name})
			old := bytes.Clone(data)
			needle := []byte(strings.TrimSpace(string(encoded)))
			if at := bytes.LastIndex(old, needle); at >= 0 {
				old = append(append(append([]byte{}, old[:at]...), []byte("name = 'Sleep State (local)'")...), old[at+len(needle):]...)
			}
			if digest(old) == want {
				return old, selected.Profile, nil
			}
		}
	}
	return nil, "", errors.New("legacy configuration cannot be verified; refusing to overwrite it; use explicit backup recovery")
}
