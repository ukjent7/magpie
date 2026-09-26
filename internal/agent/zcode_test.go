package agent

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/yetone/magpie/internal/provider"
)

func TestZCode(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(home, ".config"))
	t.Setenv("XDG_CACHE_HOME", filepath.Join(home, ".cache"))
	if err := provider.Save(provider.Provider{ID: "deepseek", Name: "DeepSeek", Chat: "https://api.deepseek.com/v1", Key: "k", Models: []string{"pro"}}); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(home, ".zcode", "v2", "config.json")
	os.MkdirAll(filepath.Dir(path), 0o755)
	os.WriteFile(path, []byte(`{"provider":{"builtin:bigmodel":{"name":"Bigmodel","kind":"anthropic","enabled":true}}}`), 0o644)
	read := func() map[string]any {
		var c struct {
			Provider map[string]map[string]any `json:"provider"`
		}
		b, _ := os.ReadFile(path)
		if err := json.Unmarshal(b, &c); err != nil {
			t.Fatalf("%v\n%s", err, b)
		}
		if c.Provider["builtin:bigmodel"] == nil {
			t.Fatalf("ZCode's own provider went: %s", b)
		}
		return c.Provider["magpie"]
	}

	a := zcode(home)
	if !a.Detected() {
		t.Fatal("not detected")
	}
	f := a.Field("provider")
	if f.Get() != "" {
		t.Fatalf("get: %q", f.Get())
	}
	if err := f.Set("magpie"); err != nil {
		t.Fatal(err)
	}
	m := read()
	opts, _ := m["options"].(map[string]any)
	models, _ := m["models"].(map[string]any)
	pro, _ := models["deepseek/pro"].(map[string]any)
	if m["kind"] != "anthropic" || m["enabled"] != true || m["source"] != "custom" || opts["apiKey"] != "magpie" ||
		opts["baseURL"] == "" || pro == nil || pro["limit"] == nil || pro["modalities"] == nil {
		t.Fatalf("magpie provider: %v", m)
	}
	if f.Get() != "magpie" {
		t.Fatalf("get: %q", f.Get())
	}
	// ZCode 3.14 reads provider_config.json
	rules := filepath.Join(home, ".zcode", "v2", "provider_config.json")
	os.WriteFile(rules, []byte(`{"schemaVersion":1,"config":{"providerConfigRules":{"providerRules":[{"providerId":"mine","config":{}}]},"modelConfigRules":{"providerModelRules":[],"manualProviderModelRules":[{"providerId":"magpie","modelId":"deepseek/pro","config":{"enabled":true}}]}},"other":1}`), 0o600)
	if err := f.Set("magpie"); err != nil {
		t.Fatal(err)
	}
	type rulesDoc struct {
		Other  int `json:"other"`
		Config struct {
			ProviderConfigRules struct {
				ProviderRules []map[string]any `json:"providerRules"`
			} `json:"providerConfigRules"`
			ModelConfigRules struct {
				ProviderModelRules       []map[string]any `json:"providerModelRules"`
				ManualProviderModelRules []map[string]any `json:"manualProviderModelRules"`
			} `json:"modelConfigRules"`
		} `json:"config"`
	}
	readRules := func() rulesDoc {
		var d rulesDoc
		b, _ := os.ReadFile(rules)
		if err := json.Unmarshal(b, &d); err != nil {
			t.Fatalf("%v\n%s", err, b)
		}
		if d.Other != 1 || len(d.Config.ProviderConfigRules.ProviderRules) == 0 || d.Config.ProviderConfigRules.ProviderRules[0]["providerId"] != "mine" {
			t.Fatalf("ZCode's own rules went: %s", b)
		}
		return d
	}
	d := readRules()
	pr := d.Config.ProviderConfigRules.ProviderRules
	if len(pr) != 2 {
		t.Fatalf("provider rules: %v", pr)
	}
	cfg, _ := pr[1]["config"].(map[string]any)
	access, _ := cfg["access"].(map[string]any)
	api, _ := cfg["api"].(map[string]any)
	if pr[1]["providerId"] != "magpie" || pr[1]["enabled"] != true || cfg["group"] != "standard-personal" ||
		access["type"] != "api-key" || access["apiKey"] != "magpie" || api["type"] != "anthropic-messages" || api["baseUrl"] == "" ||
		len(cfg["personalModelIds"].([]any)) != 1 {
		t.Fatalf("magpie rule: %v", pr[1])
	}
	// a model set by hand in ZCode keeps its manual rule, and gets no second one
	if len(d.Config.ModelConfigRules.ManualProviderModelRules) != 1 || len(d.Config.ModelConfigRules.ProviderModelRules) != 0 {
		t.Fatalf("model rules: %+v", d.Config.ModelConfigRules)
	}

	// a provider added later reaches ZCode's picker
	if err := provider.Save(provider.Provider{ID: "kimi", Name: "Kimi", Chat: "https://api.moonshot.cn/v1", Key: "k", Models: []string{"k2"}}); err != nil {
		t.Fatal(err)
	}
	if err := a.Sync(); err != nil {
		t.Fatal(err)
	}
	if models, _ := read()["models"].(map[string]any); models["kimi/k2"] == nil {
		t.Fatalf("not synced: %v", models)
	}
	if d := readRules(); len(d.Config.ProviderConfigRules.ProviderRules[1]["config"].(map[string]any)["personalModelIds"].([]any)) != 2 ||
		len(d.Config.ModelConfigRules.ProviderModelRules) != 1 || d.Config.ModelConfigRules.ProviderModelRules[0]["modelId"] != "kimi/k2" {
		t.Fatalf("rules not synced: %+v", d.Config)
	}

	if err := f.Set(""); err != nil {
		t.Fatal(err)
	}
	if read() != nil || f.Get() != "" {
		t.Fatal("magpie provider left behind")
	}
	if d := readRules(); len(d.Config.ProviderConfigRules.ProviderRules) != 1 || len(d.Config.ModelConfigRules.ManualProviderModelRules) != 0 {
		t.Fatalf("magpie rules left behind: %+v", d.Config)
	}
	// nothing to sync into a config that has no magpie provider
	if err := a.Sync(); err != nil || read() != nil {
		t.Fatal("sync added the provider")
	}
}

