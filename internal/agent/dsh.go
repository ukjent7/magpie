package agent

// DeepSeek Harness (dsh) boots from its shipped rows and then applies a
// personal patch list: a YAML list of {id, config} entries, each replacing
// the whole config of the row with that id. Since 0.1.5 every profile has
// its own, ~/.dsh/profiles/<name>/cordis.patch.yml (web and desktop read it
// live); before that there was one, ~/.dsh/config.yaml. magpie points dsh at
// the gateway with such entries: llm-deepseek (endpoint, the catalog as its
// model list; the key is a credential named in apiKeyEnv, which magpie puts
// in ~/.dsh/.env) and agent-default-model (the model new sessions start on).
// Before 0.1.5 that model was agent-loop's main agent (the TUI) and
// api-gateway's route (`dsh -p`, `dsh web`), and the key sat in the entry.
// All are marked as magpie's, the user's own entries stay as they are, and
// one magpie replaces is stashed and put back when it steps out.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"

	"github.com/yetone/magpie/internal/edit"
	"github.com/yetone/magpie/internal/gateway"
)

const dshMark = "# magpie"

// dshKeyRef is the credential dsh signs gateway requests with.
const dshKeyRef = "MAGPIE_API_KEY"

// dshModels are the models dsh reaches on its own, as it ships them.
var dshModels = []Option{
	{Value: "deepseek-flash", Label: "DeepSeek-V41-Flash", Icon: "deepseek-color", Group: "DeepSeek"},
	{Value: "deepseek-v4-pro", Label: "DeepSeek-V4-Pro", Icon: "deepseek-color", Group: "DeepSeek"},
	{Value: "deepseek-v4-flash", Label: "DeepSeek-V4-Flash", Icon: "deepseek-color", Group: "DeepSeek"},
}

func dsh(home string) *Agent {
	dir := os.Getenv("DSH_HOME")
	if dir == "" {
		dir = filepath.Join(home, ".dsh")
	}
	path := filepath.Join(dir, "config.yaml")
	if files := dshProfiles(dir); len(files) > 0 {
		path = files[0]
	}
	return &Agent{
		ID: "dsh", Name: "DeepSeek Harness", Icon: "deepseek-color", Aliases: []string{"deepseek-harness"},
		UA:  []string{"deepseek-harness"},
		Bin: "dsh", Dir: dir, Path: path,
		Sync: func() error { return dshSync(dir) },
		Notice: func() string {
			var notes []string
			if Running(`(^|/)dsh( |$)`) {
				if len(dshProfiles(dir)) > 0 {
					notes = append(notes, "New dsh sessions start on this; one already open keeps its model until you pick another in it. A dsh started with -p or in a terminal reads it at start-up.")
				} else {
					notes = append(notes, "dsh reads its config at start-up — restart open dsh sessions to use this.")
				}
			}
			if dshSettingsEndpoint(filepath.Join(dir, "settings.yaml")) {
				notes = append(notes, "~/.dsh/settings.yaml sets its own DeepSeek endpoint or key, which dsh puts over magpie's; clear it in dsh's Models page to go through magpie.")
			}
			return strings.Join(notes, " ")
		},
		Check: func() string { return dshCheck(dir) },
		Fields: []Field{{
			Key: "model", Label: "model",
			Get: func() string { return dshGet(dir) },
			Set: func(v string) error { return dshSet(dir, v) },
			Options: func(map[string]string) []Option {
				return append(append([]Option{}, dshModels...), viaMagpie("dsh", magpieID+"/")...)
			},
		}},
	}
}

// dshProfiles are the profiles' patch lists (dsh 0.1.5 on), web's first.
func dshProfiles(dir string) []string {
	files, _ := filepath.Glob(filepath.Join(dir, "profiles", "*", "cordis.patch.yml"))
	sort.SliceStable(files, func(i, j int) bool {
		return filepath.Base(filepath.Dir(files[i])) == "web" && filepath.Base(filepath.Dir(files[j])) != "web"
	})
	return files
}

// dshItem is one entry of the patch list, as its lines.
type dshItem struct {
	id     string
	magpie bool
	lines  []string
}

var dshIDLine = regexp.MustCompile(`^(?:- |  )id:\s*['"]?([^'"#\s]+)['"]?\s*(#.*)?$`)

