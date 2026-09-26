package agent

import (
	"fmt"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"github.com/yetone/magpie/internal/catalog"
	"github.com/yetone/magpie/internal/edit"
	"github.com/yetone/magpie/internal/gateway"
)

// Claude Code reads its endpoint from the `env` block of settings.json.
// Pointing ANTHROPIC_BASE_URL at the gateway and naming a catalog model in
// ANTHROPIC_MODEL (and the aliases opus/sonnet/haiku resolve through) is
// all it takes to run it on any provider.

// env vars magpie sets while routing through the gateway.
var claudeEnv = []string{
	"ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_MODEL",
	"ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL",
	"ANTHROPIC_DEFAULT_HAIKU_MODEL", "ANTHROPIC_DEFAULT_FABLE_MODEL",
	"ANTHROPIC_SMALL_FAST_MODEL", "CLAUDE_CODE_SUBAGENT_MODEL",
}

// claudeTiers are the aliases Claude Code resolves (/model opus, a
// subagent's "model: haiku", …), each of which can have a model of its own.
// A tier that has none follows the main model.
var claudeTiers = []string{"opus", "sonnet", "haiku", "fable"}

func tierEnv(tier string) string { return "ANTHROPIC_DEFAULT_" + strings.ToUpper(tier) + "_MODEL" }

