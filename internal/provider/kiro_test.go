package provider

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestParseKiroWhoami(t *testing.T) {
	for _, c := range []struct {
		in, user, plan string
		ok             bool
	}{
		// what kiro-cli 2.24.1 prints signed out, and with only an API key
		{`{"account":null}`, "", "", false},
		{`{"accountType":"ApiKey","email":null}`, "Kiro api key", "API key", true},
		{`{"accountType":"BuilderId","email":"me@example.com"}`, "me@example.com", "Builder ID", true},
		{`{"accountType":"IamIdentityCenter","email":"me@corp.example","startUrl":"https://corp.awsapps.com/start"}`, "me@corp.example", "IAM Identity Center", true},
		{``, "", "", false},
		{`error`, "", "", false},
	} {
		user, plan, ok := parseKiroWhoami([]byte(c.in))
		if user != c.user || plan != c.plan || ok != c.ok {
			t.Errorf("%s: %q %q %v", c.in, user, plan, ok)
		}
	}
}

func TestParseKiroModels(t *testing.T) {
	// the shape `kiro-cli chat --list-models -f json` printed, with a second
	// model the default is put before
	ms := parseKiroModels([]byte(`{"models":[{"model_name":"claude-sonnet-4.5","model_id":"claude-sonnet-4.5","context_window_tokens":200000},{"model_name":"auto","model_id":"auto","context_window_tokens":200000},{"model_name":"","model_id":""}],"default_model":"auto"}`))
	if len(ms) != 2 || ms[0].ID != "auto" || ms[0].Name != "Auto" || ms[1].ID != "claude-sonnet-4.5" || ms[1].Context != 200000 || ms[1].Provider != "kiro" {
		t.Fatalf("models = %+v", ms)
	}
	if parseKiroModels([]byte(`nope`)) != nil {
		t.Fatal("parsed junk")
	}
}

func TestKiroEnvTakesTheKeyFromTheProvider(t *testing.T) {
	got := strings.Join(KiroEnv([]string{"PATH=/bin", "KIRO_API_KEY=from-env", "kiro_api_key=lower"}, ""), "\n")
	if strings.Contains(got, "KIRO_API_KEY") || strings.Contains(got, "kiro_api_key") || !strings.Contains(got, "PATH=/bin") {
		t.Fatalf("env = %q", got)
	}
	got = strings.Join(KiroEnv([]string{"PATH=/bin"}, "ksk_saved"), "\n")
	if !strings.Contains(got, "KIRO_API_KEY=ksk_saved") {
		t.Fatalf("env = %q", got)
	}
}

func TestKiroChatBesideTheLauncher(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("links")
	}
	app := t.TempDir()
	for _, n := range []string{"kiro-cli", "kiro-cli-chat"} {
		os.WriteFile(filepath.Join(app, n), []byte("#!/bin/sh\n"), 0o755)
	}
	bin := t.TempDir()
	link := filepath.Join(bin, "kiro-cli")
	if err := os.Symlink(filepath.Join(app, "kiro-cli"), link); err != nil {
		t.Fatal(err)
	}
	if got := KiroChat(link); got != filepath.Join(app, "kiro-cli-chat") {
		// EvalSymlinks may resolve the temp dir itself (/var → /private/var)
		if real, _ := filepath.EvalSymlinks(filepath.Join(app, "kiro-cli-chat")); got != real {
			t.Fatalf("chat = %q", got)
		}
	}
	lone := filepath.Join(t.TempDir(), "kiro-cli")
	os.WriteFile(lone, []byte("#!/bin/sh\n"), 0o755)
	if got := KiroChat(lone); got != lone {
		t.Fatalf("without a chat binary: %q", got)
	}
}

// A key saved on Kiro is kept with it, where another account's is not, and
// it is how a Kiro that isn't signed in is added.
func TestSaveKeepsKirosKey(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	t.Setenv("XDG_CONFIG_HOME", t.TempDir())
	t.Setenv("XDG_CACHE_HOME", t.TempDir())
	was := KiroExecutable
	KiroExecutable = func() string { return "/nonexistent/kiro-cli" }
	t.Cleanup(func() { KiroExecutable = was })
	if err := Save(Provider{ID: "kiro", Name: "kiro", Key: "ksk_1", Chat: "http://ignored"}); err != nil {
		t.Fatal(err)
	}
	if got := kiroKey(); got != "ksk_1" {
		t.Fatalf("key = %q", got)
	}
	for _, p := range load().Providers {
		if p.ID == "kiro" && (p.Chat != "" || p.Name != "") {
			t.Fatalf("kept more than the key: %+v", p)
		}
	}
	// once saved, Kiro's record stays the account's, CLI or not: it never
	// turns into an HTTP provider that would hide the subscription
	KiroExecutable = func() string { return "" }
	if err := Save(Provider{ID: "kiro", Name: "kiro", Key: "ksk_2", Chat: "http://x"}); err != nil {
		t.Fatal(err)
	}
	for _, p := range load().Providers {
		if p.ID == "kiro" && (p.Chat != "" || p.Key != "ksk_2") {
			t.Fatalf("saved %+v", p)
		}
	}
	// and a fresh "kiro" without a key is still refused as a custom name
	t.Setenv("XDG_CONFIG_HOME", t.TempDir())
	if err := Save(Provider{ID: "kiro", Name: "kiro", Key: "", Chat: "http://x"}); err == nil {
		t.Fatal("a custom provider took Kiro's id")
	}
}
