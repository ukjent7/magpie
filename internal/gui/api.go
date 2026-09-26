// Package gui hosts the desktop app: a tray panel and a regular window that
// share one small web UI. The UI talks to Go over a tiny JSON API served by
// the same handler that serves the static assets, so no binding generator or
// bundler is involved.
package gui

import (
	"context"
	"embed"
	"encoding/json"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/yetone/magpie/internal/agent"
	"github.com/yetone/magpie/internal/catalog"
	"github.com/yetone/magpie/internal/gateway"
	"github.com/yetone/magpie/internal/library"
	"github.com/yetone/magpie/internal/netproxy"
	"github.com/yetone/magpie/internal/profile"
	"github.com/yetone/magpie/internal/provider"
	"github.com/yetone/magpie/internal/settings"
)

// Version is the build's version string, shown in Settings.
var Version = "dev"

//go:embed assets
var assets embed.FS

// Windows is what the API needs from the host application.
type Windows interface {
	HidePanel()
	// ShowMain brings the window up, on the named tab when view is set.
	ShowMain(view string)
	Quit()
	// OpenURL hands a link to the system browser.
	OpenURL(url string)
	// OpenFolder shows a folder in the system file manager.
	OpenFolder(path string) error
	// Copy puts text on the system clipboard, which the page's own
	// navigator.clipboard can't always reach from inside the app.
	Copy(text string) bool
	// FitPanel asks for the panel to be tall enough for its content.
	FitPanel(height int, g Glide)
	// TintPanel paints the panel's tint behind the page, where the system
	// keeps up with the panel's size; false when it can't, for the page to
	// go on painting it itself.
	TintPanel(rgba [4]uint8, ms int) bool
}

type fieldJSON struct {
	Key     string         `json:"key"`
	Label   string         `json:"label"`
	Value   string         `json:"value"`
	Options []agent.Option `json:"options"`
}

type agentJSON struct {
	ID     string      `json:"id"`
	Name   string      `json:"name"`
	Icon   string      `json:"icon"`
	Path   string      `json:"path"`
	Fields []fieldJSON `json:"fields"`
	// Drift: its config no longer does what magpie set, and how to set it again
	Drift *agent.Drift `json:"drift,omitempty"`
}

// clientJSON is an agent, or another client the gateway knows, as a
// request from it is drawn.
type clientJSON struct {
	ID   string `json:"id"`
	Name string `json:"name"`
	Icon string `json:"icon"`
}

type profileJSON struct {
	Name    string `json:"name"`
	Summary string `json:"summary"`
	// Library is what the profile gives out from the library, for one
	// saved with its setup
	Library *profileLibraryJSON `json:"library,omitempty"`
}

type profileLibraryJSON struct {
	Servers      int  `json:"servers"`      // given to at least one agent
	Skills       int  `json:"skills"`       // given to at least one agent
	Instructions bool `json:"instructions"` // some agent gets them
}

type stateJSON struct {
	Agents   []agentJSON       `json:"agents"`
	Clients  []clientJSON      `json:"clients"` // who a request may come from, by id
	Profiles []profileJSON     `json:"profiles"`
	Catalog  string            `json:"catalog"`
	Notice   string            `json:"notice,omitempty"` // advice after a change, e.g. "restart Codex"
	Settings settings.Settings `json:"settings"`
}

// settingsJSON is the Settings page: the two choices plus the facts it shows.
type settingsJSON struct {
	settings.Settings
	Version string `json:"version"`
	Dir     string `json:"dir"`     // where magpie keeps its files, as shown
	Gateway string `json:"gateway"` // the local endpoint
	// the proxy vendor requests go through now, and where it came from:
	// settings, environment, system, off or none
	ProxyNow    string `json:"proxyNow"`
	ProxySource string `json:"proxySource"`
}

func settingsState() settingsJSON {
	s := settingsJSON{Settings: settings.Load(), Version: Version, Dir: tilde(settings.Dir()), Gateway: gateway.URL()}
	s.ProxyNow, s.ProxySource = netproxy.Describe()
	return s
}

// onDock puts the app in the Mac's Dock or takes it out, when the Settings
// page changes that; set by the process that has the app.
var onDock func(bool)

