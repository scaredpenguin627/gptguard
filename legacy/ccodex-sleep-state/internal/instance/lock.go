// Package instance holds a process-lifetime advisory lock. The OS releases it
// after a crash; the lock file's existence does not mean a service is alive.
package instance

import (
	"errors"
	"os"
	"path/filepath"
)

type Lock struct{ file *os.File }

func Acquire(dir string) (*Lock, error) {
	if err := os.MkdirAll(dir, 0700); err != nil {
		return nil, err
	}
	f, err := os.OpenFile(filepath.Join(dir, "service.lock"), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, err
	}
	if err = lockFile(f); err != nil {
		f.Close()
		return nil, errors.New("another service owns this data directory")
	}
	return &Lock{f}, nil
}
func (l *Lock) Close() error { return l.file.Close() }
