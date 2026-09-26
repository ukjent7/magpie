package gui

import (
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"sync"

	"github.com/yetone/magpie/internal/library"
)

// The problems of the last change to the library stay on the page until the
// next one, so a reload still says what an agent couldn't be given.
var lastProblems struct {
	sync.Mutex
	p []library.Problem
}

type libraryJSON struct {
	*library.View
	Result *library.Result `json:"result,omitempty"`
	Home   string          `json:"home"` // for the page to show paths under it as ~
}

func libraryView(res *library.Result) (libraryJSON, error) {
	lastProblems.Lock()
	if res != nil {
		lastProblems.p = res.Problems
	}
	p := lastProblems.p
	lastProblems.Unlock()
	v, err := library.Read(p)
	home, _ := os.UserHomeDir()
	return libraryJSON{View: v, Result: res, Home: home}, err
}

// marketJSON is a market's list, and why it may be short: a search that
// couldn't reach the registry still shows what magpie has of its own.
type marketJSON struct {
	Items any    `json:"items"`
	Error string `json:"error,omitempty"`
}

func errText(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

// revealable is every path the Library page shows: the only ones it may
// ask to be shown in the file manager.
func revealable(v *library.View) []string {
	out := []string{v.Dir, v.Backups}
	for _, a := range v.Agents {
		out = append(out, a.Instructions, a.MCP, a.Skills)
	}
	for _, s := range v.Skills {
		out = append(out, library.SkillPath(s.Name))
		if s.Kind == "folder" {
			out = append(out, s.Source)
		}
	}
	for _, s := range v.FoundSkills {
		out = append(out, s.Link)
	}
	return out
}

func libraryRoutes(mux *http.ServeMux, w Windows) {
	mux.HandleFunc("GET /api/library", func(rw http.ResponseWriter, r *http.Request) {
		v, err := libraryView(nil)
		if err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, v)
	})
	mux.HandleFunc("GET /api/library/skill", func(rw http.ResponseWriter, r *http.Request) {
		text, err := library.SkillText(r.URL.Query().Get("name"))
		if err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, map[string]string{"text": text, "path": library.SkillPath(r.URL.Query().Get("name"))})
	})
	// the market: what it offers for a search, and each skill's description
	// once asked for, which skills.sh gives one at a time
	mux.HandleFunc("GET /api/library/market/servers", func(rw http.ResponseWriter, r *http.Request) {
		list, err := library.MarketServers(r.URL.Query().Get("q"))
		writeJSON(rw, marketJSON{Items: list, Error: errText(err)})
	})
	mux.HandleFunc("GET /api/library/market/skills", func(rw http.ResponseWriter, r *http.Request) {
		list, err := library.MarketSkills(r.URL.Query().Get("q"))
		writeJSON(rw, marketJSON{Items: list, Error: errText(err)})
	})
	mux.HandleFunc("POST /api/library/market/about", func(rw http.ResponseWriter, r *http.Request) {
		var in struct{ IDs []string }
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		if len(in.IDs) > 60 {
			in.IDs = in.IDs[:60]
		}
		writeJSON(rw, library.SkillsAbout(in.IDs))
	})
	mux.HandleFunc("GET /api/library/icon", func(rw http.ResponseWriter, r *http.Request) {
		b, ct, err := library.Icon(r.URL.Query().Get("u"))
		if err != nil {
			http.Error(rw, err.Error(), http.StatusNotFound)
			return
		}
		rw.Header().Set("Content-Type", ct)
		rw.Header().Set("Cache-Control", "max-age=604800")
		rw.Header().Set("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'") // an SVG opened on its own runs nothing
		rw.Write(b)
	})
	mux.HandleFunc("POST /api/library/skills/probe", func(rw http.ResponseWriter, r *http.Request) {
		var in struct{ Source string }
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		p, err := library.ProbeSkills(in.Source)
		if err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, p)
	})
	// RTK: which agents have its hook, and switching one on or off
	mux.HandleFunc("GET /api/library/rtk", func(rw http.ResponseWriter, r *http.Request) {
		writeJSON(rw, library.ReadRTK())
	})
	mux.HandleFunc("POST /api/library/rtk", func(rw http.ResponseWriter, r *http.Request) {
		var in struct {
			Agent string
			On    bool
		}
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		v, err := library.SetRTK(in.Agent, in.On)
		if err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, v)
	})
	mux.HandleFunc("POST /api/library/reveal", func(rw http.ResponseWriter, r *http.Request) {
		var in struct{ Path string }
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil || in.Path == "" {
			http.Error(rw, "no path", http.StatusBadRequest)
			return
		}
		v, err := library.Read(nil)
		if err != nil {
			fail(rw, err)
			return
		}
		if !slices.Contains(revealable(v), in.Path) {
			http.Error(rw, "not a path the library shows", http.StatusForbidden)
			return
		}
		// a file is shown in its folder; one not written yet, the folder it will be in
		p := in.Path
		for {
			fi, err := os.Stat(p)
			if err == nil && fi.IsDir() {
				break
			}
			up := filepath.Dir(p)
			if up == p {
				break
			}
			p = up
		}
		if err := w.OpenFolder(p); err != nil {
			fail(rw, err)
			return
		}
		rw.WriteHeader(http.StatusNoContent)
	})
	// every change answers with the page as it is after it, and what it did
	mux.HandleFunc("POST /api/library/{what}/{action}", func(rw http.ResponseWriter, r *http.Request) {
		var in struct {
			Name string
			Old  string
			// Agents is named as the page sends it, so that it is this
			// and not the instructions' own Agents that "agents" fills
			Agents []string `json:"agents"`
			Source string
			Paths  []string
			Server library.Server
			ID     string            // a market server's, or a market skill's in its repository
			Values map[string]string // what a market server needs
			library.InstructionsChange
		}
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			fail(rw, err)
			return
		}
		var res *library.Result
		var err error
		switch r.PathValue("what") + "/" + r.PathValue("action") {
		case "instructions/save":
			c := in.InstructionsChange
			c.Agents = in.Agents
			res, err = library.SaveInstructions(c)
		case "instructions/import":
			res, err = library.ImportInstructions(in.Name)
		case "servers/save":
			res, err = library.SaveServer(in.Old, in.Server)
		case "servers/agents":
			res, err = library.ServerAgents(in.Name, in.Agents)
		case "servers/remove":
			res, err = library.RemoveServer(in.Name)
		case "servers/import":
			res, err = library.ImportServer(in.Name)
		case "skills/install":
			res, err = library.InstallSkills(in.Source, in.Paths, in.Agents)
		case "skills/update":
			res, err = library.UpdateSkill(in.Name)
		case "skills/agents":
			res, err = library.SkillAgents(in.Name, in.Agents)
		case "skills/remove":
			res, err = library.RemoveSkill(in.Name)
		case "skills/import":
			res, err = library.ImportSkill(in.Name)
		case "market/server":
			res, err = library.InstallServer(in.ID, in.Values, in.Agents)
		case "market/skill":
			res, err = library.InstallMarketSkill(in.Source, in.ID, in.Agents)
		case "all/sync":
			res, err = library.Sync()
		default:
			http.NotFound(rw, r)
			return
		}
		if err != nil {
			fail(rw, err)
			return
		}
		v, err := libraryView(res)
		if err != nil {
			fail(rw, err)
			return
		}
		writeJSON(rw, v)
	})
}
