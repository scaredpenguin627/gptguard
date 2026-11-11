package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"
)

func TestInitCheckAndPaths(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	t.Setenv("CODEX_HOME", home)
	var out bytes.Buffer
	for _, command := range []string{"init", "check", "paths", "restore"} {
		out.Reset()
		if err := run(context.Background(), []string{command, "--data-dir", dir}, &out); err != nil {
			t.Fatalf("%s: %v", command, err)
		}
	}
	before, _ := os.ReadFile(filepath.Join(dir, "config.json"))
	if err := run(context.Background(), []string{"init", "--data-dir", dir}, &out); err == nil {
		t.Fatal("init overwrote existing config")
	}
	after, _ := os.ReadFile(filepath.Join(dir, "config.json"))
	if !bytes.Equal(before, after) {
		t.Fatal("config changed")
	}
	if _, err := os.Stat(filepath.Join(home, "config.toml")); !os.IsNotExist(err) {
		t.Fatal("non-serve command configured Codex")
	}
}
func TestHelpAndUnknownCommand(t *testing.T) {
	var out bytes.Buffer
	if err := run(context.Background(), nil, &out); err != nil || out.Len() == 0 {
		t.Fatal("missing help")
	}
	if err := run(context.Background(), []string{"wrong"}, &out); err == nil {
		t.Fatal("unknown command accepted")
	}
}
