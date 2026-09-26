package library

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/agent"
	"github.com/yetone/magpie/internal/proc"
)

// RTK (rtk-ai.app) is a CLI that the shell commands an agent runs go
// through — git status, cargo test, ls — cut down to what the model needs,
// so their output costs fewer tokens. Each agent gets it through a hook its
// own installer writes (rtk init -g …): magpie runs that installer for the
// agents switched on, and reads each one's files to say which have it, since
// rtk init --show knows only of Claude Code, OpenCode and Cursor.

// RTKURL is where rtk is installed from.
const RTKURL = "https://www.rtk-ai.app"

// rtkSpec is how rtk's installer names one agent, and what it writes there.
type rtkSpec struct {
	flags []string // after rtk init -g
	// patch: the installer takes --auto-patch (Codex's refuses it)
	patch bool
	// has says whether the agent has rtk's hook, by its files
	has func(a *agent.Agent) bool
	// touches are the files the installer edits (not those it only adds),
	// kept aside by magpie first
	touches func(a *agent.Agent) []string
	// dir is the agent's folder, made first: rtk writes nothing for an
	// agent whose folder isn't there yet
	dir func(a *agent.Agent) string
	// withClaude: the installer gives it to Claude Code too, which is
	// pointed at a folder thrown away after
	withClaude bool
}

func contains(path, s string) bool {
	b, err := os.ReadFile(path)
	return err == nil && bytes.Contains(b, []byte(s))
}

func exists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

var rtkSpecs = map[string]rtkSpec{
	"claude": {
		patch: true,
		has: func(*agent.Agent) bool {
			return contains(filepath.Join(claudeDir(), "settings.json"), "rtk hook claude")
		},
		touches: func(*agent.Agent) []string {
			return []string{filepath.Join(claudeDir(), "settings.json"), filepath.Join(claudeDir(), "CLAUDE.md")}
		},
		dir: func(*agent.Agent) string { return claudeDir() },
	},
	"codex": {
		flags: []string{"--codex"},
		has:   func(*agent.Agent) bool { return contains(filepath.Join(codexDir(), "hooks.json"), "rtk hook codex") },
		touches: func(*agent.Agent) []string {
			return []string{filepath.Join(codexDir(), "hooks.json"), filepath.Join(codexDir(), "AGENTS.md")}
		},
		dir: func(*agent.Agent) string { return codexDir() },
	},
	"gemini": {
		flags: []string{"--gemini"}, patch: true,
		has: func(*agent.Agent) bool {
			return contains(filepath.Join(home(), ".gemini", "settings.json"), "rtk-hook-gemini")
		},
		touches: func(*agent.Agent) []string {
			return []string{filepath.Join(home(), ".gemini", "settings.json"), filepath.Join(home(), ".gemini", "GEMINI.md")}
		},
		dir: func(*agent.Agent) string { return filepath.Join(home(), ".gemini") },
	},
	"opencode": {
		flags: []string{"--opencode"}, patch: true, withClaude: true,
		has:     func(a *agent.Agent) bool { return exists(filepath.Join(filepath.Dir(a.Path), "plugins", "rtk.ts")) },
		touches: func(*agent.Agent) []string { return nil },
		dir:     func(a *agent.Agent) string { return filepath.Dir(a.Path) },
	},
	"cursor": {
		flags: []string{"--agent", "cursor"}, patch: true, withClaude: true,
		has: func(*agent.Agent) bool {
			return contains(filepath.Join(home(), ".cursor", "hooks.json"), "rtk hook cursor")
		},
		touches: func(*agent.Agent) []string { return []string{filepath.Join(home(), ".cursor", "hooks.json")} },
		dir:     func(*agent.Agent) string { return filepath.Join(home(), ".cursor") },
	},
	"copilot": {
		flags: []string{"--copilot"}, patch: true,
		has: func(*agent.Agent) bool { return exists(filepath.Join(home(), ".copilot", "hooks", "rtk-rewrite.json")) },
		touches: func(*agent.Agent) []string {
			return []string{filepath.Join(home(), ".copilot", "copilot-instructions.md")}
		},
		dir: func(*agent.Agent) string { return filepath.Join(home(), ".copilot") },
	},
	"pi": {
		flags: []string{"--agent", "pi"}, patch: true,
		has:     func(*agent.Agent) bool { return exists(filepath.Join(home(), ".pi", "agent", "extensions", "rtk.ts")) },
		touches: func(*agent.Agent) []string { return nil },
		dir:     func(*agent.Agent) string { return filepath.Join(home(), ".pi", "agent") },
	},
	"omp": {
		flags: []string{"--agent", "omp"}, patch: true,
		has:     func(*agent.Agent) bool { return exists(filepath.Join(home(), ".omp", "agent", "extensions", "rtk.ts")) },
		touches: func(*agent.Agent) []string { return nil },
		dir:     func(*agent.Agent) string { return filepath.Join(home(), ".omp", "agent") },
	},
	"hermes": {
		flags: []string{"--agent", "hermes"}, patch: true,
		has: func(*agent.Agent) bool {
			return exists(filepath.Join(home(), ".hermes", "plugins", "rtk-rewrite", "plugin.yaml"))
		},
		touches: func(*agent.Agent) []string { return []string{filepath.Join(home(), ".hermes", "config.yaml")} },
		dir:     func(*agent.Agent) string { return filepath.Join(home(), ".hermes") },
	},
}

