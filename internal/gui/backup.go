package gui

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"time"

	"github.com/yetone/magpie/internal/backup"
	"github.com/yetone/magpie/internal/davsync"
	"github.com/yetone/magpie/internal/edit"
	"github.com/yetone/magpie/internal/settings"
)

// backupRoutes are the Settings page's Export and Import, and the WebDAV
// sync set up there.
func backupRoutes(mux *http.ServeMux, w Windows) {
	mux.HandleFunc("POST /api/backup/export", func(rw http.ResponseWriter, r *http.Request) {
		var in struct {
			Pass string
			Keys bool
		}
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		b, err := backup.Collect(in.Keys, Version)
		if err != nil {
			fail(rw, err)
			return
		}
		data, err := backup.Seal(b, in.Pass)
		if err != nil {
			fail(rw, err)
			return
		}
		dir := downloads()
		name := filepath.Join(dir, "magpie-"+time.Now().Format("2006-01-02")+backup.Ext)
		for i := 2; ; i++ { // never over an earlier one
			if _, err := os.Stat(name); err != nil {
				break
			}
			name = filepath.Join(dir, fmt.Sprintf("magpie-%s-%d%s", time.Now().Format("2006-01-02"), i, backup.Ext))
		}
		if err := edit.WriteAtomic(name, data); err != nil {
			fail(rw, err)
			return
		}
		os.Chmod(name, 0o600)
		_ = w.OpenFolder(dir) // saved either way; the path is in the answer
		writeJSON(rw, map[string]any{"path": tilde(name), "providers": len(b.Providers), "profiles": len(b.Profiles), "agents": len(b.Agents)})
	})
	mux.HandleFunc("POST /api/backup/import", func(rw http.ResponseWriter, r *http.Request) {
		var in struct {
			Data   string
			Pass   string
			Agents bool
		}
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		data, err := base64.StdEncoding.DecodeString(in.Data)
		if err != nil {
			fail(rw, err)
			return
		}
		b, err := backup.Open(data, in.Pass)
		if err != nil {
			fail(rw, err)
			return
		}
		parts := backup.All
		parts.Agents = in.Agents
		res, err := backup.Restore(b, parts)
		if err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, res)
	})

	mux.HandleFunc("GET /api/davsync", func(rw http.ResponseWriter, r *http.Request) {
		writeJSON(rw, davsync.Status())
	})
	mux.HandleFunc("POST /api/davsync/{action}", func(rw http.ResponseWriter, r *http.Request) {
		ctx, cancel := context.WithTimeout(r.Context(), 90*time.Second)
		defer cancel()
		var err error
		switch r.PathValue("action") {
		case "save": // and sync at once, so a wrong address or password shows now
			var c davsync.Config
			if err = json.NewDecoder(r.Body).Decode(&c); err == nil {
				if err = davsync.Configure(c); err == nil {
					davsync.Now(ctx) // how it went is in the status
				}
			}
		case "now":
			davsync.Now(ctx)
		case "off":
			err = davsync.Off()
		case "dismiss":
			davsync.Dismiss()
		case "reveal":
			err = w.OpenFolder(filepath.Join(settings.Dir(), "sync"))
		default:
			http.NotFound(rw, r)
			return
		}
		if err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, davsync.Status())
	})
}

// downloads is where an export is saved: the Downloads folder, or home
// when there is none.
func downloads() string {
	home, _ := os.UserHomeDir()
	if d := filepath.Join(home, "Downloads"); isDir(d) {
		return d
	}
	return home
}

func isDir(p string) bool {
	fi, err := os.Stat(p)
	return err == nil && fi.IsDir()
}
