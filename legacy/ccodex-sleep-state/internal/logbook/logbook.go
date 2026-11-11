// Package logbook rotates structured, metadata-only logs. Callers must pass
// fixed event names and allowlisted fields, never request data or raw errors.
package logbook

import (
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"sync"
)

type Writer struct {
	mu        sync.Mutex
	path      string
	file      *os.File
	size, max int64
}

func Open(dir string) (*slog.Logger, *Writer, error) {
	if err := os.MkdirAll(dir, 0700); err != nil {
		return nil, nil, err
	}
	w := &Writer{path: filepath.Join(dir, "service.jsonl"), max: 2 << 20}
	if err := w.open(); err != nil {
		return nil, nil, err
	}
	return slog.New(slog.NewJSONHandler(w, nil)), w, nil
}
func (w *Writer) open() error {
	f, err := os.OpenFile(w.path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	info, err := f.Stat()
	if err != nil {
		f.Close()
		return err
	}
	w.file = f
	w.size = info.Size()
	return nil
}
func (w *Writer) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.file == nil {
		return 0, errors.New("log file is closed")
	}
	if w.size+int64(len(p)) > w.max {
		if err := w.file.Close(); err != nil {
			return 0, err
		}
		w.file = nil
		for i := 3; i >= 1; i-- {
			dst := fmt.Sprintf("%s.%d", w.path, i)
			src := w.path
			if i > 1 {
				src = fmt.Sprintf("%s.%d", w.path, i-1)
			}
			if err := os.Remove(dst); err != nil && !os.IsNotExist(err) {
				return 0, err
			}
			if err := os.Rename(src, dst); err != nil && !os.IsNotExist(err) {
				return 0, err
			}
		}
		if err := w.open(); err != nil {
			return 0, err
		}
	}
	n, err := w.file.Write(p)
	w.size += int64(n)
	return n, err
}
func (w *Writer) Close() error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.file == nil {
		return nil
	}
	err := w.file.Close()
	w.file = nil
	return err
}