// rtkBundle: rtk's uninstaller for Claude Code or OpenCode takes it out of
// all three of these at once, so the others are put back after.
var rtkBundle = []string{"claude", "opencode", "cursor"}

// RTKAgent is one agent rtk can be given to, and whether it has it.
type RTKAgent struct {
	ID   string `json:"id"`
	Name string `json:"name"`
	Icon string `json:"icon"`
	On   bool   `json:"on"`
}

// RTKGain is what rtk says it saved, over every command it has recorded.
type RTKGain struct {
	Commands int     `json:"commands"`
	Input    int64   `json:"input"`
	Saved    int64   `json:"saved"`
	Pct      float64 `json:"pct"`
}

// RTKView is the RTK part of the Library page.
type RTKView struct {
	Path    string     `json:"path,omitempty"` // "" when rtk isn't installed
	Version string     `json:"version,omitempty"`
	Gain    *RTKGain   `json:"gain,omitempty"`
	Agents  []RTKAgent `json:"agents"`
	URL     string     `json:"url"`
	// Restart are the agents a change reached, to be restarted to see it
	Restart []string `json:"restart,omitempty"`
	Backup  string   `json:"backup,omitempty"`
}

var rtkMu sync.Mutex

func rtkPath() string {
	p, err := exec.LookPath("rtk")
	if err != nil {
		return ""
	}
	return p
}

func rtkRun(bin string, args ...string) (string, error) { return rtkRunEnv(bin, nil, args...) }

func rtkRunEnv(bin string, env []string, args ...string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cmd := proc.CommandContext(ctx, bin, args...)
	// rtk asks before a change it isn't told to make: nothing answers it
	cmd.Stdin = nil
	if env != nil {
		cmd.Env = append(os.Environ(), env...)
	}
	out, err := cmd.CombinedOutput()
	text := strings.TrimSpace(string(out))
	if err != nil {
		if text == "" {
			text = err.Error()
		}
		return text, fmt.Errorf("rtk %s: %s", strings.Join(args, " "), lastLines(text, 4))
	}
	return text, nil
}

func lastLines(s string, n int) string {
	ls := strings.Split(strings.TrimSpace(s), "\n")
	if len(ls) > n {
		ls = ls[len(ls)-n:]
	}
	return strings.Join(ls, " ")
}

// rtkAgents are the agents on this machine rtk can be given to.
func rtkAgents() []*agent.Agent {
	var out []*agent.Agent
	for _, a := range agent.Detected() {
		if _, ok := rtkSpecs[a.ID]; ok {
			out = append(out, a)
		}
	}
	return out
}

// ReadRTK says whether rtk is installed, what it saved, and which agents
// have it.
func ReadRTK() *RTKView {
	v := &RTKView{Agents: []RTKAgent{}, URL: RTKURL, Path: rtkPath()}
	for _, a := range rtkAgents() {
		v.Agents = append(v.Agents, RTKAgent{ID: a.ID, Name: a.Name, Icon: a.Icon, On: rtkSpecs[a.ID].has(a)})
	}
	if v.Path == "" {
		return v
	}
	if out, err := rtkRun(v.Path, "--version"); err == nil {
		v.Version = strings.TrimSpace(strings.TrimPrefix(out, "rtk"))
	}
	if out, err := rtkRun(v.Path, "gain", "--format", "json"); err == nil {
		var g struct {
			Summary struct {
				Commands int     `json:"total_commands"`
				Input    int64   `json:"total_input"`
				Saved    int64   `json:"total_saved"`
				Pct      float64 `json:"avg_savings_pct"`
			} `json:"summary"`
		}
		if json.Unmarshal([]byte(out), &g) == nil && g.Summary.Commands > 0 {
			s := g.Summary
			v.Gain = &RTKGain{Commands: s.Commands, Input: s.Input, Saved: s.Saved, Pct: s.Pct}
		}
	}
	return v
}

