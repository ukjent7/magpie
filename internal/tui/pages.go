package tui

// The pages beside the agents: providers (keys, models, balances), routing
// groups (members, their order, how they rotate) and usage — what the app's
// Providers, Routing and Usage views do, in the terminal.

import (
	"context"
	"fmt"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/yetone/magpie/internal/agent"
	"github.com/yetone/magpie/internal/provider"
	"github.com/yetone/magpie/internal/usage"
)

type page int

const (
	pageAgents page = iota
	pageProviders
	pageGroups
	pageUsage
	pageLibrary
)

var pageNames = []string{"agents", "providers", "groups", "usage", "library"}

// ask is a line to type: a key, a family, a group's name.
type ask struct {
	crumbs  []string
	hint    string
	input   textinput.Model
	empty   bool // enter with nothing typed is an answer too
	onEnter func(string) tea.Cmd
}

// askMsg opens a line to type, from a step that comes before it.
type askMsg struct{ a ask }

// balanceMsg is what is left on each provider's key, by id.
type balanceMsg map[string]string

// ---- providers --------------------------------------------------------------

func (m *model) reloadProviders() {
	m.provs = provider.All()
	m.prow = clamp(m.prow, len(m.provs))
}

// balancesCmd asks every vendor it can what is left on its key.
func balancesCmd(ps []provider.Provider) tea.Cmd {
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		out := balanceMsg{}
		var mu sync.Mutex
		var wg sync.WaitGroup
		for _, p := range ps {
			wg.Add(1)
			go func() {
				defer wg.Done()
				amount, ok, err := provider.Balance(ctx, p)
				if !ok {
					return
				}
				v := amount
				if err != nil {
					v = "balance: " + err.Error()
					if r := []rune(v); len(r) > 60 {
						v = string(r[:60]) + "…"
					}
				}
				mu.Lock()
				out[p.ID] = v
				mu.Unlock()
			}()
		}
		wg.Wait()
		return out
	}
}

func (m model) updateProviders(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	n := len(m.provs)
	key := msg.String()
	if key != "d" {
		m.confirm = ""
	}
	switch key {
	case "j", "down":
		if n > 0 {
			m.prow = (m.prow + 1) % n
		}
		return m, nil
	case "k", "up":
		if n > 0 {
			m.prow = (m.prow + n - 1) % n
		}
		return m, nil
	case "a":
		m.openPresets()
		return m, nil
	case "b":
		m.flash, m.flashOK = "asking the vendors for balances…", true
		return m, balancesCmd(m.provs)
	}
	if n == 0 {
		return m, nil
	}
	p := m.provs[m.prow]
	switch key {
	case "enter", " ":
		m.openProviderModels(p.ID)
	case "e":
		if p.Account != nil {
			m.flash, m.flashOK = p.Name+" is a signed-in account: its key is the agent's sign-in", false
			return m, nil
		}
		in := newInput("the API key")
		in.EchoMode = textinput.EchoPassword
		in.EchoCharacter = '•'
		m.openAsk(ask{crumbs: []string{"providers", p.Name, "key"}, input: in,
			hint: "now " + dash(provider.Mask(p.Key)) + " · the key is kept in magpie's providers file",
			onEnter: func(v string) tea.Cmd {
				return saveProvider(p.ID, func(p *provider.Provider) { p.Key = v; provider.ForgetBalances() }, p.Name+" key "+provider.Mask(v))
			}})
	case "f":
		in := newInput("a tag, e.g. relay")
		in.SetValue(p.Family)
		m.openAsk(ask{crumbs: []string{"providers", p.Name, "family"}, input: in, empty: true,
			hint: "agents can be shown only some families: magpie visible <agent> <family>",
			onEnter: func(v string) tea.Cmd {
				return saveProvider(p.ID, func(p *provider.Provider) { p.Family = v }, p.Name+" family "+dash(v))
			}})
	case "u":
		on := !p.Unlisted
		what := "its models are listed"
		if on {
			what = "serves only through routing groups"
		}
		return m, saveProvider(p.ID, func(p *provider.Provider) { p.Unlisted = on }, p.Name+" "+what)
	case "t":
		m.flash, m.flashOK = "testing "+p.Name+"…", true
		return m, testCmd(p)
	case "d":
		if m.confirm != "provider/"+p.ID {
			m.confirm = "provider/" + p.ID
			m.flash, m.flashOK = "press d again to remove "+p.Name, false
			return m, nil
		}
		m.confirm = ""
		return m, func() tea.Msg {
			if err := provider.Delete(p.ID); err != nil {
				return flashMsg{text: err.Error()}
			}
			return flashMsg{text: "removed " + p.Name, ok: true}
		}
	}
	return m, nil
}

