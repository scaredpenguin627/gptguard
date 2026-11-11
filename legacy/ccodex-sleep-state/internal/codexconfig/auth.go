package codexconfig

import (
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// ClassifyAuth returns only an authentication category, never credential values.
// A mixed file is deliberately ambiguous: selecting whichever credential happens
// to appear first could disclose an account token to a third-party provider.
func ClassifyAuth(data []byte) string {
	var document map[string]json.RawMessage
	if json.Unmarshal(data, &document) != nil {
		return "ambiguous"
	}
	var key string
	if raw, ok := document["OPENAI_API_KEY"]; ok && string(raw) != "null" {
		if json.Unmarshal(raw, &key) != nil {
			return "ambiguous"
		}
	}
	hasKey := strings.TrimSpace(key) != ""
	var tokens map[string]json.RawMessage
	if raw, ok := document["tokens"]; ok && string(raw) != "null" {
		if json.Unmarshal(raw, &tokens) != nil {
			return "ambiguous"
		}
	}
	hasOAuth := false
	for _, name := range []string{"access_token", "refresh_token", "id_token"} {
		if raw, ok := tokens[name]; ok && string(raw) != "null" {
			var token string
			if json.Unmarshal(raw, &token) != nil {
				return "ambiguous"
			}
			hasOAuth = hasOAuth || strings.TrimSpace(token) != ""
		}
	}
	var declared string
	if raw, ok := document["auth_mode"]; ok && string(raw) != "null" {
		if json.Unmarshal(raw, &declared) != nil {
			return "ambiguous"
		}
	}
	if hasKey && hasOAuth {
		return "ambiguous"
	}
	if hasKey {
		if declared != "" && declared != "apikey" && declared != "api_key" {
			return "ambiguous"
		}
		return "api_key"
	}
	if hasOAuth {
		if declared != "" && declared != "chatgpt" {
			return "ambiguous"
		}
		return "chatgpt"
	}
	return "unknown"
}

// ReadAuthMode is intended for the installed program, not build-time discovery.
// It reads only the explicitly selected Codex home's auth.json, without changing
// or refreshing it. Missing files (for example keychain login) remain unknown.
func ReadAuthMode(codexHome string) (string, error) {
	path := filepath.Join(codexHome, "auth.json")
	const limit = 1 << 20
	before, err := os.Lstat(path)
	if os.IsNotExist(err) {
		return "unknown", nil
	}
	if err != nil || !before.Mode().IsRegular() || before.Size() > limit {
		return "unknown", errors.New("selected Codex authentication file must be a regular file smaller than 1 MiB")
	}
	file, err := os.Open(path)
	if err != nil {
		return "unknown", errors.New("cannot inspect selected Codex authentication file")
	}
	defer file.Close()
	opened, err := file.Stat()
	if err != nil || !opened.Mode().IsRegular() || !os.SameFile(before, opened) || opened.Size() > limit {
		return "unknown", errors.New("selected Codex authentication file changed while opening")
	}
	data, err := io.ReadAll(io.LimitReader(file, limit+1))
	if err != nil || len(data) > limit {
		return "ambiguous", errors.New("selected Codex authentication file cannot be classified safely")
	}
	return ClassifyAuth(data), nil
}
