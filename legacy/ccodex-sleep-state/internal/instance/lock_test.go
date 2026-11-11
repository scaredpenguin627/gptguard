package instance

import "testing"

func TestExclusiveLockAndRecovery(t *testing.T) {
	dir := t.TempDir()
	first, err := Acquire(dir)
	if err != nil {
		t.Fatal(err)
	}
	if second, err := Acquire(dir); err == nil {
		second.Close()
		t.Fatal("second service acquired same directory")
	}
	if err = first.Close(); err != nil {
		t.Fatal(err)
	}
	recovered, err := Acquire(dir)
	if err != nil {
		t.Fatal(err)
	}
	recovered.Close()
}