// saveProvider changes one provider and saves it.
func saveProvider(id string, change func(*provider.Provider), done string) tea.Cmd {
	return func() tea.Msg {
		p, err := provider.Find(id)
		if err != nil {
			return flashMsg{text: err.Error()}
		}
		change(p)
		if err := provider.Save(*p); err != nil {
			return flashMsg{text: err.Error()}
		}
		return flashMsg{text: done, ok: true}
	}
}

func testCmd(p provider.Provider) tea.Cmd {
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		var parts []string
		ok := true
		for _, r := range p.Test(ctx) {
			if r.OK {
				parts = append(parts, fmt.Sprintf("%s ✓ %d ms", r.Protocol, r.Millis))
				continue
			}
			ok = false
			e := r.Error
			if r.Status != 0 {
				e = fmt.Sprintf("%d %s", r.Status, r.Error)
			}
			if rs := []rune(e); len(rs) > 80 {
				e = string(rs[:80]) + "…"
			}
			parts = append(parts, fmt.Sprintf("%s ✗ %s", r.Protocol, e))
		}
		if len(parts) == 0 {
			return flashMsg{text: p.Name + ": nothing to test"}
		}
		return flashMsg{text: p.Name + ": " + strings.Join(parts, " · "), ok: ok}
	}
}

// modelOptions are a provider's models, those agents are shown marked.
func modelOptions(id string) []agent.Option {
	p, err := provider.Find(id)
	if err != nil {
		return nil
	}
	on := map[string]bool{}
	for _, x := range p.Exposed() {
		on[x.ID] = true
	}
	var out []agent.Option
	seen := map[string]bool{}
	add := func(id, name string) {
		if seen[id] {
			return
		}
		seen[id] = true
		note := "○ off"
		if on[id] {
			note = "● on"
		}
		if name != "" && name != id {
			note += " · " + name
		}
		out = append(out, agent.Option{Value: id, Note: note})
	}
	for _, x := range p.Exposed() {
		add(x.ID, x.Name)
	}
	for _, x := range p.Available() {
		add(x.ID, x.Name)
	}
	return out
}

// openProviderModels lists a provider's models; enter turns one on or off
// for agents, and the list stays open.
func (m *model) openProviderModels(id string) {
	p := m.provs[m.prow]
	m.pk = picker{
		crumbs: []string{"providers", p.Name, "models"},
		input:  newInput("filter models"),
		items:  modelOptions(id),
		empty:  "no models known yet · t tests it, which asks the vendor for them",
		toggle: func(model string) ([]agent.Option, string, bool) {
			p, err := provider.Find(id)
			if err != nil {
				return nil, err.Error(), false
			}
			ids := p.Models
			if len(ids) == 0 {
				for _, x := range p.Exposed() {
					ids = append(ids, x.ID)
				}
			}
			verb := "on"
			if i := slices.Index(ids, model); i >= 0 {
				if len(ids) == 1 {
					return nil, "one model has to stay on", false
				}
				ids = slices.Delete(slices.Clone(ids), i, i+1)
				verb = "off"
			} else {
				ids = append(slices.Clone(ids), model)
			}
			p.Models = ids
			if err := provider.Save(*p); err != nil {
				return nil, err.Error(), false
			}
			items := modelOptions(id)
			if !slices.ContainsFunc(items, func(o agent.Option) bool { return o.Value == model }) {
				// a vendor whose list wasn't fetched knows only those on:
				// the one just turned off stays to be turned on again
				items = append(items, agent.Option{Value: model, Note: "○ off"})
			}
			return items, model + " " + verb, true
		},
	}
	m.pk.refilter()
	m.mode = modePick
}

