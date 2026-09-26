package gateway

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"regexp"
	"strings"
	"testing"

	"github.com/yetone/magpie/internal/provider"
	"github.com/yetone/magpie/internal/settings"
)

// the vendor sees placeholders; the agent gets its key back, even from a
// stream that splits a placeholder and a protocol it was translated from
func TestRedactedRequest(t *testing.T) {
	const key = "sk-proj-abcdEFGH1234ijklMNOP5678qrst"
	var got []byte
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got, _ = io.ReadAll(r.Body)
		p := regexp.MustCompile(`\{\{API_KEY_[a-z2-7]{8}\}\}`).FindString(string(got))
		if p == "" {
			p = "none"
		}
		chunk := func(s string) string {
			b, _ := json.Marshal(map[string]any{"id": "c1", "model": "m1", "choices": []any{map[string]any{"index": 0, "delta": map[string]any{"content": s}}}})
			return "data: " + string(b)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, sse(chunk("it is "+p[:9]), chunk(p[9:]), `data: {"id":"c1","model":"m1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}`, "data: [DONE]"))
	}))
	defer up.Close()
	t.Setenv("XDG_CONFIG_HOME", t.TempDir())
	t.Setenv("XDG_CACHE_HOME", t.TempDir())
	if err := provider.Save(provider.Provider{ID: "fake", Name: "Fake", Key: "k", Models: []string{"m1"}, Chat: up.URL + "/v1"}); err != nil {
		t.Fatal(err)
	}
	req := `{"model":"fake/m1","max_tokens":10,"stream":true,"messages":[{"role":"user","content":"my key is ` + key + `"}]}`

	// off: as it was
	post(t, "/v1/messages", req)
	if !strings.Contains(string(got), key) {
		t.Fatalf("off, sent: %s", got)
	}

	if err := settings.Save(settings.Settings{Redact: true}); err != nil {
		t.Fatal(err)
	}
	code, body := post(t, "/v1/messages", req)
	if code != 200 || strings.Contains(string(got), key) || !strings.Contains(string(got), "{{API_KEY_") {
		t.Fatalf("%d, sent: %s", code, got)
	}
	var text strings.Builder
	for _, e := range events(body) {
		if d, ok := e["delta"].(map[string]any); ok {
			s, _ := d["text"].(string)
			text.WriteString(s)
		}
	}
	if text.String() != "it is "+key {
		t.Fatalf("text %q in:\n%s", text.String(), body)
	}
}