func claude(home string) *Agent {
	path := filepath.Join(home, ".claude", "settings.json")
	env := func(k string) string { v, _ := edit.GetJSON(path, "env."+k); return v }
	model := jsonGet(path, "model")
	routed := func() bool { return env("ANTHROPIC_BASE_URL") == gateway.URL() }

	// the value shown: the catalog ref while routed, else Claude's own model.
	get := func() string {
		if routed() {
			if m := env("ANTHROPIC_MODEL"); m != "" {
				return m
			}
		}
		return model()
	}
	var writeTiers func(main string, tiers map[string]string) error
	set := func(v string) error {
		if v == "" {
			// Claude Code as installed: Anthropic's own endpoint and model
			keys := []string{"model"}
			for _, k := range claudeEnv {
				keys = append(keys, "env."+k)
			}
			forget("claude.model", "claude.base_url", "claude.auth_token")
			return edit.DelJSON(path, keys...)
		}
		if isMagpie(v) {
			if !routed() {
				stash(map[string]string{
					"claude.model":      model(),
					"claude.base_url":   env("ANTHROPIC_BASE_URL"),
					"claude.auth_token": env("ANTHROPIC_AUTH_TOKEN"),
				})
			}
			// tiers that followed the old model follow the new one; the
			// ones given a model of their own keep it
			tiers := map[string]string{}
			for _, t := range claudeTiers {
				tiers[t] = v
				if w := env(tierEnv(t)); routed() && w != "" && w != env("ANTHROPIC_MODEL") && isMagpie(w) {
					tiers[t] = w
				}
			}
			return writeTiers(v, tiers)
		}
		if routed() {
			keys := make([]string, len(claudeEnv))
			for i, k := range claudeEnv {
				keys[i] = "env." + k
			}
			if err := edit.DelJSON(path, keys...); err != nil {
				return err
			}
			unstash("claude.model")
			var back []edit.KV
			if u := unstash("claude.base_url"); u != "" {
				back = append(back, edit.KV{Path: "env.ANTHROPIC_BASE_URL", Value: u})
			}
			if t := unstash("claude.auth_token"); t != "" {
				back = append(back, edit.KV{Path: "env.ANTHROPIC_AUTH_TOKEN", Value: t})
			}
			if len(back) > 0 {
				if err := edit.SetJSON(path, back...); err != nil {
					return err
				}
			}
		}
		return edit.SetJSON(path, edit.KV{Path: "model", Value: v})
	}

	// writeTiers routes Claude Code through the gateway with main as its
	// model and each tier on the model given.
	writeTiers = func(main string, tiers map[string]string) error {
		kvs := []edit.KV{
			{Path: "env.ANTHROPIC_BASE_URL", Value: gateway.URL()},
			{Path: "env.ANTHROPIC_AUTH_TOKEN", Value: gateway.Token},
			{Path: "env.ANTHROPIC_MODEL", Value: main},
			{Path: "env.ANTHROPIC_SMALL_FAST_MODEL", Value: tiers["haiku"]},
			{Path: "model", Value: main},
		}
		same := true
		for _, t := range claudeTiers {
			kvs = append(kvs, edit.KV{Path: "env." + tierEnv(t), Value: tiers[t]})
			same = same && tiers[t] == main
		}
		if !same {
			// one model for every subagent would override the tiers they ask for
			if err := edit.DelJSON(path, "env.CLAUDE_CODE_SUBAGENT_MODEL"); err != nil {
				return err
			}
		} else {
			kvs = append(kvs, edit.KV{Path: "env.CLAUDE_CODE_SUBAGENT_MODEL", Value: main})
		}
		return edit.SetJSON(path, kvs...)
	}

	fields := []Field{{
		Key: "model", Label: "model",
		Get: get,
		Set: set,
		Options: func(map[string]string) []Option {
			var own []Option
			for _, m := range catalog.Provider("anthropic") {
				if strings.HasPrefix(m.ID, "claude") {
					own = append(own, Option{Value: m.ID, Note: m.Name, Icon: "claude-color"})
				}
			}
			name := "Claude Code"
			if u := env("ANTHROPIC_BASE_URL"); u != "" && !routed() {
				name += " · " + hostOf(u)
			}
			// Only the catalog's models; Claude Code's own short aliases are
			// not something any API lists, and a compiled-in copy would just
			// go stale.
			return append(group(name, own), claudeViaMagpie()...)
		},
	}}
	for _, tier := range claudeTiers {
		fields = append(fields, Field{
			Key: tier, Label: tier, Quiet: true,
			// empty while the tier follows the main model
			Get: func() string {
				if w := env(tierEnv(tier)); routed() && w != env("ANTHROPIC_MODEL") {
					return w
				}
				return ""
			},
			Set: func(v string) error {
				if !routed() {
					if v == "" {
						return nil
					}
					return fmt.Errorf("pick a model through magpie for Claude Code first; %s can then have its own", tier)
				}
				if v != "" && !isMagpie(v) {
					return fmt.Errorf("%s: %q is not a model magpie serves", tier, v)
				}
				main := env("ANTHROPIC_MODEL")
				tiers := map[string]string{}
				for _, t := range claudeTiers {
					tiers[t] = env(tierEnv(t))
					if tiers[t] == "" {
						tiers[t] = main
					}
				}
				tiers[tier] = v
				if v == "" {
					tiers[tier] = main
				}
				return writeTiers(main, tiers)
			},
			Options: func(map[string]string) []Option {
				if !routed() {
					return nil
				}
				return claudeViaMagpie()
			},
		})
	}

	return &Agent{
		ID: "claude", Name: "Claude Code", Icon: "claudecode-color", Aliases: []string{"cc", "claude-code"},
		UA:  []string{"claude-cli", "claude-code"},
		Bin: "claude", Dir: filepath.Dir(path), Path: path,
		Fields: fields,
		Check: func() string {
			if !isMagpie(get()) {
				return ""
			}
			// an administrator's settings win over the user's
			if u, _ := edit.GetJSON(claudeManaged(), "env.ANTHROPIC_BASE_URL"); u != "" && u != gateway.URL() {
				return "Claude Code's managed settings (" + claudeManaged() + ") set ANTHROPIC_BASE_URL to " + u + ", which wins over magpie's"
			}
			return wiringOff("Claude Code", path, func(k string) (string, bool) { return edit.GetJSON(path, "env."+k) },
				"ANTHROPIC_BASE_URL", gateway.URL(), "ANTHROPIC_AUTH_TOKEN", gateway.Token)
		},
		// every prompt typed into Claude Code goes into history.jsonl
		LastUsed: func() time.Time {
			return lastJSONLTime(filepath.Join(filepath.Dir(path), "history.jsonl"), "timestamp", "display")
		},
	}
}

// claudeViaMagpie is what magpie serves Claude Code, a model with a window
// of 1M or more marked [1m]: Claude Code takes any other for 200K, and
// compacts long before a 1M model needs it. It drops the mark before asking.
func claudeViaMagpie() []Option {
	big := map[string]bool{}
	for _, m := range magpieModels("claude") {
		big[m.ID] = m.Context >= 1_000_000
	}
	opts := viaMagpie("claude", "")
	for i, o := range opts {
		if big[o.Ref] {
			opts[i].Value += "[1m]"
		}
	}
	return opts
}

// claudeManaged is where an administrator's Claude Code settings live; a var
// so tests can point it elsewhere.
var claudeManaged = func() string {
	switch runtime.GOOS {
	case "darwin":
		return "/Library/Application Support/ClaudeCode/managed-settings.json"
	case "windows":
		return `C:\ProgramData\ClaudeCode\managed-settings.json`
	}
	return "/etc/claude-code/managed-settings.json"
}
