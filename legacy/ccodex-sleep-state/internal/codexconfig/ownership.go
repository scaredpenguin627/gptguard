package codexconfig

import (
	"bytes"
	"errors"
	"reflect"
	"sort"
	"strings"

	"github.com/pelletier/go-toml/v2"
	"github.com/pelletier/go-toml/v2/unstable"
)

var ownedKeys = []string{"model", "model_provider", "openai_base_url"}

func document(data []byte) (map[string]any, error) {
	var doc map[string]any
	if err := toml.Unmarshal(data, &doc); err != nil {
		return nil, errors.New("Codex config is not valid TOML; left unchanged")
	}
	return doc, nil
}

func tableAt(doc map[string]any, path ...string) map[string]any {
	for _, key := range path {
		doc, _ = doc[key].(map[string]any)
	}
	return doc
}

func selectedTable(doc map[string]any, profile string) map[string]any {
	if profile == "" {
		return doc
	}
	return tableAt(doc, "profiles", profile)
}

func sameField(a, b map[string]any, key string) bool {
	av, ae := a[key]
	bv, be := b[key]
	return ae == be && reflect.DeepEqual(av, bv)
}

// Ownership is deliberately narrower than a document hash, but wider than the
// local URL alone. Changes to authentication or the selected upstream must stop
// forwarding; comments, trust settings and unrelated profiles must not.
func checkOwnership(original, installed, current []byte, profile string) error {
	before, err := document(original)
	if err != nil {
		return err
	}
	expected, err := document(installed)
	if err != nil {
		return err
	}
	actual, err := document(current)
	if err != nil {
		return err
	}
	if !sameField(expected, actual, "profile") {
		return errors.New("Codex active profile changed; reconnect after switching profiles")
	}
	for _, key := range ownedKeys {
		if !sameField(selectedTable(expected, profile), selectedTable(actual, profile), key) {
			return errors.New("Codex managed model or provider changed; reconnect after switching providers")
		}
	}
	if !reflect.DeepEqual(tableAt(expected, "model_providers", provider), tableAt(actual, "model_providers", provider)) {
		return errors.New("Codex managed endpoint or authentication changed; reconnect after switching providers")
	}
	// The original provider still defines where the service forwards requests.
	// Guard it even though Codex currently selects the managed provider.
	selected := selectedTable(before, profile)
	if profile != "" {
		// A profile may inherit its original endpoint/provider from root. The
		// local profile override must not hide CCS changing those source values.
		for _, key := range []string{"model_provider", "openai_base_url"} {
			if _, own := selected[key]; !own && !sameField(expected, actual, key) {
				return errors.New("Codex inherited upstream changed; reconnect after switching providers")
			}
		}
	}
	id, _ := selected["model_provider"].(string)
	if id == "" {
		id, _ = before["model_provider"].(string)
	}
	if id == "" {
		id = "openai"
	}
	if !reflect.DeepEqual(tableAt(expected, "model_providers", id), tableAt(actual, "model_providers", id)) {
		return errors.New("Codex original upstream definition changed; reconnect after switching providers")
	}
	return nil
}

type settingSpan struct {
	valueStart, valueEnd, lineStart, lineEnd int
	raw                                      []byte
}

// locateSettings preserves the user's formatting rather than reserializing the
// entire TOML document during recovery. Dotted/inline rewrites are left alone
// when there is no unambiguous editable span.
func locateSettings(data []byte, profile string) (map[string]settingSpan, []edit, error) {
	settings := map[string]settingSpan{}
	var removals []edit
	var parser unstable.Parser
	parser.Reset(data)
	var table []string
	managedStart := -1
	for parser.NextExpression() {
		node := parser.Expression()
		if node.Kind == unstable.Table || node.Kind == unstable.ArrayTable {
			keys := node.Key()
			table = nil
			start := -1
			for keys.Next() {
				if start < 0 {
					start = int(keys.Node().Raw.Offset)
				}
				table = append(table, string(keys.Node().Data))
			}
			for start > 0 && data[start-1] != '\n' {
				start--
			}
			if managedStart >= 0 {
				removals = append(removals, edit{managedStart, start, ""})
				managedStart = -1
			}
			if len(table) >= 2 && table[0] == "model_providers" && table[1] == provider {
				managedStart = start
			}
			continue
		}
		selected := len(table) == 0 && profile == "" || len(table) == 2 && table[0] == "profiles" && table[1] == profile && profile != ""
		if !selected || node.Kind != unstable.KeyValue {
			continue
		}
		keys := node.Key()
		var parts []string
		start := -1
		for keys.Next() {
			if start < 0 {
				start = int(keys.Node().Raw.Offset)
			}
			parts = append(parts, string(keys.Node().Data))
		}
		if len(parts) != 1 {
			continue
		}
		key := parts[0]
		if key != "model" && key != "model_provider" && key != "openai_base_url" {
			continue
		}
		raw := node.Value().Raw
		if raw.Length == 0 {
			return nil, nil, errors.New("cannot safely locate managed setting")
		}
		end := int(raw.Offset + raw.Length)
		for start > 0 && data[start-1] != '\n' {
			start--
		}
		lineEnd := end
		for lineEnd < len(data) && data[lineEnd] != '\n' {
			lineEnd++
		}
		if lineEnd < len(data) {
			lineEnd++
		}
		settings[key] = settingSpan{int(raw.Offset), end, start, lineEnd, bytes.Clone(data[raw.Offset : raw.Offset+raw.Length])}
	}
	if managedStart >= 0 {
		removals = append(removals, edit{managedStart, len(data), ""})
	}
	if parser.Error() != nil {
		return nil, nil, errors.New("cannot parse Codex settings; left unchanged")
	}
	for i := range removals {
		removals[i].text = commentsInRange(data, removals[i].start, removals[i].end)
	}
	return settings, removals, nil
}