// dshParse splits the patch list into what comes before its first entry and
// the entries. A file that is not a plain block list is left alone.
func dshParse(raw string) (head []string, items []dshItem, err error) {
	for _, l := range splitLinesKeep(raw) {
		t := strings.TrimSpace(l)
		switch {
		case strings.HasPrefix(l, "- ") || l == "-":
			items = append(items, dshItem{lines: []string{l}})
		case len(items) == 0:
			if t != "" && !strings.HasPrefix(t, "#") && t != "[]" {
				return nil, nil, fmt.Errorf("%s is not a list of entries magpie can edit", "config.yaml")
			}
			if t != "[]" {
				head = append(head, l)
			}
			continue
		case t != "" && !strings.HasPrefix(t, "#") && !strings.HasPrefix(l, " "):
			return nil, nil, fmt.Errorf("%s is not a list of entries magpie can edit", "config.yaml")
		default:
			items[len(items)-1].lines = append(items[len(items)-1].lines, l)
		}
		it := &items[len(items)-1]
		if m := dshIDLine.FindStringSubmatch(l); m != nil && it.id == "" {
			it.id = m[1]
			it.magpie = strings.TrimSpace(m[2]) == dshMark
		}
	}
	return head, items, nil
}

func splitLinesKeep(s string) []string {
	s = strings.ReplaceAll(s, "\r\n", "\n")
	s = strings.TrimRight(s, "\n")
	if s == "" {
		return nil
	}
	return strings.Split(s, "\n")
}

func dshRead(path string) ([]string, []dshItem, error) {
	b, err := edit.Read(path)
	if err != nil {
		return nil, nil, err
	}
	return dshParse(string(b))
}

func dshFind(items []dshItem, id string) int {
	for i, it := range items {
		if it.id == id {
			return i
		}
	}
	return -1
}

var dshModelLine = regexp.MustCompile(`^\s+model:\s*(.+?)\s*$`)

// dshGet reads the model new sessions start on: the one last picked in dsh,
// saved in its settings, else the patch list's.
func dshGet(dir string) string {
	files := dshProfiles(dir)
	path, row := filepath.Join(dir, "config.yaml"), "agent-loop"
	if len(files) > 0 {
		path, row = files[0], "agent-default-model"
	}
	_, items, err := dshRead(path)
	if err != nil {
		return ""
	}
	model := ""
	if len(files) > 0 {
		if m := edit.GetYAMLMap(filepath.Join(dir, "settings.yaml"), "agent-default-model"); m["model"] != "" {
			model = m["model"]
		}
	}
	if i := dshFind(items, row); i >= 0 && model == "" {
		for _, l := range items[i].lines {
			if m := dshModelLine.FindStringSubmatch(l); m != nil {
				model = yamlScalar(m[1])
				break
			}
		}
	}
	if model == "" {
		return ""
	}
	if j := dshFind(items, "llm-deepseek"); j >= 0 && items[j].magpie {
		return magpieID + "/" + model
	}
	return model
}

// dshCheck says what keeps a dsh on one of magpie's models from reaching
// the gateway: its llm-deepseek entry pointed elsewhere, or — since 0.1.5 —
// the key it names gone from .env.
func dshCheck(dir string) string {
	if !usesMagpie(dshGet(dir)) {
		return ""
	}
	files := dshProfiles(dir)
	path := filepath.Join(dir, "config.yaml")
	if len(files) > 0 {
		path = files[0]
	}
	_, items, _ := dshRead(path)
	base := ""
	if i := dshFind(items, "llm-deepseek"); i >= 0 {
		for _, l := range items[i].lines {
			if k, v, ok := strings.Cut(strings.TrimSpace(l), ":"); ok && k == "baseURL" {
				base = yamlScalar(strings.TrimSpace(v))
			}
		}
	}
	get := func(string) (string, bool) { return base, base != "" }
	if off := wiringOff("DeepSeek Harness", path, get, "baseURL", gatewayV1()); off != "" {
		return off
	}
	if len(files) > 0 {
		env := filepath.Join(dir, ".env")
		return wiringOff("DeepSeek Harness", env, func(k string) (string, bool) { return edit.GetEnvFile(env, k) },
			dshKeyRef, gateway.Token)
	}
	return ""
}

