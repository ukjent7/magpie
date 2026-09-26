package agent

// omp (oh-my-pi, a fork of Pi) keeps its settings in ~/.omp/agent/config.yml,
// the model of each role under modelRoles as "provider/model", and providers
// of the user's own in models.yml beside it. magpie adds itself there as the
// provider "magpie", keyless (auth: none), with the catalog as its models; a
// model through magpie is "magpie/<provider>/<model>", which omp matches
// whole against provider/id.

import (
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/yetone/magpie/internal/edit"
)

// ompEfforts are the thinking levels omp knows.
var ompEfforts = []string{"minimal", "low", "medium", "high", "xhigh", "max"}

func omp(home string) *Agent {
	dir := filepath.Join(home, ".omp", "agent")
	// omp reads the .yml and falls back to the .yaml
	pick := func(name string) string {
		yml := filepath.Join(dir, name+".yml")
		if _, err := os.Stat(yml); err != nil {
			if _, err := os.Stat(filepath.Join(dir, name+".yaml")); err == nil {
				return filepath.Join(dir, name+".yaml")
			}
		}
		return yml
	}
	path := pick("config")
	get := func() string { v, _ := edit.GetYAML(path, "modelRoles.default"); return v }
	dropMagpie := func() error {
		// another role may still go through magpie
		for _, v := range edit.GetYAMLMap(path, "modelRoles") {
			if usesMagpie(v) {
				return nil
			}
		}
		return edit.DelYAML(pick("models"), "providers."+magpieID)
	}
	writeMagpie := func() error {
		models := pick("models")
		if _, err := os.Stat(models); err != nil {
			// omp moves an older models.json to models.yml only while there is
			// no models.yml, so one magpie writes first has to carry it over
			if err := edit.JSONToYAML(filepath.Join(dir, "models.json"), models); err != nil {
				return err
			}
		}
		return edit.SetYAML(models, edit.KV{Path: "providers." + magpieID, Value: ompProvider()})
	}
	return &Agent{
		ID: "omp", Name: "omp", Icon: "omp", Aliases: []string{"oh-my-pi"},
		UA:  []string{"oh-my-pi"},
		Bin: "omp", Dir: dir, Path: path,
		Sync: func() error {
			return syncYAML(pick("models"), "providers."+magpieID, func() any { return ompProvider() })
		},
		Notice: func() string {
			if Running(`(^|/)omp( |$)`, `@oh-my-pi/pi-coding-agent`) {
				return "omp reads its settings at start-up — restart open omp sessions to use this."
			}
			return ""
		},
		Check: func() string {
			if !usesMagpie(get()) {
				return ""
			}
			models := pick("models")
			return wiringOff("omp", models, func(k string) (string, bool) { return edit.GetYAML(models, "providers."+magpieID+"."+k) },
				"baseUrl", gatewayV1())
		},
		Fields: []Field{{
			Key: "model", Label: "model",
			Get: get,
			Set: func(v string) error {
				if v == "" {
					if err := edit.DelYAML(path, "modelRoles.default"); err != nil {
						return err
					}
					return dropMagpie()
				}
				if ref, ok := strings.CutPrefix(v, magpieID+"/"); ok && isMagpie(ref) {
					if err := writeMagpie(); err != nil {
						return err
					}
					return edit.SetYAML(path, edit.KV{Path: "modelRoles.default", Value: v})
				}
				if err := edit.SetYAML(path, edit.KV{Path: "modelRoles.default", Value: v}); err != nil {
					return err
				}
				return dropMagpie()
			},
			Options: func(cur map[string]string) []Option {
				return append(ownOptions("", cur["model"]), viaMagpie("omp", magpieID+"/")...)
			},
		}},
	}
}

type ompModel struct {
	ID        string       `yaml:"id"`
	Name      string       `yaml:"name,omitempty"`
	Reasoning bool         `yaml:"reasoning"`
	Thinking  *ompThinking `yaml:"thinking,omitempty"`
	Context   int          `yaml:"contextWindow,omitempty"`
}

type ompThinking struct {
	Mode    string   `yaml:"mode"`
	Efforts []string `yaml:"efforts"`
}

type ompProviderEntry struct {
	BaseURL string     `yaml:"baseUrl"`
	API     string     `yaml:"api"`
	Auth    string     `yaml:"auth"`
	Models  []ompModel `yaml:"models"`
}

// ompProvider is magpie's entry in models.yml. The thinking efforts are the
// levels omp offers for the model; it sends them as reasoning_effort.
func ompProvider() ompProviderEntry {
	ms := []ompModel{}
	for _, m := range magpieModels("omp") {
		e := ompModel{ID: m.ID, Name: m.Name, Context: m.Context}
		var efforts []string
		for _, x := range ompEfforts { // in omp's order
			if slices.Contains(m.Efforts, x) {
				efforts = append(efforts, x)
			}
		}
		if len(efforts) > 0 {
			e.Reasoning = true
			e.Thinking = &ompThinking{Mode: "effort", Efforts: efforts}
		}
		ms = append(ms, e)
	}
	return ompProviderEntry{BaseURL: gatewayV1(), API: "openai-completions", Auth: "none", Models: ms}
}
