package provider

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
)

// Codex signed in to an account out of its allowance is signed in to the
// next account that is on and has room; while none has, it stays.
func TestCodexSwitchedWhenUsedUp(t *testing.T) {
	home := signIn(t) // me@example.com, acct-1
	rememberLogins(true)
	codexSignIn(t, home, "spare@example.com", "r-spare")
	rememberLogins(true)
	codexSignIn(t, home, "work@example.com", "r-work")
	rememberLogins(true)
	used := map[string]float64{"acct-work@example.com": 100, "acct-1": 100, "acct-spare@example.com": 20}
	fake := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{"plan_type": "plus", "rate_limit": map[string]any{
			"primary_window": map[string]any{"used_percent": used[r.Header.Get("chatgpt-account-id")], "limit_window_seconds": 18000}}})
	}))
	defer fake.Close()
	old := CodexBase
	CodexBase = fake.URL + "/backend-api/codex"
	t.Cleanup(func() { CodexBase = old })
	fresh := func() {
		loginUsageCache.Lock()
		loginUsageCache.m = nil
		loginUsageCache.Unlock()
	}
	switched := func() string {
		t.Helper()
		fresh()
		to, err := SwitchCodexWhenUsedUp(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		return to
	}

	// no other account on: nothing to go to
	if to := switched(); to != "" {
		t.Fatalf("switched to %s with none on", to)
	}
	// the one on is out too
	if err := SetLoginOn("codex", "me@example.com", true); err != nil {
		t.Fatal(err)
	}
	if to := switched(); to != "" {
		t.Fatalf("switched to %s, out as well", to)
	}
	// one on with room: the first of those, not one that is off
	if err := SetLoginOn("codex", "spare@example.com", true); err != nil {
		t.Fatal(err)
	}
	if to := switched(); to != "spare@example.com" {
		t.Fatalf("switched to %q", to)
	}
	var live codexAuth
	readJSON(filepath.Join(home, ".codex", "auth.json"), &live)
	if live.Tokens.RefreshToken != "r-spare" {
		t.Fatalf("Codex is signed in with %+v", live.Tokens)
	}
	for _, l := range Logins("codex") {
		if l.User == "work@example.com" && (!l.On || l.Active) || l.User == "spare@example.com" && !l.Active {
			t.Fatalf("after: %+v", l)
		}
	}
	// on one with room it stays
	if to := switched(); to != "" {
		t.Fatalf("switched again, to %s", to)
	}
	// an allowance not known is not taken for used up
	used["acct-spare@example.com"] = 100
	used["acct-work@example.com"] = 0
	CodexBase = "http://127.0.0.1:1/backend-api/codex"
	if to := switched(); to != "" {
		t.Fatalf("switched to %s, not knowing", to)
	}
}
