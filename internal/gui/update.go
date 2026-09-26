package gui

import (
	"context"
	"errors"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/update"
)

// updater keeps the app current. It asks the feed at start-up and every six
// hours; when a newer release is out and the app may replace itself — the
// bundle on a Mac, the binary elsewhere — the new one is downloaded
// straight away, so all that is left is a restart. Quitting installs it too, and the next launch is
// the new version. Where the app's folder isn't the user's to change, the
// restart asks for the administrator's password; only an app that can't be
// replaced at all (run from its disk image, say) is sent to the release page.
type updater struct {
	mu      sync.Mutex
	state   string // checking | latest | downloading | ready | available | source | error
	latest  *update.Release
	err     string
	bundle  string      // the .app to replace, "" when not in one or stuck
	stuck   string      // why the .app can't be replaced where it is (update.Stuck)
	exe     string      // off the Mac: the binary to replace, "" when not writable
	self    os.FileInfo // exe as this process started from it
	retry   bool        // error: the download failed, and may be tried again
	staged  string
	done    int64 // downloading: bytes so far, of total (0 when unknown)
	total   int64
	onReady func(version string)
}

type updateJSON struct {
	State   string `json:"state"`
	Current string `json:"current"`
	Latest  string `json:"latest,omitempty"`
	Notes   string `json:"notes,omitempty"`
	URL     string `json:"url,omitempty"`
	Stuck   string `json:"stuck,omitempty"` // available: why it can't update itself
	Retry   bool   `json:"retry,omitempty"` // error: a click downloads it again
	Error   string `json:"error,omitempty"`
	Done    int64  `json:"done,omitempty"` // downloading: bytes so far
	Total   int64  `json:"total,omitempty"`
}

var updates = &updater{}

const updateEvery = 6 * time.Hour

func (u *updater) start() {
	if b := update.Bundle(); b != "" {
		if u.stuck = update.Stuck(b); u.stuck == "" {
			u.bundle = b
		}
	} else if runtime.GOOS != "darwin" {
		if exe, err := update.Executable(); err == nil && (update.Writable(filepath.Dir(exe)) || update.CanElevate()) {
			u.exe = exe
			u.self, _ = os.Stat(exe)
			update.RemoveOld(exe)      // what the last updates on Windows moved aside
			update.RemoveStaleNew(exe) // what a magpie left running downloaded again
		}
	}
	go func() {
		time.Sleep(5 * time.Second) // let the app settle first
		for {
			u.check()
			time.Sleep(updateEvery)
		}
	}()
}

// check asks the feed and, when it can, stages the new version.
func (u *updater) check() {
	if u.begin() {
		u.run()
	}
}

// begin marks a check as under way, unless one is or there's nothing left
// to do.
func (u *updater) begin() bool {
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.state == "checking" || u.state == "downloading" || u.state == "ready" {
		return false
	}
	u.state, u.err, u.retry = "checking", "", false
	return true
}

func (u *updater) run() {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	rel, err := update.Latest(ctx)
	u.mu.Lock()
	defer u.mu.Unlock()
	if err != nil {
		u.state, u.err = "error", err.Error()
		return
	}
	u.latest = rel
	switch {
	case !update.Released(Version):
		u.state = "source"
		return
	case !update.Newer(rel.Version, Version):
		u.state = "latest"
		return
	case u.bundle == "" && u.exe == "":
		u.state = "available" // the user fetches it from the release page
		return
	case u.replaced():
		// another magpie put the update in already: a restart is all
		u.state = "ready"
		if u.onReady != nil {
			go u.onReady(rel.Version)
		}
		return
	}
	u.state, u.done, u.total = "downloading", 0, 0
	u.mu.Unlock()
	ctx = update.WithProgress(ctx, func(done, total int64) {
		u.mu.Lock()
		u.done, u.total = done, total
		u.mu.Unlock()
	})
	var staged string
	if u.bundle != "" {
		staged, err = update.Stage(ctx, rel, u.bundle)
	} else {
		staged, err = update.StageBinary(ctx, rel)
	}
	u.mu.Lock()
	if err != nil {
		log.Println("update:", err)
		u.state, u.err, u.retry = "error", err.Error(), true
		return
	}
	u.state, u.staged = "ready", staged
	if u.onReady != nil {
		go u.onReady(rel.Version)
	}
}

