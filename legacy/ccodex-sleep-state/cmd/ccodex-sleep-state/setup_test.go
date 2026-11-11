package main

import (
	"bytes"
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestPrepareSetupCreatesAndPreserves(t *testing.T) {
	path := filepath.Join(t.TempDir(), "folder with spaces", "config.json")
	var out bytes.Buffer
	if _, err := prepareSetup(path, &out); err != nil {
		t.Fatal(err)
	}
	original, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	// Whitespace and unknown-but-invalid fields belong to the user too.
	custom := append([]byte("\n  "), original...)
	if err := os.WriteFile(path, custom, 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := prepareSetup(path, &out); err != nil {
		t.Fatal(err)
	}
	after, _ := os.ReadFile(path)
	if !bytes.Equal(custom, after) {
		t.Fatal("setup changed existing config")
	}
	invalid := []byte(`{"unknown_setting":true}`)
	if err := os.WriteFile(path, invalid, 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := prepareSetup(path, &out); err == nil {
		t.Fatal("invalid config accepted")
	}
	after, _ = os.ReadFile(path)
	if !bytes.Equal(invalid, after) {
		t.Fatal("setup reset invalid config")
	}
}

func TestPrepareSetupRejectsNonRegularFile(t *testing.T) {
	var out bytes.Buffer
	if _, err := prepareSetup(t.TempDir(), &out); err == nil {
		t.Fatal("directory accepted")
	}
	if runtime.GOOS == "windows" {
		return
	}
	dir := t.TempDir()
	target, link := filepath.Join(dir, "real.json"), filepath.Join(dir, "config.json")
	if err := os.WriteFile(target, []byte("{}"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(target, link); err != nil {
		t.Fatal(err)
	}
	if _, err := prepareSetup(link, &out); err == nil {
		t.Fatal("symlink accepted")
	}
}

func TestDoctorSafeReport(t *testing.T) {
	data := []byte(`{"configured_codex":false,"config_error":"secret-api-key","route_error":"https://secret-subscription/","token":"secret-token","routes":1,"injection_enabled":true,"sessions":[{"model":"unknown-secret-model","phase":"waiting_for_state","diagnostic":"secret-detail","observed_length":332},{"model":"gpt-5.6-sol","phase":"rate_limited"}]}`)
	var out bytes.Buffer
	if err := writeDiagnosis(data, &out); err != nil {
		t.Fatal(err)
	}
	if strings.Contains(out.String(), "secret") {
		t.Fatal("free-form field escaped whitelist")
	}
	for _, required := range []string{"未接管", "332", "gpt-5.6-sol", "上游限流"} {
		if !strings.Contains(out.String(), required) {
			t.Fatalf("missing %s", required)
		}
	}
	if err := writeDiagnosis([]byte("invalid"), &out); err == nil {
		t.Fatal("invalid status accepted")
	}
}

func TestDoctorWithoutServiceDoesNotCreateFiles(t *testing.T) {
	dir := t.TempDir()
	var out bytes.Buffer
	if err := run(context.Background(), []string{"doctor", "--data-dir", dir}, &out); err != nil {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(dir)
	if err != nil || len(entries) != 0 {
		t.Fatal("doctor modified directory", err)
	}
	if !strings.Contains(out.String(), "先运行 setup") {
		t.Fatal("missing next step")
	}
}

func TestRepairableConfigOnlyExistingRegularFile(t *testing.T) {
	dir := t.TempDir()
	if repairableConfig(filepath.Join(dir, "missing.json")) || repairableConfig(dir) {
		t.Fatal("invalid repair target")
	}
	path := filepath.Join(dir, "config.json")
	if err := os.WriteFile(path, []byte("{ broken"), 0600); err != nil {
		t.Fatal(err)
	}
	if !repairableConfig(path) {
		t.Fatal("existing malformed config cannot be repaired")
	}
}

func TestMacLauncherFindsAdjacentBinaryWithSpaces(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("POSIX launcher")
	}
	data, err := os.ReadFile(filepath.Join("..", "..", "scripts", "start.command"))
	if err != nil {
		t.Fatal(err)
	}
	root := filepath.Join(t.TempDir(), "release with spaces")
	if err := os.MkdirAll(root, 0700); err != nil {
		t.Fatal(err)
	}
	launcher := filepath.Join(root, "start.command")
	if err := os.WriteFile(launcher, data, 0700); err != nil {
		t.Fatal(err)
	}
	stub := []byte("#!/bin/sh\nprintf 'arg=<%s>\\n' \"$@\"\n")
	if err := os.WriteFile(filepath.Join(root, "ccodex-sleep-state"), stub, 0700); err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command("/bin/sh", launcher, "--data-dir", "a path with spaces", "--no-config")
	cmd.Dir = t.TempDir()
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("launcher: %v: %s", err, out)
	}
	for _, arg := range []string{"arg=<setup>", "arg=<--data-dir>", "arg=<a path with spaces>", "arg=<--no-config>"} {
		if !strings.Contains(string(out), arg) {
			t.Fatalf("argument lost: %s; %s", arg, out)
		}
	}
}
