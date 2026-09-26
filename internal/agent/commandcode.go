package agent

// Command Code (commandcode.ai) keeps its settings in ~/.commandcode/
// settings.json, the model as "provider/model", and takes providers of the
// user's own in ~/.commandcode/providers.json. magpie adds itself there as
// the provider "magpie", keyless, with the catalog as its models; a model
// through magpie is "magpie/<provider>/<model>", which Command Code splits
// at the first "/". Command Code still wants its own sign-in for these.

import (
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/yetone/magpie/internal/edit"
)

// ccEfforts are the reasoning levels Command Code knows.
var ccEfforts = []string{"low", "medium", "high", "xhigh", "max"}

func commandCode(home string) *Agent {
	dir := filepath.Join(home, ".commandcode")
	path := filepath.Join(dir, "settings.json")
	providers := filepath.Join(dir, "providers.json")
	get := func(k string) string { v, _ := edit.GetJSON(path, k); return v }
	dropMagpie := func() error {
		if _, err := os.Stat(providers); err != nil {
			return nil
		}
		return edit.DelJSON(providers, "provider."+magpieID)
	}
	return &Agent{
		ID: "commandcode", Name: "Command Code", Icon: "commandcode", Aliases: []string{"command-code", "cmd"},
		UA:  []string{"command-code", "commandcode"},
		Bin: "command-code", Dir: dir, Path: path,
		Sync: func() error {
			return syncJSON(providers, "provider."+magpieID, func() any { return ccProviderJSON() })
		},
		Notice: func() string {
			notes := []string{"Command Code wants its own sign-in (cmd login) even for models through magpie."}
			if Running(`(^|/)(cmd|cmdc|command-code|commandcode)( |$)`) {
				notes = append(notes, "It reads its settings at start-up — restart open Command Code sessions to use this.")
			}
			return strings.Join(notes, " ")
		},
		Check: func() string {
			if !usesMagpie(get("model")) {
				return ""
			}
			if p := get("modelProvider"); p != magpieID {
				return "Command Code's modelProvider (settings.json) is " + orDefault(p) + ", so it no longer asks magpie"
			}
			return wiringOff("Command Code", providers, func(k string) (string, bool) { return edit.GetJSON(providers, "provider."+magpieID+"."+k) },
				"baseURL", gatewayV1())
		},
		Fields: []Field{{
			Key: "model", Label: "model",
			Get: func() string { return get("model") },
			Set: func(v string) error {
				if v == "" {
					keys := []string{"model"}
					if get("modelProvider") == magpieID {
						keys = append(keys, "modelProvider")
					}
					if err := edit.DelJSON(path, keys...); err != nil {
						return err
					}
					return dropMagpie()
				}
				if ref, ok := strings.CutPrefix(v, magpieID+"/"); ok && isMagpie(ref) {
					if err := edit.SetJSON(providers, edit.KV{Path: "provider." + magpieID, Value: ccProviderJSON()}); err != nil {
						return err
					}
					return edit.SetJSON(path, edit.KV{Path: "model", Value: v}, edit.KV{Path: "modelProvider", Value: magpieID})
				}
				if get("modelProvider") == magpieID {
					if err := edit.DelJSON(path, "modelProvider"); err != nil {
						return err
					}
				}
				if err := edit.SetJSON(path, edit.KV{Path: "model", Value: v}); err != nil {
					return err
				}
				return dropMagpie()
			},
			Options: func(cur map[string]string) []Option {
				var out []Option
				if v := cur["model"]; v != "" && !usesMagpie(v) {
					out = append(out, Option{Value: v, Icon: modelIcon("", v)})
				}
				return append(out, viaMagpie("commandcode", magpieID+"/")...)
			},
		}},
	}
}

// ccProviderJSON is magpie's entry in providers.json. The key is false:
// the gateway takes any, and Command Code refuses one written out.
func ccProviderJSON() any {
	ms := map[string]any{}
	for _, m := range magpieModels("commandcode") {
		e := map[string]any{"name": m.Name}
		var efforts []string
		for _, x := range m.Efforts {
			if slices.Contains(ccEfforts, x) {
				efforts = append(efforts, x)
			}
		}
		if len(efforts) > 0 {
			e["reasoning"] = true
			e["reasoningEfforts"] = efforts
		}
		ms[m.ID] = e
	}
	return map[string]any{"name": "magpie", "api": "openai-completions", "baseURL": gatewayV1(), "apiKey": false, "models": ms}
}