// Handler serves the embedded UI and the JSON API.
// gw is the gateway this process serves, or nil when another magpie has it
// (for now: see startBackend).
func Handler(w Windows, gw *gateway.Server) http.Handler {
	if gw != nil {
		served.Store(gw)
	}
	mux := http.NewServeMux()
	mux.Handle("/", devPage(http.FileServer(http.FS(staticFS()))))
	devRoutes(mux)
	// boot.js hands the page the saved language and theme before it paints:
	// they came only with the settings, so the tabs showed English first
	mux.HandleFunc("GET /boot.js", func(rw http.ResponseWriter, r *http.Request) {
		s := settings.Load()
		b, _ := json.Marshal(map[string]string{"lang": s.Lang, "theme": s.Theme})
		rw.Header().Set("Content-Type", "text/javascript; charset=utf-8")
		rw.Header().Set("Cache-Control", "no-store")
		rw.Write(append(append([]byte("window.bootPrefs = "), b...), ";\n"...))
	})
	mux.HandleFunc("GET /api/state", func(rw http.ResponseWriter, r *http.Request) {
		writeJSON(rw, state())
	})
	mux.HandleFunc("POST /api/set", func(rw http.ResponseWriter, r *http.Request) {
		var in struct{ Agent, Field, Value string }
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		a, err := agent.Find(in.Agent)
		if err != nil {
			fail(rw, err)
			return
		}
		f := a.Field(in.Field)
		if f == nil {
			http.Error(rw, "unknown field", http.StatusBadRequest)
			return
		}
		if err := a.Apply(f.Key, strings.TrimSpace(in.Value)); err != nil {
			fail(rw, err)
			return
		}
		s := state()
		if a.Notice != nil {
			s.Notice = a.Notice()
		}
		writeJSON(rw, s)
	})
	// reapply sets again what magpie set on an agent something else
	// rewrote; keep takes the agent as it is now
	// what drifted, per agent: cheap enough to ask while the window is up,
	// so a config rewritten elsewhere shows without a reload
	mux.HandleFunc("GET /api/drift", func(rw http.ResponseWriter, r *http.Request) {
		out := map[string]*agent.Drift{}
		for _, a := range agent.Detected() {
			if d := a.Drift(); d != nil {
				out[a.ID] = d
			}
		}
		writeJSON(rw, out)
	})
	mux.HandleFunc("POST /api/agents/{action}/{id}", func(rw http.ResponseWriter, r *http.Request) {
		a, err := agent.Find(r.PathValue("id"))
		if err != nil {
			fail(rw, err)
			return
		}
		switch r.PathValue("action") {
		case "reapply":
			err = a.Reapply()
		case "keep":
			a.Keep()
		default:
			http.NotFound(rw, r)
			return
		}
		if err != nil {
			fail(rw, err)
			return
		}
		s := state()
		if a.Notice != nil {
			s.Notice = a.Notice()
		}
		writeJSON(rw, s)
	})
	mux.HandleFunc("POST /api/profile/{action}", func(rw http.ResponseWriter, r *http.Request) {
		var in struct{ Name string }
		_ = json.NewDecoder(r.Body).Decode(&in)
		in.Name = strings.TrimSpace(in.Name)
		var err error
		var applied profile.Applied
		switch r.PathValue("action") {
		case "save":
			var p profile.Profile
			if p, err = profile.Snapshot(); err == nil {
				err = profile.Save(in.Name, p)
			}
		case "use":
			var ps map[string]profile.Profile
			if ps, err = profile.Load(); err == nil {
				applied, err = profile.Apply(ps[in.Name])
			}
			if applied.Library != nil {
				lastProblems.Lock()
				lastProblems.p = applied.Library.Problems
				lastProblems.Unlock()
			}
		case "delete":
			err = profile.Delete(in.Name)
		default:
			http.NotFound(rw, r)
			return
		}
		if err != nil {
			fail(rw, err)
			return
		}
		s := state()
		writeJSON(rw, struct {
			stateJSON
			Changed int `json:"changed"`
			// Library is what bringing the profile's library setup back did
			Library *library.Result `json:"library,omitempty"`
		}{s, applied.Changed, applied.Library})
	})
	mux.HandleFunc("POST /api/sync", func(rw http.ResponseWriter, r *http.Request) {
		ctx, cancel := context.WithTimeout(r.Context(), 30*time.Second)
		defer cancel()
		if err := catalog.Sync(ctx); err != nil {
			fail(rw, err)
			return
		}
		for _, p := range provider.All() {
			if p.Ready() {
				c, cancel := context.WithTimeout(ctx, 8*time.Second)
				p.Fetch(c)
				cancel()
			}
		}
		writeJSON(rw, state())
	})
	providerRoutes(mux, w)
	importRoutes(mux)
	usageRoutes(mux)
	backupRoutes(mux, w)
	libraryRoutes(mux, w)
	updateRoutes(mux, w)
	mux.HandleFunc("GET /api/settings", func(rw http.ResponseWriter, r *http.Request) {
		writeJSON(rw, settingsState())
	})
	mux.HandleFunc("POST /api/settings", func(rw http.ResponseWriter, r *http.Request) {
		var in settings.Settings
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		// the Settings page sends its own choices; how the agents are
		// arranged is the Agents page's, and the window's size its own; both stay as they are
		cur := settings.Load()
		in.AgentOrder, in.AgentsHidden, in.AgentsShown = cur.AgentOrder, cur.AgentsHidden, cur.AgentsShown
		in.Window = cur.Window // the window's own, as it was last resized
		if err := settings.Save(in); err != nil {
			fail(rw, err)
			return
		}
		if in.Dock != cur.Dock && onDock != nil {
			onDock(in.Dock)
		}
		writeJSON(rw, settingsState())
	})
	// the Agents page's order and what it folds away, in magpie's settings
	mux.HandleFunc("POST /api/agents/arrange", func(rw http.ResponseWriter, r *http.Request) {
		var in struct{ Order, Hidden, Shown []string }
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		s := settings.Load()
		s.AgentOrder, s.AgentsHidden, s.AgentsShown = in.Order, in.Hidden, in.Shown
		if err := settings.Save(s); err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, settingsState())
	})
	// the config folder only: the page names no path, so it can't open others
	mux.HandleFunc("POST /api/settings/reveal", func(rw http.ResponseWriter, r *http.Request) {
		if err := w.OpenFolder(settings.Dir()); err != nil {
			fail(rw, err)
			return
		}
		rw.WriteHeader(http.StatusNoContent)
	})
	mux.HandleFunc("POST /api/window/{action}", func(rw http.ResponseWriter, r *http.Request) {
		switch r.PathValue("action") {
		case "hide":
			w.HidePanel()
		case "main":
			w.ShowMain(r.URL.Query().Get("view"))
		case "quit":
			w.Quit()
		case "fit":
			if h, g, ok := parseFit(r.URL.Query()); ok {
				w.FitPanel(h, g)
			}
		case "tint":
			if c, ms, ok := parseTint(r.URL.Query()); ok && w.TintPanel(c, ms) {
				writeJSON(rw, map[string]bool{"ok": true})
				return
			}
		}
		rw.WriteHeader(http.StatusNoContent)
	})
	devListen(mux)
	return mux
}