// install swaps the staged version in; it reports whether it did. Asked
// (the restart), it may put up the password prompt; on quitting it doesn't,
// and a folder that needs one waits for the next restart. A failure keeps
// the staged version, so the restart can be tried again.
func (u *updater) install(ask bool) bool {
	u.mu.Lock()
	defer u.mu.Unlock()
	if u.replaced() {
		// another magpie updated the binary after this one started: it is
		// in, and swapping this one's download in would move it aside (the
		// download is left: it may be the other's, staged at the same name)
		u.staged = ""
		return true
	}
	if u.staged == "" {
		return false
	}
	var err error
	if u.bundle != "" {
		err = update.Install(u.staged, u.bundle)
		if ask && update.NeedsAdmin(err) {
			err = update.InstallAsAdmin(u.staged, u.bundle)
		}
	} else {
		err = update.InstallBinary(u.staged, u.exe)
		if ask && update.NeedsAdmin(err) && update.CanElevate() {
			err = update.InstallBinaryAsAdmin(u.staged, u.exe)
		}
	}
	u.err = ""
	if err != nil {
		if !errors.Is(err, update.ErrCanceled) {
			log.Println("update:", err)
			u.err = err.Error()
		}
		return false
	}
	u.staged = ""
	return true
}

// replaced is whether the binary was updated by another magpie since this
// one started, which runs on from where it was moved aside.
func (u *updater) replaced() bool {
	return u.exe != "" && update.Replaced(u.exe, u.self)
}

func (u *updater) json() updateJSON {
	u.mu.Lock()
	defer u.mu.Unlock()
	j := updateJSON{State: u.state, Current: Version, Error: u.err, Retry: u.retry}
	if u.state == "available" {
		j.Stuck = u.stuck
	}
	if u.state == "downloading" {
		j.Done, j.Total = u.done, u.total
	}
	if u.latest != nil {
		j.Latest, j.Notes, j.URL = u.latest.Version, u.latest.Notes, u.latest.URL
	}
	return j
}

func updateRoutes(mux *http.ServeMux, w Windows) {
	mux.HandleFunc("GET /api/update", func(rw http.ResponseWriter, r *http.Request) {
		writeJSON(rw, updates.json())
	})
	mux.HandleFunc("POST /api/update/check", func(rw http.ResponseWriter, r *http.Request) {
		updates.check()
		writeJSON(rw, updates.json())
	})
	// install restarts into the staged version; after a failed download it
	// downloads it again, and the page restarts once it's in. Only an app
	// that can't replace itself is sent to the release page.
	mux.HandleFunc("POST /api/update/install", func(rw http.ResponseWriter, r *http.Request) {
		switch j := updates.json(); {
		case j.State == "ready":
			if restartToUpdate() {
				rw.WriteHeader(http.StatusNoContent)
				go w.Quit()
				return
			}
		case j.State == "error" && j.Retry:
			if updates.begin() {
				go updates.run()
			}
		case j.State == "available" && j.URL != "":
			w.OpenURL(j.URL)
		}
		writeJSON(rw, updates.json())
	})
}

// restartToUpdate installs the staged version and arranges for it to open
// once this process is gone; the caller then quits.
func restartToUpdate() bool {
	bundle, exe := updates.bundle, updates.exe
	if !updates.install(true) {
		return false
	}
	var err error
	if bundle != "" {
		err = update.Relaunch(bundle)
	} else {
		err = update.RelaunchBinary(exe)
	}
	if err != nil {
		log.Println("update:", err)
	}
	return true
}
