package gateway

import (
	"encoding/base64"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/yetone/magpie/internal/provider"
)

// codexSignedIn signs a test's Codex in to ChatGPT (acct-1), with the
// saved accounts spares (acct-2, …) on beside it in magpie.
func codexSignedIn(t *testing.T, spares ...string) {
	t.Helper()
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(home, ".config"))
	t.Setenv("XDG_CACHE_HOME", filepath.Join(home, ".cache"))
	restingUntil.Lock()
	restingUntil.m = map[string]time.Time{}
	restingUntil.Unlock()
	claims := func(m map[string]any) string {
		b, _ := json.Marshal(m)
		return "h." + base64.RawURLEncoding.EncodeToString(b) + ".s"
	}
	auth := func(email, acct string) map[string]any {
		return map[string]any{"auth_mode": "chatgpt", "tokens": map[string]any{
			"id_token":      claims(map[string]any{"email": email}),
			"access_token":  claims(map[string]any{"exp": time.Now().Add(time.Hour).Unix(), "who": acct}),
			"refresh_token": "r-" + acct, "account_id": acct}}
	}
	os.MkdirAll(filepath.Join(home, ".codex"), 0o755)
	os.WriteFile(filepath.Join(home, ".codex", "auth.json"), mustJSON(auth("me@example.com", "acct-1")), 0o600)
	var saved []map[string]any
	for i, s := range spares {
		saved = append(saved, map[string]any{"agent": "codex", "user": s, "on": true, "seen": time.Now(),
			"auth": auth(s, "acct-"+string(rune('2'+i)))})
	}
	os.MkdirAll(filepath.Dir(provider.Path()), 0o755)
	os.WriteFile(filepath.Join(filepath.Dir(provider.Path()), "logins.json"), mustJSON(saved), 0o600)
}

// usedUp stands in for the ChatGPT backend with acct-1 out of its
// allowance, as it says so; it notes the accounts asked and the models.
func usedUp(t *testing.T, tried, models *[]string) {
	t.Helper()
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		*tried = append(*tried, r.Header.Get("chatgpt-account-id"))
		*models = append(*models, modelOf(b))
		if r.Header.Get("chatgpt-account-id") == "acct-1" {
			w.WriteHeader(429)
			io.WriteString(w, `{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_in_seconds":7200}}`)
			return
		}
		io.WriteString(w, sse(
			`data: {"type":"response.created","response":{"id":"r1","model":"gpt-5.5"}}`,
			`data: {"type":"response.output_text.delta","delta":"pong"}`,
			`data: {"type":"response.completed","response":{"id":"r1","usage":{"input_tokens":7,"output_tokens":1}}}`))
	}))
	t.Cleanup(up.Close)
	was := provider.CodexBase
	provider.CodexBase = up.URL + "/backend-api/codex"
	t.Cleanup(func() { provider.CodexBase = was })
}

// Codex on one of its own models, signed in to a ChatGPT account that has
// used its allowance up, with another of its accounts on in magpie: the
// turn goes on to that one, asked for the model Codex asked for, and the
// first sits out as long as ChatGPT says, up to the longest wait.
func TestCodexOwnModelMovesToNextAccount(t *testing.T) {
	codexSignedIn(t, "spare@example.com")
	var tried, models []string
	usedUp(t, &tried, &models)

	code, body := codexPost(t, `{"model":"gpt-5.5","stream":true,"input":"ping"}`)
	if code != 200 || !strings.Contains(body, "pong") {
		t.Fatalf("status %d: %s", code, body)
	}
	if strings.Join(tried, ",") != "acct-1,acct-2" || strings.Join(models, ",") != "gpt-5.5,gpt-5.5" {
		t.Fatalf("tried %v models %v", tried, models)
	}
	restingUntil.Lock()
	var rest time.Duration
	for _, u := range restingUntil.m {
		rest = max(rest, time.Until(u))
	}
	restingUntil.Unlock()
	if rest < longestWait-time.Minute || rest > longestWait {
		t.Errorf("rests %v, not the %v ChatGPT's two hours come to", rest, longestWait)
	}
	tried, models = nil, nil
	codexPost(t, `{"model":"gpt-5.5","stream":true,"input":"ping"}`)
	if strings.Join(tried, ",") != "acct-2" {
		t.Fatalf("second turn tried %v", tried)
	}
}

// Codex signed in to the next account once the one it was on is out
// (provider.SwitchCodexWhenUsedUp): the one out still rests, now beside it,
// and the one it is on now doesn't take that rest over.
func TestCodexSwitchedAccountRestsAsItself(t *testing.T) {
	codexSignedIn(t, "spare@example.com")
	var tried, models []string
	usedUp(t, &tried, &models)
	codexPost(t, `{"model":"gpt-5.5","stream":true,"input":"ping"}`)
	if err := provider.SwitchLogin("codex", "spare@example.com"); err != nil {
		t.Fatal(err)
	}
	tried, models = nil, nil
	code, body := codexPost(t, `{"model":"gpt-5.5","stream":true,"input":"ping"}`)
	if code != 200 || strings.Join(tried, ",") != "acct-2" {
		t.Fatalf("%d %s tried %v", code, body, tried)
	}
}

// With no other account on, Codex's own model goes as it came: its own
// sign-in, and the refusal back to Codex as ChatGPT said it.
func TestCodexOwnModelOneAccountRelayed(t *testing.T) {
	codexSignedIn(t)
	var tried, models []string
	usedUp(t, &tried, &models)
	code, body := codexPost(t, `{"model":"gpt-5.5","stream":true,"input":"ping"}`)
	if code != 429 || !strings.Contains(body, "usage_limit_reached") || strings.Join(tried, ",") != "acct-1" {
		t.Fatalf("%d %s tried %v", code, body, tried)
	}
}

func TestResetsIn(t *testing.T) {
	now := time.Unix(1_000_000, 0)
	for body, want := range map[string]time.Duration{
		`{"error":{"type":"usage_limit_reached","resets_at":1003600,"resets_in_seconds":5}}`: time.Hour,
		`{"error":{"type":"usage_limit_reached","resets_in_seconds":90}}`:                    90 * time.Second,
		`{"error":{"resets_at":999000}}`:                                                     0,
		`{"error":{"message":"quota"}}`:                                                      0,
		`not json`:                                                                           0,
	} {
		if got := resetsIn([]byte(body), now); got != want {
			t.Errorf("%s: %v, want %v", body, got, want)
		}
	}
}