func state() stateJSON {
	s := stateJSON{Agents: []agentJSON{}, Profiles: []profileJSON{}, Catalog: catalog.Source(), Settings: settings.Load()}
	for _, a := range agent.Clients() {
		s.Clients = append(s.Clients, clientJSON{ID: a.ID, Name: a.Name, Icon: a.Icon})
	}
	for _, a := range agent.Detected() {
		vals := a.Values()
		aj := agentJSON{ID: a.ID, Name: a.Name, Icon: a.Icon, Path: tilde(a.Path)}
		for _, f := range a.Fields {
			opts := f.Options(vals)
			if opts == nil {
				opts = []agent.Option{}
			}
			aj.Fields = append(aj.Fields, fieldJSON{Key: f.Key, Label: f.Label, Value: vals[f.Key], Options: opts})
		}
		aj.Drift = a.Drift()
		s.Agents = append(s.Agents, aj)
	}
	if ps, err := profile.Load(); err == nil {
		for _, n := range profile.Names(ps) {
			pj := profileJSON{Name: n, Summary: profile.Summary(ps[n])}
			if l := ps[n].Library; l != nil {
				servers, skills := l.On()
				pj.Library = &profileLibraryJSON{Servers: servers, Skills: skills, Instructions: l.GivesInstructions()}
			}
			s.Profiles = append(s.Profiles, pj)
		}
	}
	return s
}

func writeJSON(rw http.ResponseWriter, v any) {
	rw.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(rw).Encode(v)
}

func fail(rw http.ResponseWriter, err error) {
	rw.Header().Set("Content-Type", "application/json")
	rw.WriteHeader(http.StatusBadRequest)
	_ = json.NewEncoder(rw).Encode(map[string]string{"error": err.Error()})
}

func tilde(p string) string {
	if home, err := os.UserHomeDir(); err == nil && strings.HasPrefix(p, home) {
		return "~" + p[len(home):]
	}
	return p
}
