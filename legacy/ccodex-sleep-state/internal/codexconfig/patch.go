// Package codexconfig makes a small, reversible edit to Codex's TOML.
// It never reads auth.json and never rewrites unrelated TOML tables.
package codexconfig

import (
	"bytes"
	"errors"
	"fmt"
	"sort"
	"strconv"
	"strings"

	"github.com/gylive/ccodex-sleep-state/internal/settings"
	"github.com/pelletier/go-toml/v2"
	"github.com/pelletier/go-toml/v2/unstable"
)

const provider = "ccodex-sleep-state"

type edit struct {
	start, end int
	text       string
}

func Patch(original []byte, baseURL string) ([]byte, error) {
	return PatchWithOptions(original, baseURL, Options{})
}

func PatchWithOptions(original []byte, baseURL string, options Options) ([]byte, error) {
	if options.ExpectedConfigSHA256 != "" && digest(original) != options.ExpectedConfigSHA256 {
		return nil, errors.New("Codex configuration changed after provider selection; reconnect using the new configuration")
	}
	selection, err := ResolveWithAuth(original, options.Profile, options.AuthMode)
	if err != nil {
		return nil, err
	}
	var document map[string]any
	if toml.Unmarshal(original, &document) != nil {
		return nil, errors.New("Codex config is not valid TOML; left unchanged")
	}
	if referencesManaged(document) {
		return nil, errors.New("configuration already references the managed provider; recover its previous transaction first")
	}
	if providers, ok := document["model_providers"].(map[string]any); ok {
		if _, exists := providers[provider]; exists {
			return nil, errors.New("provider name already exists; restore the previous service transaction first")
		}
	}
	model := options.Model
	if model == "" {
		model = settings.Model
	}
	if model != "gpt-6-astra" && model != "gpt-5.6-sol" && model != "gpt-5.6-terra" {
		return nil, errors.New("unsupported managed model")
	}
	replacements := map[string]string{"model": strconv.Quote(model), "model_provider": strconv.Quote(provider), "openai_base_url": strconv.Quote(baseURL)}
	var edits []edit
	var parser unstable.Parser
	parser.Reset(original)
	root := true
	var table []string
	foundProfile := false
	for parser.NextExpression() {
		node := parser.Expression()
		if node.Kind == unstable.Table || node.Kind == unstable.ArrayTable {
			root = false
			table = nil
			keys := node.Key()
			for keys.Next() {
				table = append(table, string(keys.Node().Data))
			}
			if selection.Profile != "" && len(table) == 2 && table[0] == "profiles" && table[1] == selection.Profile {
				root = true
				foundProfile = true
			}
			continue
		}
		if (selection.Profile != "" && len(table) == 0) || !root || node.Kind != unstable.KeyValue {
			continue
		}
		keys := node.Key()
		var parts []string
		for keys.Next() {
			parts = append(parts, string(keys.Node().Data))
		}
		if len(parts) != 1 {
			continue
		}
		value, ok := replacements[parts[0]]
		if !ok {
			continue
		}
		raw := node.Value().Raw
		if raw.Length == 0 {
			return nil, errors.New("unsupported root setting; left unchanged")
		}
		edits = append(edits, edit{int(raw.Offset), int(raw.Offset + raw.Length), value})
		delete(replacements, parts[0])
	}
	if parser.Error() != nil {
		return nil, errors.New("cannot parse Codex settings; left unchanged")
	}
	sort.Slice(edits, func(i, j int) bool { return edits[i].start > edits[j].start })
	updated := bytes.Clone(original)
	for _, e := range edits {
		updated = append(append(append([]byte{}, updated[:e.start]...), []byte(e.text)...), updated[e.end:]...)
	}
	var prefix strings.Builder
	for _, key := range []string{"model", "model_provider", "openai_base_url"} {
		if value, ok := replacements[key]; ok {
			fmt.Fprintf(&prefix, "%s = %s\n", key, value)
		}
	}
	if selection.Profile == "" {
		updated = append([]byte(prefix.String()), updated...)
	} else if prefix.Len() > 0 {
		// Append missing keys inside the existing profile table, not at root.
		if !foundProfile {
			return nil, errors.New("selected profile must use an explicit TOML table; left unchanged")
		}
		var pp unstable.Parser
		pp.Reset(updated)
		insertion := len(updated)
		inside := false
		for pp.NextExpression() {
			n := pp.Expression()
			if n.Kind != unstable.Table && n.Kind != unstable.ArrayTable {
				continue
			}
			var names []string
			ks := n.Key()
			tableStart := -1
			for ks.Next() {
				if tableStart < 0 {
					tableStart = int(ks.Node().Raw.Offset)
				}
				names = append(names, string(ks.Node().Data))
			}
			if inside {
				if tableStart < 0 {
					return nil, errors.New("cannot locate profile boundary")
				}
				insertion = tableStart
				for insertion > 0 && updated[insertion-1] != '\n' {
					insertion--
				}
				break
			}
			inside = len(names) == 2 && names[0] == "profiles" && names[1] == selection.Profile
		}
		updated = append(append(append([]byte{}, updated[:insertion]...), []byte("\n"+prefix.String())...), updated[insertion:]...)
	}
	// Retain provider-specific authentication and request headers without reading
	// their environment values. Never turn a relay into an official-auth provider.
	managed := make(map[string]any)
	for _, key := range []string{"env_key", "env_key_instructions", "experimental_bearer_token", "http_headers", "env_http_headers", "requires_openai_auth"} {
		if value, ok := selection.fields[key]; ok {
			managed[key] = value
		}
	}
	// Codex uses this name to discover official remote-compaction support.
	// An arbitrary local label silently disables that capability.
	managed["name"] = "Sleep State (local)"
	if name, ok := selection.fields["name"].(string); ok && name != "" {
		managed["name"] = name
	}
	if selection.Official {
		managed["name"] = "OpenAI"
	}
	managed["base_url"] = baseURL
	managed["wire_api"] = "responses"
	managed["supports_websockets"] = false
	managed["request_max_retries"] = 0
	managed["stream_max_retries"] = 0
	block, err := toml.Marshal(map[string]any{"model_providers": map[string]any{provider: managed}})
	if err != nil {
		return nil, errors.New("cannot encode managed provider")
	}
	updated = append(updated, []byte("\n\n# Managed by ccodex-sleep-state; restore before changing providers.\n")...)
	// The parent table may already be explicitly declared in the user file.
	block = bytes.TrimPrefix(block, []byte("[model_providers]\n"))
	updated = append(updated, block...)
	// Decode into a fresh map: reusing the one holding the original's array
	// tables makes go-toml append through an unaddressable map value and panic.
	var verified map[string]any
	if toml.Unmarshal(updated, &verified) != nil {
		return nil, errors.New("managed provider conflicts with existing TOML; left unchanged")
	}
	return updated, nil
}