// What the user did in ZCode stays: magpie turned off stays off, in both
// files; its rule stays where it is among theirs; and a rule they removed
// isn't put back when magpie syncs.
func TestZCodeKeepsUserChoices(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(home, ".config"))
	t.Setenv("XDG_CACHE_HOME", filepath.Join(home, ".cache"))
	if err := provider.Save(provider.Provider{ID: "deepseek", Name: "DeepSeek", Chat: "https://api.deepseek.com/v1", Key: "k", Models: []string{"pro"}}); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(home, ".zcode", "v2", "config.json")
	rules := filepath.Join(home, ".zcode", "v2", "provider_config.json")
	os.MkdirAll(filepath.Dir(path), 0o755)
	os.WriteFile(path, []byte(`{"provider":{"magpie":{"name":"magpie","kind":"anthropic","enabled":false}}}`), 0o644)
	os.WriteFile(rules, []byte(`{"schemaVersion":1,"config":{"providerConfigRules":{"providerRules":[
	  {"providerId":"a","config":{}},{"providerId":"magpie","enabled":false,"config":{}},{"providerId":"b","config":{}}]}}}`), 0o600)
	a := zcode(home)
	if err := a.Sync(); err != nil {
		t.Fatal(err)
	}
	ids := func() (out []string, on []any) {
		var d struct {
			Config struct {
				ProviderConfigRules struct {
					ProviderRules []map[string]any `json:"providerRules"`
				} `json:"providerConfigRules"`
			} `json:"config"`
		}
		b, _ := os.ReadFile(rules)
		json.Unmarshal(b, &d)
		for _, r := range d.Config.ProviderConfigRules.ProviderRules {
			out = append(out, r["providerId"].(string))
			on = append(on, r["enabled"])
		}
		return out, on
	}
	if got, on := ids(); len(got) != 3 || got[1] != "magpie" || on[1] != false {
		t.Fatalf("rules %v %v", got, on)
	}
	var c struct {
		Provider map[string]map[string]any `json:"provider"`
	}
	b, _ := os.ReadFile(path)
	json.Unmarshal(b, &c)
	if c.Provider["magpie"]["enabled"] != false || c.Provider["magpie"]["models"] == nil {
		t.Fatalf("config.json: %s", b)
	}

	// removed in ZCode: not put back
	os.WriteFile(rules, []byte(`{"schemaVersion":1,"config":{"providerConfigRules":{"providerRules":[{"providerId":"a","config":{}}]}}}`), 0o600)
	if err := a.Sync(); err != nil {
		t.Fatal(err)
	}
	if got, _ := ids(); len(got) != 1 {
		t.Fatalf("magpie put back: %v", got)
	}
}