// openPresets picks a vendor magpie knows, then asks for its key.
func (m *model) openPresets() {
	var items []agent.Option
	for _, d := range provider.Presets() {
		items = append(items, agent.Option{Value: d.ID, Note: d.Name + " · " + string(d.Kind)})
	}
	m.pk = picker{
		crumbs: []string{"providers", "add"},
		input:  newInput("a vendor magpie knows (magpie provider add <name> url=… for another)"),
		items:  items,
		onPick: func(id string) tea.Cmd {
			return func() tea.Msg {
				p, err := provider.FromPreset(id)
				if err != nil {
					return flashMsg{text: err.Error()}
				}
				in := newInput("the API key")
				in.EchoMode = textinput.EchoPassword
				in.EchoCharacter = '•'
				hint := "the key is kept in magpie's providers file"
				if p.KeysURL != "" {
					hint = "keys: " + p.KeysURL
				}
				return askMsg{ask{crumbs: []string{"providers", "add", p.Name}, input: in, hint: hint, empty: true,
					onEnter: func(key string) tea.Cmd {
						return func() tea.Msg {
							p.Key = key
							id, err := provider.Add(p)
							if err != nil {
								return flashMsg{text: err.Error()}
							}
							ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
							defer cancel()
							text := "added " + p.Name
							if saved, err := provider.Find(id); err == nil {
								if ms, err := saved.Fetch(ctx); err == nil {
									text += fmt.Sprintf(" · %d models", len(ms))
								}
							}
							return flashMsg{text: text, ok: true}
						}
					}}}
			}
		},
	}
	m.pk.refilter()
	m.mode = modePick
}

func (m model) viewProviders() string {
	var b strings.Builder
	b.WriteString(m.header())
	b.WriteString("\n\n")
	if len(m.provs) == 0 {
		b.WriteString(pad + "  " + sMuted.Render("no providers yet · a adds one"))
		return b.String()
	}
	type row struct{ name, id, key, models, note string }
	var rows []row
	var w [4]int
	for _, p := range m.provs {
		r := row{name: p.Name, id: p.ID}
		switch {
		case p.Account != nil:
			r.key = "● " + p.Account.User
		case p.Key != "":
			r.key = "● " + provider.Mask(p.Key)
		case p.Ready():
			r.key = "● no key needed"
		default:
			r.key = "○ no key"
		}
		r.models = fmt.Sprintf("%d models", len(p.Exposed()))
		var notes []string
		if v, ok := m.bal[p.ID]; ok {
			notes = append(notes, v)
		}
		if p.Family != "" {
			notes = append(notes, "family "+p.Family)
		}
		if p.Unlisted {
			notes = append(notes, "groups only")
		}
		r.note = strings.Join(notes, " · ")
		for i, s := range []string{r.name, r.id, r.key, r.models} {
			w[i] = max(w[i], lipgloss.Width(s))
		}
		rows = append(rows, r)
	}
	visible := max(3, m.h-7)
	start := 0
	if m.prow >= visible {
		start = m.prow - visible + 1
	}
	for i := start; i < min(len(rows), start+visible); i++ {
		r := rows[i]
		marker, name := "  ", sName.Render(padRight(r.name, w[0]))
		if i == m.prow {
			marker, name = sCursor.Render("▸ "), sNameOn.Render(padRight(r.name, w[0]))
		}
		key := sMuted.Render(padRight(r.key, w[2]))
		if strings.HasPrefix(r.key, "○") {
			key = sBad.Render(padRight(r.key, w[2]))
		}
		b.WriteString(pad + marker + name + "  " + sFaint.Render(padRight(r.id, w[1])) + "  " + key + "  " + sText.Render(padRight(r.models, w[3])) + "  " + sMuted.Render(r.note) + "\n")
	}
	return strings.TrimRight(b.String(), "\n")
}

// ---- groups -----------------------------------------------------------------

func (m *model) reloadGroups() {
	m.groups = m.groups[:0]
	for _, g := range provider.Groups() {
		if !g.Hidden {
			m.groups = append(m.groups, g)
		}
	}
	m.grow = clamp(m.grow, len(m.groups))
}

var routings = []string{provider.Ordered, provider.Rotate, provider.LeastUsed}

func routingName(v string) string {
	switch v {
	case provider.Rotate:
		return "rotate"
	case provider.LeastUsed:
		return "least used"
	}
	return "in order"
}