// yamlScalar reads a plain or quoted YAML scalar.
func yamlScalar(v string) string {
	if i := strings.Index(v, " #"); i >= 0 {
		v = strings.TrimSpace(v[:i])
	}
	if strings.HasPrefix(v, `"`) {
		var s string
		if json.Unmarshal([]byte(v), &s) == nil {
			return s
		}
	}
	return strings.Trim(v, `'"`)
}

func dshStashKey(path, id string) string { return "dsh:" + path + ":" + id }

// dshSet writes v as the model new sessions start on: a catalog model
// through the gateway, one of dsh's own models directly, or "" for dsh's own
// default. Every profile gets it; config.yaml only where there are none.
func dshSet(dir, v string) error {
	ref, viaGateway := strings.CutPrefix(v, magpieID+"/")
	if viaGateway && !isMagpie(ref) {
		return fmt.Errorf("unknown model %q", v)
	}
	legacy := filepath.Join(dir, "config.yaml")
	files := dshProfiles(dir)
	if len(files) == 0 {
		return dshSetFile(legacy, v, false)
	}
	for _, f := range files {
		if err := dshSetFile(f, v, true); err != nil {
			return err
		}
	}
	// what an older dsh was given is no use now
	if _, err := os.Stat(legacy); err == nil {
		if err := dshSetFile(legacy, "", false); err != nil {
			return err
		}
	}
	env := filepath.Join(dir, ".env")
	if viaGateway {
		if err := edit.SetEnvFile(env, edit.KV{Path: dshKeyRef, Value: gateway.Token}); err != nil {
			return err
		}
	} else if _, ok := edit.GetEnvFile(env, dshKeyRef); ok {
		if err := edit.DelEnvFile(env, dshKeyRef); err != nil {
			return err
		}
	}
	// a model picked in dsh is saved in its settings and goes over the
	// patch list; picking one here takes over from it
	settings := filepath.Join(dir, "settings.yaml")
	if v != "" && edit.GetYAMLMap(settings, "agent-default-model") != nil {
		return edit.DelYAML(settings, "agent-default-model")
	}
	return nil
}

// dshSetFile writes v into one patch list; modern is the layout of dsh
// 0.1.5 on.
func dshSetFile(path, v string, modern bool) error {
	head, items, err := dshRead(path)
	if err != nil {
		return err
	}
	ref, viaGateway := strings.CutPrefix(v, magpieID+"/")
	put := func(id string, lines []string) {
		i := dshFind(items, id)
		if i >= 0 && !items[i].magpie {
			stash(map[string]string{dshStashKey(path, id): strings.Join(items[i].lines, "\n")})
		}
		it := dshItem{id: id, magpie: true, lines: lines}
		if i >= 0 {
			items[i] = it
		} else {
			items = append(items, it)
		}
	}
	drop := func(id string) {
		i := dshFind(items, id)
		if i < 0 || !items[i].magpie {
			return
		}
		if old := unstash(dshStashKey(path, id)); old != "" {
			items[i] = dshItem{id: id, lines: strings.Split(old, "\n")}
			return
		}
		items = append(items[:i], items[i+1:]...)
	}

	switch {
	case v == "":
		drop("agent-default-model")
		drop("api-gateway")
		drop("agent-loop")
		drop("llm-deepseek")
	case modern && viaGateway:
		put("llm-deepseek", dshProviderLines(true))
		put("agent-default-model", dshDefaultLines(ref))
	case modern:
		drop("llm-deepseek")
		put("agent-default-model", dshDefaultLines(v))
	case viaGateway:
		put("llm-deepseek", dshProviderLines(false))
		put("agent-loop", dshLoopLines(ref))
		put("api-gateway", dshRouteLines(ref))
	default:
		drop("api-gateway")
		drop("llm-deepseek")
		put("agent-loop", dshLoopLines(v))
	}

	var out []string
	out = append(out, head...)
	for _, it := range items {
		out = append(out, it.lines...)
	}
	if len(items) == 0 {
		if _, err := os.Stat(path); err != nil {
			return nil // nothing was there, nothing to write
		}
		out = append(out, "[]") // dsh wants a list, even an empty one
	}
	return edit.WriteAtomic(path, []byte(strings.Join(out, "\n")+"\n"))
}

