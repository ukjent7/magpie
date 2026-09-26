package agent

// Alma (a desktop AI chat app) keeps its providers and settings in the app,
// not in a file magpie can edit: they are reached through its local REST API
// (http://localhost:23001, while Alma runs).
//
//	GET  /api/providers              [{"id","name","type","baseURL","enabled","models":["<id>",…],"availableModels":[{"id","name",…}]},…]
//	POST /api/providers              {"name","type","apiKey","baseURL","enabled"} → the provider
//	PUT  /api/providers/:id          the fields to change
//	PUT  /api/providers/:id/models   {"models":["<id>",…],"availableModels":[{"id","name"}]}: models, kept as
//	                                 given, are the ones Alma offers; its capabilities it works out itself
//	GET  /api/settings, PUT it back  the whole settings; chat.defaultModel is "<providerId>:<model>"
//
// magpie is one provider there, named magpie, of type openai at the
// gateway's /v1; choosing one of its models makes it Alma's default model.
// Alma not running is no error: there is nothing to read or keep current.

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/yetone/magpie/internal/gateway"
)

// almaAPI is where Alma's API answers. Under test it is nothing, so no test
// reaches a real Alma; a test points it at a fake one.
var almaAPI = func() string {
	if testing.Testing() {
		return ""
	}
	return "http://localhost:23001"
}()

// almaClient gives up quickly: a local app that doesn't answer at once isn't
// running, or is stuck.
var almaClient = &http.Client{Timeout: 1500 * time.Millisecond}

// errAlmaDown is Alma not answering.
var errAlmaDown = errors.New("Alma isn't running — open Alma and try again")

// almaProvider is what magpie reads of one of Alma's providers.
type almaProvider struct {
	ID      string           `json:"id"`
	Name    string           `json:"name"`
	Type    string           `json:"type"`
	BaseURL string           `json:"baseURL"`
	Enabled bool             `json:"enabled"`
	Models  []string         `json:"models"`
	Known   []map[string]any `json:"availableModels"`
}

// almaDo sends one request to Alma's API and decodes its reply into out.
func almaDo(method, path string, body, out any) error {
	if almaAPI == "" {
		return errAlmaDown
	}
	var rd io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		rd = bytes.NewReader(b)
	}
	req, err := http.NewRequest(method, strings.TrimRight(almaAPI, "/")+path, rd)
	if err != nil {
		return err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := almaClient.Do(req)
	if err != nil {
		return errAlmaDown
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(resp.Body, 16<<20))
	if resp.StatusCode/100 != 2 {
		msg := strings.TrimSpace(string(b))
		var e struct {
			Error string `json:"error"`
		}
		if json.Unmarshal(b, &e) == nil && e.Error != "" {
			msg = e.Error
		}
		return fmt.Errorf("Alma: %s %s: %d %s", method, path, resp.StatusCode, msg)
	}
	if out == nil || len(bytes.TrimSpace(b)) == 0 {
		return nil
	}
	return json.Unmarshal(b, out)
}

// almaProviders lists Alma's providers.
func almaProviders() ([]almaProvider, error) {
	var ps []almaProvider
	err := almaDo("GET", "/api/providers", nil, &ps)
	return ps, err
}

// almaMagpie is magpie's provider among Alma's: the one named magpie, or
// an OpenAI-shaped one at the gateway. Nil if there is none.
func almaMagpie(ps []almaProvider) *almaProvider {
	for i := range ps {
		if ps[i].Name == magpieID && (ps[i].Type == "openai" || ps[i].Type == "custom") {
			return &ps[i]
		}
	}
	for i := range ps {
		if (ps[i].Type == "openai" || ps[i].Type == "custom") && ps[i].BaseURL != "" && sameHost(ps[i].BaseURL, gatewayV1()) {
			return &ps[i]
		}
	}
	return nil
}

// almaModels is magpie's catalog as Alma is shown it: the ids Alma offers,
// and each one's name.
func almaModels() (ids []string, known []map[string]any) {
	ids, known = []string{}, []map[string]any{}
	for _, m := range magpieModels("alma") {
		ids = append(ids, m.ID)
		known = append(known, map[string]any{"id": m.ID, "name": m.Name})
	}
	return ids, known
}

// almaSameModels reports whether Alma already offers these models, in this
// order, each under its name.
func almaSameModels(p *almaProvider, ids []string, known []map[string]any) bool {
	if !slices.Equal(p.Models, ids) || len(p.Known) != len(known) {
		return false
	}
	for i, k := range known {
		if p.Known[i]["id"] != k["id"] || p.Known[i]["name"] != k["name"] {
			return false
		}
	}
	return true
}

// almaSyncModels puts magpie's catalog into its provider in Alma, if it
// isn't there already.
func almaSyncModels(p *almaProvider) error {
	ids, known := almaModels()
	if almaSameModels(p, ids, known) {
		return nil
	}
	return almaDo("PUT", "/api/providers/"+p.ID+"/models", map[string]any{"models": ids, "availableModels": known}, nil)
}

// almaWire makes sure Alma has magpie's provider, pointed at the gateway,
// turned on and listing the catalog, and returns its id.
func almaWire() (string, error) {
	ps, err := almaProviders()
	if err != nil {
		return "", err
	}
	p := almaMagpie(ps)
	if p == nil {
		var made almaProvider
		if err := almaDo("POST", "/api/providers", map[string]any{"name": magpieID, "type": "openai",
			"apiKey": gateway.Token, "baseURL": gatewayV1(), "enabled": true}, &made); err != nil {
			return "", err
		}
		if made.ID == "" {
			// a reply without the provider: look it up
			if ps, err = almaProviders(); err != nil {
				return "", err
			}
			if p = almaMagpie(ps); p == nil {
				return "", errors.New("Alma didn't keep magpie's provider")
			}
			made = *p
		}
		p = &made
	} else if p.BaseURL != gatewayV1() || !p.Enabled {
		if err := almaDo("PUT", "/api/providers/"+p.ID, map[string]any{"baseURL": gatewayV1(),
			"apiKey": gateway.Token, "enabled": true}, nil); err != nil {
			return "", err
		}
	}
	return p.ID, almaSyncModels(p)
}

