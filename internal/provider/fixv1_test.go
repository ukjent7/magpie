package provider

import (
	"context"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
)

// A relay given without its /v1 lists its models under /v1 only; fetching
// them sets the base right, so chat goes to /v1/chat/completions too. One
// that answers at the base as written is left alone.
func TestFetchAddsMissingV1(t *testing.T) {
	isolate(t)
	h := t.TempDir()
	t.Setenv("HOME", h)
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(h, ".config"))
	t.Setenv("XDG_CACHE_HOME", filepath.Join(h, ".cache"))
	t.Setenv("PATH", h)
	for _, v := range []string{"CLAUDE_CONFIG_DIR", "CODEX_HOME"} {
		t.Setenv(v, "")
	}

	v1only := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/models" {
			http.NotFound(w, r)
			return
		}
		w.Write([]byte(`{"data":[{"id":"gpt-5.5"}]}`))
	}))
	defer v1only.Close()
	both := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte(`{"data":[{"id":"gpt-5.5"}]}`))
	}))
	defer both.Close()

	for _, p := range []Provider{
		{ID: "relay", Name: "Relay", Chat: v1only.URL + "/", Responses: v1only.URL, Anthropic: v1only.URL, Key: "sk-x"},
		{ID: "root", Name: "Root", Chat: both.URL, Key: "sk-y"},
	} {
		if err := Save(p); err != nil {
			t.Fatal(err)
		}
	}
	for _, id := range []string{"relay", "root"} {
		p, err := Find(id)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := p.Fetch(context.Background()); err != nil {
			t.Fatalf("%s: %v", id, err)
		}
	}
	p, _ := Find("relay")
	if p.Chat != v1only.URL+"/v1" || p.Responses != v1only.URL+"/v1" {
		t.Errorf("relay: chat %q responses %q, want …/v1", p.Chat, p.Responses)
	}
	if p.Anthropic != v1only.URL {
		t.Errorf("relay: anthropic base changed to %q; it has no /v1", p.Anthropic)
	}
	if q, _ := Find("root"); q.Chat != both.URL {
		t.Errorf("root: chat changed to %q", q.Chat)
	}
}