// SetRTK gives rtk to an agent, or takes it away, with rtk's own installer.
func SetRTK(id string, on bool) (*RTKView, error) {
	rtkMu.Lock()
	defer rtkMu.Unlock()
	bin := rtkPath()
	if bin == "" {
		return nil, fmt.Errorf("rtk isn't installed — get it from %s", RTKURL)
	}
	agents := map[string]*agent.Agent{}
	for _, a := range rtkAgents() {
		agents[a.ID] = a
	}
	a := agents[id]
	if a == nil {
		if _, ok := rtkSpecs[id]; !ok {
			return nil, fmt.Errorf("rtk has no hook for %s", id)
		}
		return nil, fmt.Errorf("%s isn't installed", id)
	}
	sp := rtkSpecs[id]
	if sp.has(a) == on {
		v := ReadRTK()
		return v, nil
	}
	// what else the uninstaller takes, to put back
	var back []*agent.Agent
	if !on && (id == "claude" || id == "opencode") {
		for _, o := range rtkBundle {
			if b := agents[o]; b != nil && o != id && rtkSpecs[o].has(b) {
				back = append(back, b)
			}
		}
	}
	b := newBackups()
	for _, x := range append([]*agent.Agent{a}, back...) {
		for _, p := range rtkSpecs[x.ID].touches(x) {
			if err := b.keep(x.ID, p); err != nil {
				return nil, err
			}
		}
	}
	if on {
		if err := rtkInstall(bin, a); err != nil {
			return nil, err
		}
	} else {
		if _, err := rtkRun(bin, append(append([]string{"init", "-g"}, sp.flags...), "--uninstall")...); err != nil {
			return nil, err
		}
		for _, x := range back {
			if err := rtkInstall(bin, x); err != nil {
				return nil, fmt.Errorf("rtk was taken out of %s, but putting it back into %s failed: %w", a.Name, x.Name, err)
			}
		}
		if sp.has(a) {
			return nil, fmt.Errorf("rtk's uninstaller left its hook in %s", a.Name)
		}
	}
	if b.dir != "" {
		pruneBackups()
	}
	v := ReadRTK()
	// those put back are as they were: only this one has anything new
	v.Restart, v.Backup = []string{a.ID}, b.dir
	return v, nil
}

func rtkInstall(bin string, a *agent.Agent) error {
	sp := rtkSpecs[a.ID]
	if err := os.MkdirAll(sp.dir(a), 0o755); err != nil {
		return err
	}
	args := append([]string{"init", "-g"}, sp.flags...)
	if sp.patch {
		args = append(args, "--auto-patch")
	}
	var env []string
	if sp.withClaude {
		tmp, err := os.MkdirTemp("", "magpie-rtk-")
		if err != nil {
			return err
		}
		defer os.RemoveAll(tmp)
		env = []string{"CLAUDE_CONFIG_DIR=" + tmp}
	}
	out, err := rtkRunEnv(bin, env, args...)
	if err != nil {
		return err
	}
	if !sp.has(a) {
		return fmt.Errorf("rtk's installer didn't add its hook to %s: %s", a.Name, lastLines(out, 3))
	}
	return nil
}

// RTKTakes is the id of the agent q names when rtk can be given to it.
func RTKTakes(q string) (string, error) {
	a, err := agent.Find(q)
	if err != nil {
		return "", err
	}
	if _, ok := rtkSpecs[a.ID]; !ok {
		var ids []string
		for id := range rtkSpecs {
			ids = append(ids, id)
		}
		slices.Sort(ids)
		return "", fmt.Errorf("rtk has no hook for %s (it has for %s)", a.Name, strings.Join(ids, ", "))
	}
	return a.ID, nil
}