// saveGroup changes one group and saves it: one magpie found becomes the
// user's.
func saveGroup(id string, change func(*provider.Group) error, done string) tea.Cmd {
	return func() tea.Msg {
		g, ok := findGroup(id)
		if !ok {
			return flashMsg{text: "no group " + id}
		}
		if err := change(&g); err != nil {
			return flashMsg{text: err.Error()}
		}
		if err := provider.SaveGroup(g); err != nil {
			return flashMsg{text: err.Error()}
		}
		return flashMsg{text: done, ok: true}
	}
}

// findGroup is a group by its id, without group/ in front.
func findGroup(id string) (provider.Group, bool) {
	for _, g := range provider.Groups() {
		if g.ID == id && !g.Hidden {
			return g, true
		}
	}
	return provider.Group{}, false
}

func nextRouting(g *provider.Group) error {
	i := slices.Index(routings, g.Routing)
	if g.Routing == "" {
		i = 0
	}
	g.Routing = routings[(i+1)%len(routings)]
	return nil
}

func (m model) updateGroups(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	n := len(m.groups)
	key := msg.String()
	if key != "d" {
		m.confirm = ""
	}
	switch key {
	case "j", "down":
		if n > 0 {
			m.grow = (m.grow + 1) % n
		}
		return m, nil
	case "k", "up":
		if n > 0 {
			m.grow = (m.grow + n - 1) % n
		}
		return m, nil
	case "n":
		m.openAsk(ask{crumbs: []string{"groups", "new"}, input: newInput("the group's name, e.g. opus-anywhere"),
			hint: "then pick its first model; agents choose it as group/<id>",
			onEnter: func(name string) tea.Cmd {
				return func() tea.Msg { return pickMsg{newGroup: name} }
			}})
		return m, nil
	}
	if n == 0 {
		return m, nil
	}
	g := m.groups[m.grow]
	switch key {
	case "enter", " ":
		m.gid, m.gsel = g.ID, 0
		m.mode = modeGroup
	case "o":
		return m, saveGroup(g.ID, nextRouting, g.Name+" routing changed")
	case "d":
		if m.confirm != "group/"+g.ID {
			m.confirm = "group/" + g.ID
			m.flash, m.flashOK = "press d again to remove "+g.Name, false
			return m, nil
		}
		m.confirm = ""
		return m, func() tea.Msg {
			if err := provider.DeleteGroup(g.ID); err != nil {
				return flashMsg{text: err.Error()}
			}
			return flashMsg{text: "removed " + g.Name, ok: true}
		}
	}
	return m, nil
}

// pickMsg opens the model picker for a group: to add to one, or to make
// one with its first model.
type pickMsg struct{ group, newGroup string }

// memberOptions is every model and group magpie has, but those in skip.
func memberOptions(skip []string, self string) []agent.Option {
	var out []agent.Option
	for _, e := range provider.Catalog() {
		if slices.Contains(skip, e.ID) || e.ID == provider.GroupPrefix+self {
			continue
		}
		note := e.Name
		if e.Group != "" {
			note = "group · " + e.Name
		}
		out = append(out, agent.Option{Value: e.ID, Note: note})
	}
	return out
}

func (m *model) openMemberPicker(p pickMsg) {
	var skip []string
	crumbs := []string{"groups", p.newGroup, "first model"}
	empty := "magpie has no models yet · add a provider first"
	if p.group != "" {
		g, _ := findGroup(p.group)
		skip, crumbs = g.Members, []string{"groups", g.Name, "add a model"}
		empty = "every model magpie has is in it"
	}
	m.pk = picker{
		crumbs: crumbs,
		input:  newInput("filter models and groups"),
		items:  memberOptions(skip, p.group),
		empty:  empty,
		onPick: func(id string) tea.Cmd {
			if p.group != "" {
				return saveGroup(p.group, func(g *provider.Group) error {
					g.Members = append(g.Members, id)
					return nil
				}, id+" added")
			}
			return func() tea.Msg {
				g := provider.Group{Name: p.newGroup, Members: []string{id}}
				if err := provider.SaveGroup(g); err != nil {
					return flashMsg{text: err.Error()}
				}
				return flashMsg{text: "made group " + p.newGroup + " · ↵ on it adds more models", ok: true}
			}
		},
	}
	m.pk.refilter()
	m.mode = modePick
	if p.group != "" {
		m.back = modeGroup
	}
}

