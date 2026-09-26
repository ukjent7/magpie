package provider

// A Kiro subscription is used through Kiro's own agent, as Devin's is: magpie
// runs the genuine kiro-cli (gateway/kiro_subscription.go drives `kiro-cli
// acp`) with whatever it is signed in to, and never speaks Kiro's API itself.
// Here is only who that account is and the models it offers. A Kiro API key,
// for those who have one instead of a sign-in, is kept as the provider's key
// and handed to the CLI as KIRO_API_KEY — the CLI's own way to take one.

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/catalog"
	"github.com/yetone/magpie/internal/netproxy"
	"github.com/yetone/magpie/internal/proc"
)

// KiroExecutable finds kiro-cli; a var so tests can fake it. The installer
// puts it in ~/.local/bin, and on macOS inside Kiro CLI.app as well.
var KiroExecutable = func() string {
	if p, err := exec.LookPath("kiro-cli"); err == nil {
		return p
	}
	home, _ := os.UserHomeDir()
	for _, p := range []string{filepath.Join(home, ".local", "bin", "kiro-cli"),
		"/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli", "/usr/local/bin/kiro-cli", "/opt/homebrew/bin/kiro-cli"} {
		if st, err := os.Stat(p); err == nil && !st.IsDir() {
			return p
		}
	}
	return ""
}

// KiroChat is the binary that runs Kiro's agent: kiro-cli is a launcher that
// execs kiro-cli-chat, which it looks for under $HOME/.local/bin rather than
// beside itself, so the one next to the launcher (through its links) is run
// directly. A layout without it gets the launcher.
func KiroChat(launcher string) string {
	real, err := filepath.EvalSymlinks(launcher)
	if err != nil {
		real = launcher
	}
	name := "kiro-cli-chat"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	for _, dir := range []string{filepath.Dir(real), filepath.Dir(launcher)} {
		p := filepath.Join(dir, name)
		if st, err := os.Stat(p); err == nil && !st.IsDir() {
			return p
		}
	}
	return launcher
}

// KiroEnv is the environment kiro-cli runs in: the user's, less any
// KIRO_API_KEY of theirs — keys come from the provider, never the
// environment — and with the provider's key when it has one.
func KiroEnv(env []string, key string) []string {
	out := make([]string, 0, len(env)+1)
	for _, e := range env {
		k, _, _ := strings.Cut(e, "=")
		if strings.EqualFold(k, "KIRO_API_KEY") {
			continue
		}
		out = append(out, e)
	}
	if key != "" {
		out = append(out, "KIRO_API_KEY="+key)
	}
	return out
}

// kiroKey is the API key saved on the Kiro provider, if any.
func kiroKey() string {
	for _, p := range load().Providers {
		if p.ID == "kiro" {
			return p.Key
		}
	}
	return ""
}

var kiroStatus struct {
	sync.Mutex
	at         time.Time
	key        string
	refreshing bool
	user, plan string
	ok         bool
}

// kiroIdentity asks the CLI who is signed in, served stale while a fresh
// answer is fetched, as devinIdentity is.
func kiroIdentity(key string) (user, plan string, ok bool) {
	kiroStatus.Lock()
	defer kiroStatus.Unlock()
	if kiroStatus.at.IsZero() || kiroStatus.key != key {
		kiroStatus.user, kiroStatus.plan, kiroStatus.ok = askKiroIdentity(key)
		kiroStatus.at, kiroStatus.key = time.Now(), key
	} else if time.Since(kiroStatus.at) > time.Minute && !kiroStatus.refreshing {
		kiroStatus.refreshing = true
		go func() {
			u, p, ok := askKiroIdentity(key)
			kiroStatus.Lock()
			if kiroStatus.key == key {
				kiroStatus.user, kiroStatus.plan, kiroStatus.ok = u, p, ok
				kiroStatus.at = time.Now()
			}
			kiroStatus.refreshing = false
			kiroStatus.Unlock()
		}()
	}
	return kiroStatus.user, kiroStatus.plan, kiroStatus.ok
}

