// Package settings keeps the few preferences the desktop app has: which
// palette to paint with, which language to speak, how the agents are
// arranged. Everything else magpie knows is derived from the agents' own
// files.
//
// The file is ~/.config/magpie/settings.json; a missing file means "follow
// the system" for both.
package settings

import (
	"encoding/json"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strings"
)

// Settings is what the user chose. "" and "system" both mean "follow the OS".
type Settings struct {
	Theme string `json:"theme,omitempty"` // system | light | dark
	Lang  string `json:"lang,omitempty"`  // system | en | zh
	Tray  string `json:"tray,omitempty"`  // what clicking the tray icon opens: panel | window
	// Dock keeps magpie in the Mac's Dock as well as the menu bar, for a
	// menu bar too full to show its icon.
	Dock bool `json:"dock,omitempty"`
	// Proxy for magpie's own requests to vendors: "" follows the
	// environment and then the system, "direct" uses none, anything else
	// is the proxy (http://, https:// or socks5://; host:port means http).
	Proxy string `json:"proxy,omitempty"`
	// Redact keeps secrets in what agents send (API keys, private keys,
	// tokens, passwords) from the vendors behind magpie: they go as
	// placeholders, and come back as they were. RedactPersonal does the same
	// for emails, phone numbers and ID and bank card numbers, and
	// RedactWords for the user's own words.
	Redact         bool     `json:"redact,omitempty"`
	RedactPersonal bool     `json:"redactPersonal,omitempty"`
	RedactWords    []string `json:"redactWords,omitempty"`
	// How the agents are listed, by agent id. AgentOrder comes first, as
	// ordered; an agent it doesn't name (one installed since) follows in
	// magpie's own order. A hidden agent is folded away at the bottom of the
	// list; a shown one stays in view even while nothing is set on it, which
	// otherwise folds it away too. The agents' own files never hear of it.
	AgentOrder   []string `json:"agentOrder,omitempty"`
	AgentsHidden []string `json:"agentsHidden,omitempty"`
	AgentsShown  []string `json:"agentsShown,omitempty"`
	// Visible narrows the models an agent is shown, by agent id: the
	// families (the tag a provider or group is given), provider ids and
	// group ids its lists hold. An agent it doesn't name is shown them all.
	Visible map[string][]string `json:"visible,omitempty"`
	// The main window's size when it was last resized, width and height,
	// so it opens at it again after a restart.
	Window []int `json:"window,omitempty"`
}

// Arrange puts items in the order the user gave the agents, those named
// first and the rest after in the order they came, and splits off the
// hidden ones, which keep that order too. id names an item's agent.
func Arrange[T any](s Settings, items []T, id func(T) string) (shown, hidden []T) {
	rank := map[string]int{}
	for i, x := range s.AgentOrder {
		if _, dup := rank[x]; !dup {
			rank[x] = i
		}
	}
	sorted := slices.Clone(items)
	slices.SortStableFunc(sorted, func(a, b T) int {
		ra, oka := rank[id(a)]
		rb, okb := rank[id(b)]
		switch {
		case oka && okb:
			return ra - rb
		case oka:
			return -1
		case okb:
			return 1
		}
		return 0
	})
	for _, x := range sorted {
		if slices.Contains(s.AgentsHidden, id(x)) {
			hidden = append(hidden, x)
		} else {
			shown = append(shown, x)
		}
	}
	return shown, hidden
}

// Themes and Langs are the accepted values, in the order the UI offers them.
var (
	Themes = []string{"system", "light", "dark"}
	Langs  = []string{"system", "en", "zh"}
	Trays  = []string{"panel", "window"}
)

// Path is the settings file.
func Path() string {
	if x := os.Getenv("XDG_CONFIG_HOME"); x != "" {
		return filepath.Join(x, "magpie", "settings.json")
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "magpie", "settings.json")
}

// Dir is the folder every magpie file lives in.
func Dir() string { return filepath.Dir(Path()) }

// Load reads the settings; anything missing or unreadable is the default.
func Load() Settings {
	var s Settings
	if b, err := os.ReadFile(Path()); err == nil {
		_ = json.Unmarshal(b, &s)
	}
	return s.normal()
}

// Save validates and writes the settings.
func Save(s Settings) error {
	s = s.normal()
	if !slices.Contains(Themes, s.Theme) {
		return fmt.Errorf("theme must be one of %v, not %q", Themes, s.Theme)
	}
	if !slices.Contains(Langs, s.Lang) {
		return fmt.Errorf("language must be one of %v, not %q", Langs, s.Lang)
	}
	if !slices.Contains(Trays, s.Tray) {
		return fmt.Errorf("tray must be one of %v, not %q", Trays, s.Tray)
	}
	s.Proxy = strings.TrimSpace(s.Proxy)
	if s.Proxy != "" && s.Proxy != "direct" {
		raw := s.Proxy
		if !strings.Contains(raw, "://") {
			raw = "http://" + raw
		}
		u, err := url.Parse(raw)
		if err != nil || u.Host == "" || !slices.Contains([]string{"http", "https", "socks5", "socks5h"}, u.Scheme) {
			return fmt.Errorf("proxy must look like http://127.0.0.1:7890 or socks5://127.0.0.1:1080, not %q", s.Proxy)
		}
	}
	s.AgentOrder, s.AgentsHidden, s.AgentsShown = ids(s.AgentOrder), ids(s.AgentsHidden), ids(s.AgentsShown)
	if err := os.MkdirAll(Dir(), 0o755); err != nil {
		return err
	}
	b, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(Path(), append(b, '\n'), 0o644)
}

func (s Settings) normal() Settings {
	if s.Theme == "" {
		s.Theme = "system"
	}
	if s.Lang == "" {
		s.Lang = "system"
	}
	if s.Tray == "" {
		s.Tray = "panel"
	}
	return s
}

// ids trims, drops empties and repeats, and keeps the first of each.
func ids(in []string) []string {
	var out []string
	for _, x := range in {
		if x = strings.TrimSpace(x); x != "" && !slices.Contains(out, x) {
			out = append(out, x)
		}
	}
	return out
}
