// Package filememo keeps what was parsed from a file until the file
// changes, so a page that asks many times over reads it once.
package filememo

import (
	"os"
	"sync"
	"time"
)

type entry struct {
	mod  time.Time
	size int64
	v    any
}

var (
	mu   sync.Mutex
	seen = map[string]entry{}
)

// Read is parse of the file at path, from what it gave last time when the
// file has the same size and time as then. kind tells apart two parses of
// one file. What it returns is shared: the caller must not change it.
func Read[T any](kind, path string, parse func([]byte) (T, error)) (T, error) {
	var zero T
	fi, err := os.Stat(path)
	if err != nil {
		return zero, err
	}
	key := kind + "\x00" + path
	mu.Lock()
	e, ok := seen[key]
	mu.Unlock()
	if ok && e.mod.Equal(fi.ModTime()) && e.size == fi.Size() {
		return e.v.(T), nil
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return zero, err
	}
	v, err := parse(b)
	if err != nil {
		return zero, err
	}
	mu.Lock()
	seen[key] = entry{mod: fi.ModTime(), size: fi.Size(), v: v}
	mu.Unlock()
	return v, nil
}