func askKiroIdentity(key string) (user, plan string, ok bool) {
	path := KiroExecutable()
	if path == "" {
		return "", "", false
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	cmd := proc.CommandContext(ctx, path, "whoami", "-f", "json")
	cmd.Env = netproxy.Env(KiroEnv(os.Environ(), key))
	// signed out, whoami says so and exits 1: the output is read either way
	out, _ := cmd.Output()
	return parseKiroWhoami(out)
}

// parseKiroWhoami reads `kiro-cli whoami -f json`. Signed out it is
// {"account":null}; with an API key {"accountType":"ApiKey","email":null};
// signed in, the account's type and its email.
func parseKiroWhoami(b []byte) (user, plan string, ok bool) {
	var w struct {
		AccountType string `json:"accountType"`
		Email       string `json:"email"`
		StartURL    string `json:"startUrl"`
	}
	if json.Unmarshal(b, &w) != nil || w.AccountType == "" && w.Email == "" {
		return "", "", false
	}
	user = w.Email
	switch w.AccountType {
	case "ApiKey":
		plan = "API key"
	case "BuilderId":
		plan = "Builder ID"
	case "IamIdentityCenter":
		plan = "IAM Identity Center"
	default:
		plan = w.AccountType
	}
	if user == "" {
		user = "Kiro " + strings.ToLower(plan)
		if plan == "" {
			user = "Kiro account"
		}
	}
	return user, plan, true
}

func kiroAccount() (Provider, bool) {
	if KiroExecutable() == "" {
		return Provider{}, false
	}
	key := kiroKey()
	user, plan, ok := kiroIdentity(key)
	if !ok {
		return Provider{}, false
	}
	acct := &Account{Agent: "kiro", User: user, Plan: plan}
	acct.models = func() []catalog.Model {
		// what the last fetch saved — Available() runs on every request,
		// so no CLI is spawned here
		if ms, _, ok := catalog.Live("kiro"); ok {
			return ms
		}
		return []catalog.Model{{ID: "auto", Name: "Auto", Provider: "kiro"}}
	}
	acct.fetch = func(ctx context.Context) ([]catalog.Model, error) {
		ms, err := askKiroModels(ctx, key)
		if err != nil {
			return nil, err
		}
		return ms, catalog.SaveLive("kiro", "", ms)
	}
	return Provider{ID: "kiro", Name: "Kiro", Icon: "generic", Website: "https://kiro.dev", Key: key, Account: acct}, true
}

func askKiroModels(ctx context.Context, key string) ([]catalog.Model, error) {
	path := KiroExecutable()
	if path == "" {
		return nil, errors.New("kiro-cli is not installed; install it from https://kiro.dev/cli")
	}
	// Signed out, --list-models starts a browser sign-in rather than
	// failing, so it is only asked of a CLI that says who it is.
	if _, _, ok := askKiroIdentity(key); !ok {
		return nil, errors.New("kiro-cli is not signed in; run `kiro-cli login`")
	}
	ctx, cancel := context.WithTimeout(ctx, 60*time.Second)
	defer cancel()
	cmd := proc.CommandContext(ctx, KiroChat(path), "chat", "--list-models", "-f", "json")
	cmd.Env = netproxy.Env(KiroEnv(os.Environ(), key))
	out, err := cmd.Output()
	if err != nil {
		return nil, errorf("kiro-cli chat --list-models: %v", err)
	}
	ms := parseKiroModels(out)
	if len(ms) == 0 {
		return nil, errors.New("kiro-cli chat --list-models: no models")
	}
	return ms, nil
}

// parseKiroModels reads `kiro-cli chat --list-models -f json`:
//
//	{"models": [{"model_name": "auto", "model_id": "auto",
//	  "context_window_tokens": 200000}], "default_model": "auto"}
//
// The default comes first.
func parseKiroModels(b []byte) []catalog.Model {
	var list struct {
		Models []struct {
			Name    string `json:"model_name"`
			ID      string `json:"model_id"`
			Context int    `json:"context_window_tokens"`
		} `json:"models"`
		Default string `json:"default_model"`
	}
	if json.Unmarshal(b, &list) != nil {
		return nil
	}
	var out []catalog.Model
	for _, m := range list.Models {
		if m.ID == "" {
			continue
		}
		name := m.Name
		if name == "" || name == m.ID {
			name = m.ID
			if m.ID == "auto" {
				name = "Auto"
			}
		}
		cm := catalog.Model{ID: m.ID, Name: name, Provider: "kiro", Context: m.Context}
		if m.ID == list.Default {
			out = append([]catalog.Model{cm}, out...)
		} else {
			out = append(out, cm)
		}
	}
	return out
}

// KiroSignedIn asks kiro-cli, now, whether it has an account to run with:
// the gateway asks when a run fails, since kiro-cli-chat run directly starts
// signed out and only fails on the prompt, with nothing about signing in.
func KiroSignedIn(key string) bool {
	_, _, ok := askKiroIdentity(key)
	return ok
}