// almaSettings reads Alma's whole settings, every key kept as it is.
func almaSettings() (map[string]any, error) {
	var s map[string]any
	if err := almaDo("GET", "/api/settings", nil, &s); err != nil {
		return nil, err
	}
	if s == nil {
		s = map[string]any{}
	}
	return s, nil
}

func almaDefault(s map[string]any) string {
	chat, _ := s["chat"].(map[string]any)
	v, _ := chat["defaultModel"].(string)
	return v
}

// almaSetDefault sets chat.defaultModel, putting the rest of the settings
// back as they were: Alma takes only the whole object.
func almaSetDefault(v string) error {
	s, err := almaSettings()
	if err != nil {
		return err
	}
	if almaDefault(s) == v {
		return nil
	}
	chat, ok := s["chat"].(map[string]any)
	if !ok {
		chat = map[string]any{}
		s["chat"] = chat
	}
	chat["defaultModel"] = v
	return almaDo("PUT", "/api/settings", s, nil)
}

// almaGet is Alma's default model, one of magpie's as magpie/<model>. With
// Alma not running it is what magpie last set there, so that isn't taken
// for something else having changed it.
func almaGet() string {
	s, err := almaSettings()
	if err != nil {
		return appliedOf("alma").Fields["model"]
	}
	v := almaDefault(s)
	pid, model, ok := strings.Cut(v, ":")
	if !ok {
		return v
	}
	ps, err := almaProviders()
	if err != nil {
		return appliedOf("alma").Fields["model"]
	}
	if p := almaMagpie(ps); p != nil && p.ID == pid {
		return magpieID + "/" + model
	}
	return v
}

func almaSet(v string) error {
	if v == "" {
		// back to Alma's own: its default model is its to pick, and
		// magpie's provider comes out
		s, err := almaSettings()
		if err != nil {
			return err
		}
		ps, err := almaProviders()
		if err != nil {
			return err
		}
		p := almaMagpie(ps)
		if p == nil {
			return nil
		}
		if pid, _, _ := strings.Cut(almaDefault(s), ":"); pid == p.ID {
			if err := almaSetDefault(""); err != nil {
				return err
			}
		}
		return almaDo("DELETE", "/api/providers/"+p.ID, nil, nil)
	}
	if ref, ok := strings.CutPrefix(v, magpieID+"/"); ok {
		id, err := almaWire()
		if err != nil {
			return err
		}
		return almaSetDefault(id + ":" + ref)
	}
	if _, _, ok := strings.Cut(v, ":"); !ok {
		return fmt.Errorf("expected providerId:model or magpie/<model>, got %q", v)
	}
	return almaSetDefault(v)
}

// almaOwn lists the models Alma reaches through its other providers.
func almaOwn() []Option {
	var ms []struct {
		ID         string `json:"id"`
		Name       string `json:"name"`
		Provider   string `json:"provider"`
		ProviderID string `json:"providerId"`
	}
	if almaDo("GET", "/api/models", nil, &ms) != nil {
		return nil
	}
	ps, _ := almaProviders()
	mine := ""
	if p := almaMagpie(ps); p != nil {
		mine = p.ID
	}
	var out []Option
	for _, m := range ms {
		pid, model, _ := strings.Cut(m.ID, ":")
		if m.ProviderID != "" {
			pid = m.ProviderID
		}
		if pid == mine {
			continue
		}
		out = append(out, Option{Value: m.ID, Label: m.Name, Group: m.Provider, Icon: modelIcon("", model)})
	}
	return out
}

func alma() *Agent {
	// where Alma keeps its data (~/Library/Application Support/alma on a
	// Mac): there once Alma was installed and opened
	dir := ""
	if d, err := os.UserConfigDir(); err == nil {
		dir = filepath.Join(d, "alma")
	}
	return &Agent{
		ID: "alma", Name: "Alma", Icon: "alma",
		UA:  []string{"alma"},
		Dir: dir, Path: almaAPI,
		Check: func() string {
			s, err := almaSettings()
			if err != nil {
				return ""
			}
			ps, err := almaProviders()
			if err != nil {
				return ""
			}
			p := almaMagpie(ps)
			if p == nil {
				return ""
			}
			if pid, _, _ := strings.Cut(almaDefault(s), ":"); pid != p.ID {
				return ""
			}
			if !p.Enabled {
				return "Alma's magpie provider is turned off, so Alma won't use its models"
			}
			return wiringOff("Alma", "providers", func(k string) (string, bool) { return p.BaseURL, p.BaseURL != "" },
				"baseURL", gatewayV1())
		},
		Sync: func() error {
			if _, err := os.Stat(dir); almaAPI == "" || dir == "" || err != nil {
				return nil
			}
			ps, err := almaProviders()
			if err != nil {
				return nil // not running: nothing to keep current
			}
			p := almaMagpie(ps)
			if p == nil {
				return nil
			}
			return almaSyncModels(p)
		},
		Fields: []Field{{
			Key: "model", Label: "model",
			Get: almaGet,
			Set: almaSet,
			Options: func(map[string]string) []Option {
				return append(almaOwn(), viaMagpie("alma", magpieID+"/")...)
			},
		}},
	}
}