func (m model) group() (provider.Group, bool) {
	for _, g := range m.groups {
		if g.ID == m.gid {
			return g, true
		}
	}
	return provider.Group{}, false
}

func (m model) updateGroup(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	g, ok := m.group()
	if !ok {
		m.mode = modeList
		return m, nil
	}
	n := len(g.Members)
	key := msg.String()
	if key != "d" {
		m.confirm = ""
	}
	move := func(by int) (tea.Model, tea.Cmd) {
		to := m.gsel + by
		if to < 0 || to >= n {
			return m, nil
		}
		from := m.gsel
		m.gsel = to
		return m, saveGroup(g.ID, func(g *provider.Group) error {
			g.Members[from], g.Members[to] = g.Members[to], g.Members[from]
			return nil
		}, g.Members[from]+" moved")
	}
	switch key {
	case "esc", "q":
		m.mode = modeList
	case "j", "down":
		if n > 0 {
			m.gsel = (m.gsel + 1) % n
		}
	case "k", "up":
		if n > 0 {
			m.gsel = (m.gsel + n - 1) % n
		}
	case "J", "shift+down":
		return move(1)
	case "K", "shift+up":
		return move(-1)
	case "a":
		m.openMemberPicker(pickMsg{group: g.ID})
	case "o":
		return m, saveGroup(g.ID, nextRouting, g.Name+" routing changed")
	case "f":
		in := newInput("a tag, e.g. relay")
		in.SetValue(g.Family)
		m.openAsk(ask{crumbs: []string{"groups", g.Name, "family"}, input: in, empty: true,
			hint: "agents can be shown only some families: magpie visible <agent> <family>",
			onEnter: func(v string) tea.Cmd {
				return saveGroup(g.ID, func(g *provider.Group) error { g.Family = v; return nil }, g.Name+" family "+dash(v))
			}})
		m.back = modeGroup
	case "d":
		if n == 0 {
			return m, nil
		}
		id := g.Members[m.gsel]
		if n == 1 {
			m.flash, m.flashOK = "a group needs a model in it · d on the groups list removes the group", false
			return m, nil
		}
		if m.confirm != "member/"+id {
			m.confirm = "member/" + id
			m.flash, m.flashOK = "press d again to take "+id+" out", false
			return m, nil
		}
		m.confirm = ""
		m.gsel = clamp(m.gsel, n-1)
		return m, saveGroup(g.ID, func(g *provider.Group) error {
			g.Members = slices.DeleteFunc(g.Members, func(x string) bool { return x == id })
			var rules []provider.Rule
			for _, r := range g.Rules {
				if r.Use != id {
					rules = append(rules, r)
				}
			}
			g.Rules = rules
			return nil
		}, id+" taken out")
	}
	return m, nil
}

func (m model) viewGroups() string {
	var b strings.Builder
	b.WriteString(m.header())
	b.WriteString("\n\n")
	if len(m.groups) == 0 {
		b.WriteString(pad + "  " + sMuted.Render("no routing groups yet · n makes one"))
		return b.String()
	}
	nameW, idW := 0, 0
	for _, g := range m.groups {
		nameW, idW = max(nameW, lipgloss.Width(g.Name)), max(idW, lipgloss.Width(provider.GroupPrefix+g.ID))
	}
	visible := max(3, m.h-7)
	start := 0
	if m.grow >= visible {
		start = m.grow - visible + 1
	}
	for i := start; i < min(len(m.groups), start+visible); i++ {
		g := m.groups[i]
		marker, name := "  ", sName.Render(padRight(g.Name, nameW))
		if i == m.grow {
			marker, name = sCursor.Render("▸ "), sNameOn.Render(padRight(g.Name, nameW))
		}
		notes := []string{fmt.Sprintf("%d model%s", len(g.Members), plural(len(g.Members))), routingName(g.Routing)}
		if len(g.Rules) > 0 {
			notes = append(notes, fmt.Sprintf("%d rule%s", len(g.Rules), plural(len(g.Rules))))
		}
		if g.Family != "" {
			notes = append(notes, "family "+g.Family)
		}
		if g.Auto {
			notes = append(notes, "found")
		}
		members := strings.Join(g.Members, " → ")
		line := pad + marker + name + "  " + sFaint.Render(padRight(provider.GroupPrefix+g.ID, idW)) + "  " + sText.Render(strings.Join(notes, " · "))
		if room := m.w - lipgloss.Width(line) - 4; room > 10 {
			if r := []rune(members); len(r) > room {
				members = string(r[:room-1]) + "…"
			}
			line += "  " + sMuted.Render(members)
		}
		b.WriteString(line + "\n")
	}
	return strings.TrimRight(b.String(), "\n")
}

