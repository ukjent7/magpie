package gui

import (
	"context"
	"net/http"
	"time"

	"github.com/yetone/magpie/internal/agent"
	"github.com/yetone/magpie/internal/provider"
	"github.com/yetone/magpie/internal/usage"
)

// usageGroup is a usage.Group with what the UI needs to draw it.
type usageGroup struct {
	usage.Group
	Name string `json:"name"`
	Sub  string `json:"sub,omitempty"`  // models: the provider's name
	Icon string `json:"icon,omitempty"` // a real logo, or "generic" for an unknown client
}

type usageJSON struct {
	usage.Summary
	Agents []usageGroup `json:"agents"`
	Models []usageGroup `json:"models"`
	Path   string       `json:"path"`
}

func usageState(p usage.Period) usageJSON {
	s := usage.Summarize(p)
	out := usageJSON{Summary: s, Agents: []usageGroup{}, Models: []usageGroup{}, Path: tilde(usage.Path())}
	agents := map[string]*agent.Agent{}
	for _, a := range agent.Clients() {
		agents[a.ID] = a
	}
	for _, g := range s.Agents {
		ug := usageGroup{Group: g, Name: g.ID, Icon: "generic"}
		if a := agents[g.ID]; a != nil {
			ug.Name, ug.Icon = a.Name, a.Icon
		}
		out.Agents = append(out.Agents, ug)
	}
	providers := map[string]provider.Provider{}
	for _, p := range provider.All() {
		providers[p.ID] = p
	}
	for _, g := range s.Models {
		ug := usageGroup{Group: g, Name: g.Model, Sub: g.Provider, Icon: "generic"}
		if p, ok := providers[g.Provider]; ok {
			ug.Sub = p.Name
			if p.Icon != "" {
				ug.Icon = p.Icon
			}
		}
		if g.Host != "" {
			ug.Sub += " · " + g.Host
		}
		out.Models = append(out.Models, ug)
	}
	return out
}

func usageRoutes(mux *http.ServeMux) {
	mux.HandleFunc("GET /api/usage", func(rw http.ResponseWriter, r *http.Request) {
		p := usage.Period(r.URL.Query().Get("period"))
		switch p {
		case usage.Today, usage.Week, usage.Month, usage.All:
		default:
			p = usage.Month
		}
		writeJSON(rw, usageState(p))
	})
	// The subscriptions' quotas, the plans' bought with a key, and the
	// keys' balances come from the
	// vendors, which can be slow or unreachable, so the page asks for them
	// apart from the local log.
	mux.HandleFunc("GET /api/usage/quotas", func(rw http.ResponseWriter, r *http.Request) {
		ctx, cancel := context.WithTimeout(r.Context(), 12*time.Second)
		defer cancel()
		writeJSON(rw, provider.Quotas(ctx))
	})
}
