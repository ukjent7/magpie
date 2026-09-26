package provider

import (
	"slices"
	"strings"

	"github.com/yetone/magpie/internal/settings"
)

// Which of the catalog an agent is shown. Settings' Visible names, for an
// agent, the families its lists hold: a family is the tag providers and
// groups are given (magpie provider set <id> family=relay), and a
// provider's or group's id names it alone. The gateway's model list and
// what magpie writes into the agents' files both come from CatalogFor, so
// the two never differ.

// Names are what an entry answers to in a visibility: its family, and its
// provider's id or its group's id (as group/<id> too).
func (e Entry) Names() []string {
	var out []string
	if e.Family != "" {
		out = append(out, e.Family)
	}
	if e.Group != "" {
		return append(out, e.Group, GroupPrefix+e.Group)
	}
	return append(out, e.Provider.ID)
}

// VisibleTo is what agent's lists are narrowed to, and whether they are.
func VisibleTo(agent string) ([]string, bool) {
	names, ok := settings.Load().Visible[strings.ToLower(agent)]
	return names, ok
}

// Shows reports whether a visibility shows e.
func Shows(names []string, e Entry) bool {
	for _, n := range e.Names() {
		if slices.ContainsFunc(names, func(v string) bool { return strings.EqualFold(v, n) }) {
			return true
		}
	}
	return false
}

// CatalogFor is the catalog as agent is shown it, and what is kept from it
// (none when its lists aren't narrowed).
func CatalogFor(agent string) (shown, hidden []Entry) {
	all := Catalog()
	names, ok := VisibleTo(agent)
	if !ok {
		return all, nil
	}
	for _, e := range all {
		if Shows(names, e) {
			shown = append(shown, e)
		} else {
			hidden = append(hidden, e)
		}
	}
	return shown, hidden
}

// Families are the families providers and groups are tagged with, sorted.
func Families() []string {
	var out []string
	add := func(f string) {
		if f != "" && !slices.Contains(out, f) {
			out = append(out, f)
		}
	}
	for _, p := range All() {
		add(p.Family)
	}
	for _, g := range Groups() {
		add(g.Family)
	}
	slices.Sort(out)
	return out
}