// mergeRestore is a three-way reversal: only values still equal to our installed
// values can be reverted. preserveChanged is an explicitly confirmed recovery,
// not the automatic shutdown path.
func mergeRestore(original, installed, current []byte, profile string, preserveChanged bool) ([]byte, error) {
	before, err := document(original)
	if err != nil {
		return nil, err
	}
	expected, err := document(installed)
	if err != nil {
		return nil, err
	}
	actual, err := document(current)
	if err != nil {
		return nil, err
	}
	managed := tableAt(actual, "model_providers", provider)
	if managed != nil && !reflect.DeepEqual(managed, tableAt(expected, "model_providers", provider)) {
		return nil, errors.New("managed provider was edited; refusing to remove its changed endpoint or authentication")
	}
	oldSpans, _, err := locateSettings(original, profile)
	if err != nil {
		return nil, err
	}
	spans, edits, err := locateSettings(current, profile)
	if err != nil {
		return nil, err
	}
	if managed != nil && len(edits) == 0 {
		return nil, errors.New("managed provider uses unsupported inline/dotted TOML; left unchanged")
	}
	a, b, e := selectedTable(actual, profile), selectedTable(before, profile), selectedTable(expected, profile)
	for _, key := range ownedKeys {
		if !sameField(a, e, key) {
			if preserveChanged {
				continue
			}
			return nil, errors.New("managed setting changed; refusing to overwrite it")
		}
		if sameField(a, b, key) {
			continue
		}
		span, found := spans[key]
		if !found {
			return nil, errors.New("cannot safely locate managed setting; left unchanged")
		}
		if _, exists := b[key]; exists {
			old, found := oldSpans[key]
			if !found {
				return nil, errors.New("cannot safely locate original setting; left unchanged")
			}
			edits = append(edits, edit{span.valueStart, span.valueEnd, string(old.raw)})
		} else {
			// Keep a user's inline comment even when removing our inserted key.
			suffix := string(current[span.valueEnd:span.lineEnd])
			comment := ""
			if i := strings.Index(suffix, "#"); i >= 0 {
				comment = suffix[i:]
			}
			edits = append(edits, edit{span.lineStart, span.lineEnd, comment})
		}
	}
	sort.Slice(edits, func(i, j int) bool { return edits[i].start > edits[j].start })
	result := bytes.Clone(current)
	last := len(current)
	for _, e := range edits {
		if e.end > last || e.start < 0 || e.end < e.start {
			return nil, errors.New("overlapping managed settings; left unchanged")
		}
		result = append(append(append([]byte{}, result[:e.start]...), []byte(e.text)...), result[e.end:]...)
		last = e.start
	}
	merged, err := document(result)
	if err != nil {
		return nil, err
	}
	// Never remove a provider while another profile still references it.
	if referencesManaged(merged) {
		return nil, errors.New("another profile still references the managed provider; choose its intended provider before recovery")
	}
	return result, nil
}

func referencesManaged(doc map[string]any) bool {
	if doc["model_provider"] == provider {
		return true
	}
	for _, v := range tableAt(doc, "profiles") {
		if p, ok := v.(map[string]any); ok && p["model_provider"] == provider {
			return true
		}
	}
	return false
}

// Comments are parsed rather than split by lines: a line beginning with # can
// be part of a multiline token and must not be copied out as documentation.
func commentsInRange(data []byte, start, end int) string {
	var parser unstable.Parser
	parser.KeepComments = true
	parser.Reset(data)
	var comments strings.Builder
	var visit func(*unstable.Node)
	visit = func(node *unstable.Node) {
		for ; node != nil; node = node.Next() {
			if node.Kind == unstable.Comment && int(node.Raw.Offset) >= start && int(node.Raw.Offset+node.Raw.Length) <= end {
				comments.Write(data[node.Raw.Offset : node.Raw.Offset+node.Raw.Length])
				comments.WriteByte('\n')
			}
			if child := node.Child(); child != nil {
				visit(child)
			}
		}
	}
	for parser.NextExpression() {
		visit(parser.Expression())
	}
	return comments.String()
}