func (m model) viewGroup() string {
	g, _ := m.group()
	var b strings.Builder
	b.WriteString(m.header("groups", g.Name))
	b.WriteString("\n\n")
	head := provider.GroupPrefix + g.ID + " · " + routingName(g.Routing)
	if g.Family != "" {
		head += " · family " + g.Family
	}
	if g.Auto {
		head += " · found by magpie: a change makes it yours"
	}
	b.WriteString(pad + "  " + sMuted.Render(head) + "\n\n")
	for i, id := range g.Members {
		marker, name := "  ", sText.Render(id)
		if i == m.gsel {
			marker, name = sCursor.Render("▸ "), sNameOn.Render(id)
		}
		b.WriteString(pad + marker + sFaint.Render(fmt.Sprintf("%d  ", i+1)) + name + "\n")
	}
	if len(g.Rules) > 0 {
		b.WriteString("\n" + pad + "  " + sFaint.Render("rules · magpie group rule "+g.ID+" changes them") + "\n")
		for i, r := range g.Rules {
			b.WriteString(pad + "  " + sFaint.Render(fmt.Sprintf("%d  ", i+1)) + sMuted.Render(strings.Join(r.Conditions(), " · ")+" → ") + sText.Render(r.Use) + "\n")
		}
	}
	return strings.TrimRight(b.String(), "\n")
}

// ---- usage ------------------------------------------------------------------

var periods = []usage.Period{usage.Today, usage.Week, usage.Month, usage.All}

var periodNames = map[usage.Period]string{usage.Today: "today", usage.Week: "7 days", usage.Month: "30 days", usage.All: "all time"}

func (m model) updateUsage(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	i := slices.Index(periods, m.period)
	switch msg.String() {
	case "l", "right":
		m.period = periods[(i+1)%len(periods)]
	case "h", "left":
		m.period = periods[(i+len(periods)-1)%len(periods)]
	case "t":
		m.period = usage.Today
	case "w":
		m.period = usage.Week
	case "m":
		m.period = usage.Month
	case "A":
		m.period = usage.All
	default:
		return m, nil
	}
	m.sum = usage.Summarize(m.period)
	return m, nil
}

func fmtTokens(n int) string {
	switch {
	case n >= 1_000_000_000:
		return fmt.Sprintf("%.2fB", float64(n)/1e9)
	case n >= 10_000_000:
		return fmt.Sprintf("%.0fM", float64(n)/1e6)
	case n >= 1_000_000:
		return fmt.Sprintf("%.1fM", float64(n)/1e6)
	case n >= 100_000:
		return fmt.Sprintf("%.0fK", float64(n)/1e3)
	case n >= 1_000:
		return fmt.Sprintf("%.1fK", float64(n)/1e3)
	}
	return fmt.Sprint(n)
}

func fmtCost(t usage.Totals) string {
	if t.Cost == 0 && t.Unpriced > 0 {
		return "no price"
	}
	var s string
	switch {
	case t.Cost >= 100:
		s = fmt.Sprintf("$%.0f", t.Cost)
	case t.Cost >= 1:
		s = fmt.Sprintf("$%.2f", t.Cost)
	default:
		s = fmt.Sprintf("$%.3f", t.Cost)
	}
	if t.Unpriced > 0 {
		s += "+"
	}
	return "≈" + s
}

var bars = []rune(" ▁▂▃▄▅▆▇█")