func yamlQuote(s string) string {
	b, _ := json.Marshal(s)
	return string(b)
}

// dshProviderLines is the llm-deepseek entry pointing dsh at the gateway,
// with the catalog as the models its /model offers. Since 0.1.5 the key is a
// credential the entry names rather than holds.
func dshProviderLines(modern bool) []string {
	key := "    apiKey: " + yamlQuote(gateway.Token)
	if modern {
		key = "    apiKeyEnv: " + dshKeyRef
	}
	lines := []string{
		"- id: llm-deepseek " + dshMark,
		"  config:",
		key,
		"    baseURL: " + yamlQuote(gatewayV1()),
		"    thinking: enabled",
		"    reasoningEffort: high",
		"    models:",
	}
	ms := magpieModels("dsh")
	if len(ms) == 0 {
		lines[len(lines)-1] = "    models: []"
	}
	for _, m := range ms {
		lines = append(lines, "      - id: "+yamlQuote(m.ID), "        name: "+yamlQuote(m.Name))
		// what dsh would otherwise take for every model: a million tokens
		// of context, 256K out, and text only
		if m.Context > 0 {
			lines = append(lines, fmt.Sprintf("        contextWindow: %d", m.Context))
		}
		if m.Output > 0 {
			lines = append(lines, fmt.Sprintf("        maxTokens: %d", m.Output))
		}
		if m.Images {
			lines = append(lines, "        inputModalities: [text, image]")
		}
	}
	return lines
}

// dshDefaultLines is the agent-default-model entry: the model new sessions
// start on, in every entry point.
func dshDefaultLines(model string) []string {
	return []string{
		"- id: agent-default-model " + dshMark,
		"  config:",
		"    provider: deepseek-official",
		"    model: " + yamlQuote(model),
	}
}

// dshLoopLines is the agent-loop entry dsh ships, with model in place.
func dshLoopLines(model string) []string {
	return []string{
		"- id: agent-loop " + dshMark,
		"  config:",
		"    agents:",
		"      - id: main",
		"        provider: deepseek-official",
		"        model: " + yamlQuote(model),
		"        cwd: !!js process.cwd()",
	}
}

// dshRouteLines is the api-gateway entry: the route headless and web
// sessions start on.
func dshRouteLines(model string) []string {
	return []string{
		"- id: api-gateway " + dshMark,
		"  config:",
		"    provider: deepseek-official",
		"    model: " + yamlQuote(model),
	}
}

// dshSync puts the catalog as it is now into each llm-deepseek entry magpie
// wrote, keyed as that entry is; nothing else in the patch lists changes.
func dshSync(dir string) error {
	files := dshProfiles(dir)
	if len(files) == 0 {
		files = []string{filepath.Join(dir, "config.yaml")}
	}
	for _, f := range files {
		head, items, err := dshRead(f)
		if err != nil {
			continue
		}
		i := dshFind(items, "llm-deepseek")
		if i < 0 || !items[i].magpie {
			continue
		}
		modern := false
		for _, l := range items[i].lines {
			modern = modern || strings.HasPrefix(strings.TrimSpace(l), "apiKeyEnv:")
		}
		lines := dshProviderLines(modern)
		if strings.Join(lines, "\n") == strings.Join(items[i].lines, "\n") {
			continue
		}
		items[i].lines = lines
		out := append([]string{}, head...)
		for _, it := range items {
			out = append(out, it.lines...)
		}
		if err := edit.WriteAtomic(f, []byte(strings.Join(out, "\n")+"\n")); err != nil {
			return err
		}
	}
	return nil
}

// dshSettingsEndpoint reports whether dsh's own settings carry an
// llm-deepseek section with an endpoint or key, which dsh puts over the
// patch list.
func dshSettingsEndpoint(path string) bool {
	b, err := os.ReadFile(path)
	if err != nil {
		return false
	}
	in := false
	for _, l := range splitLinesKeep(string(b)) {
		if l != "" && !strings.HasPrefix(l, " ") && !strings.HasPrefix(l, "#") {
			in = strings.HasPrefix(l, "llm-deepseek:")
			continue
		}
		t := strings.TrimSpace(l)
		if in && (strings.HasPrefix(t, "baseURL:") || strings.HasPrefix(t, "apiKey:")) {
			return true
		}
	}
	return false
}
