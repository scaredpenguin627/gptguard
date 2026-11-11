package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/gylive/ccodex-sleep-state/internal/service"
	"github.com/gylive/ccodex-sleep-state/internal/settings"
)

// The child runs the actual CLI entrypoint, but every writable path is supplied
// by the test. It never has a model credential and never sends a probe.
func TestCLIHelperProcess(t *testing.T) {
	if os.Getenv("CCODEX_TEST_CHILD") != "1" {
		return
	}
	for i, arg := range os.Args {
		if arg == "--" {
			os.Args = append([]string{os.Args[0]}, os.Args[i+1:]...)
			main()
			os.Exit(0)
		}
	}
	os.Exit(2)
}

func TestCrashRecoveryAcrossProcesses(t *testing.T) {
	dir, home := t.TempDir(), t.TempDir()
	original := []byte("# sandbox user's original config\nmodel = 'before'\n")
	configFile := filepath.Join(home, "config.toml")
	if err := os.WriteFile(configFile, original, 0600); err != nil {
		t.Fatal(err)
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	c := settings.Default()
	c.Listen = listener.Addr().String()
	c.CodexHome = home
	listener.Close()
	encoded, _ := json.Marshal(c)
	if err = os.WriteFile(filepath.Join(dir, "config.json"), encoded, 0600); err != nil {
		t.Fatal(err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := exec.Command(executable, "-test.run=^TestCLIHelperProcess$", "--", "serve", "--data-dir", dir)
	command.Env = append(os.Environ(), "CCODEX_TEST_CHILD=1")
	command.Stdout = io.Discard
	command.Stderr = io.Discard
	if err = command.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = command.Process.Kill(); _ = command.Wait() }()
	ready := false
	for deadline := time.Now().Add(10 * time.Second); time.Now().Before(deadline); {
		if _, err = service.Status(context.Background(), dir); err == nil {
			ready = true
			break
		}
		time.Sleep(25 * time.Millisecond)
	}
	if !ready {
		t.Fatal("child did not start")
	}
	if err = run(context.Background(), []string{"restore", "--data-dir", dir}, io.Discard); err == nil {
		t.Fatal("restore raced a live service")
	}
	if err = command.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	_ = command.Wait()
	installed, _ := os.ReadFile(configFile)
	if bytes.Equal(installed, original) {
		t.Fatal("crash did not leave a managed config for recovery test")
	}
	if err = run(context.Background(), []string{"restore", "--data-dir", dir}, io.Discard); err != nil {
		t.Fatal(err)
	}
	restored, _ := os.ReadFile(configFile)
	if !bytes.Equal(restored, original) {
		t.Fatal("crash recovery changed original bytes")
	}
}