func (m model) viewUsage() string {
	s := m.sum
	var b strings.Builder
	b.WriteString(m.header())
	b.WriteString("\n\n")
	var tabs []string
	for _, p := range periods {
		if p == s.Period {
			tabs = append(tabs, sPill.Render(periodNames[p]))
		} else {
			tabs = append(tabs, sMuted.Render(" "+periodNames[p]+" "))
		}
	}
	b.WriteString(pad + "  " + strings.Join(tabs, " ") + "\n\n")
	if s.Calls == 0 {
		b.WriteString(pad + "  " + sMuted.Render("no calls in this time · route an agent through magpie and its usage shows up here"))
		return b.String()
	}
	line := sName.Render(fmtTokens(s.Tokens())+" tokens") + sMuted.Render(fmt.Sprintf(" · %d call%s · ", s.Calls, plural(s.Calls))) + sOK.Render(fmtCost(s.Totals))
	if s.Errors > 0 {
		line += sMuted.Render(" · ") + sBad.Render(fmt.Sprintf("%d error%s", s.Errors, plural(s.Errors)))
	}
	b.WriteString(pad + "  " + line + "\n")
	b.WriteString(pad + "  " + sMuted.Render("in "+fmtTokens(s.Input)+"  out "+fmtTokens(s.Output)+"  cache read "+fmtTokens(s.CacheRead)+"  cache write "+fmtTokens(s.CacheWrite)+"  reasoning "+fmtTokens(s.Reasoning)) + "\n\n")

	// the timeline, one bar a bucket
	top := 0
	for _, p := range s.Series {
		top = max(top, p.Tokens())
	}
	if top > 0 && len(s.Series) > 0 {
		var sb strings.Builder
		for _, p := range s.Series {
			i := 0
			if p.Tokens() > 0 {
				i = max(1, p.Tokens()*(len(bars)-1)/top)
			}
			sb.WriteRune(bars[i])
		}
		first, last := s.Series[0].Label, s.Series[len(s.Series)-1].Label
		b.WriteString(pad + "  " + sCursor.Render(sb.String()) + "  " + sFaint.Render(first+" – "+last) + "\n\n")
	}

	names := map[string]string{}
	for _, a := range agent.All() {
		names[a.ID] = a.Name
	}
	room := max(2, (m.h-16)/2)
	table := func(head string, gs []usage.Group, name func(usage.Group) string) {
		b.WriteString(pad + "  " + sFaint.Render(head) + "\n")
		w := 0
		for _, g := range gs {
			w = max(w, lipgloss.Width(name(g)))
		}
		for i, g := range gs {
			if i == room {
				b.WriteString(pad + "  " + sFaint.Render(fmt.Sprintf("and %d more · magpie usage", len(gs)-room)) + "\n")
				break
			}
			share := fmt.Sprintf("%3.0f%%", 100*float64(g.Tokens())/float64(max(1, s.Tokens())))
			b.WriteString(pad + "  " + sText.Render(padRight(name(g), w)) + "  " + sMuted.Render(share) + "  " + sText.Render(padRight(fmtTokens(g.Tokens()), 7)) + "  " + sFaint.Render(padRight(fmt.Sprintf("%d call%s", g.Calls, plural(g.Calls)), 10)) + "  " + sOK.Render(fmtCost(g.Totals)) + "\n")
		}
		b.WriteString("\n")
	}
	table("agents", s.Agents, func(g usage.Group) string {
		if n := names[g.ID]; n != "" {
			return n
		}
		return g.ID
	})
	table("models", s.Models, func(g usage.Group) string { return g.ID })
	return strings.TrimRight(b.String(), "\n")
}

// ---- a line to type ---------------------------------------------------------

func (m *model) openAsk(a ask) {
	m.ask = a
	m.mode = modeAsk
	m.back = modeList
}

func (m model) updateAsk(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "esc":
		m.mode = m.back
		return m, nil
	case "enter":
		v := strings.TrimSpace(m.ask.input.Value())
		if v == "" && !m.ask.empty {
			return m, nil
		}
		m.mode = m.back
		return m, m.ask.onEnter(v)
	}
	var cmd tea.Cmd
	m.ask.input, cmd = m.ask.input.Update(msg)
	return m, cmd
}

func (m model) viewAsk() string {
	var b strings.Builder
	b.WriteString(m.header(m.ask.crumbs...))
	b.WriteString("\n\n")
	b.WriteString(pad + sCursor.Render("❯ ") + m.ask.input.View())
	if m.ask.hint != "" {
		b.WriteString("\n\n" + pad + "  " + sMuted.Render(m.ask.hint))
	}
	return b.String()
}
