package gateway

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/yetone/magpie/internal/provider"
)

func TestTokenFloor(t *testing.T) {
	for msg, want := range map[string]int{
		`{"error":{"message":"max_tokens must be greater than 2"}}`:                3,
		`max_completion_tokens must be at least 16`:                                16,
		`Invalid 'max_output_tokens': integer below minimum value. Expected >= 16`: 16,
		`max_tokens is too large: 999999`:                                          0,
		`messages: at least 1 message is required`:                                 0,
	} {
		if got := tokenFloor([]byte(msg)); got != want {
			t.Errorf("%s: %d, want %d", msg, got, want)
		}
	}
}

// A provider that turns away a request for a reply of a token or two
// (#64: "max_tokens must be greater than 2", to an app checking the model
// is up) is asked again for the least it gives, and the app gets a reply.
func TestTooFewTokensAskedAgain(t *testing.T) {
	t.Setenv("XDG_CONFIG_HOME", t.TempDir())
	t.Setenv("XDG_CACHE_HOME", t.TempDir())
	var asked []int
	stubborn := false
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var q struct {
			MaxTokens int `json:"max_tokens"`
		}
		b, _ := io.ReadAll(r.Body)
		json.Unmarshal(b, &q)
		asked = append(asked, q.MaxTokens)
		w.Header().Set("Content-Type", "application/json")
		if stubborn || q.MaxTokens > 0 && q.MaxTokens <= 2 {
			w.WriteHeader(400)
			io.WriteString(w, `{"error":{"message":"max_tokens must be greater than 2","type":"invalid_request_error"}}`)
			return
		}
		io.WriteString(w, `{"id":"c1","object":"chat.completion","model":"m1","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"length"}],"usage":{"prompt_tokens":3,"completion_tokens":3}}`)
	}))
	defer up.Close()
	if err := provider.Save(provider.Provider{ID: "bai", Name: "Bai", Key: "k", Models: []string{"m1"}, Chat: up.URL + "/v1"}); err != nil {
		t.Fatal(err)
	}
	code, body := post(t, "/v1/chat/completions", `{"model":"bai/m1","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}`)
	if code != 200 || !strings.Contains(body, `"ok"`) {
		t.Fatalf("status %d: %s", code, body)
	}
	if len(asked) != 2 || asked[0] != 1 || asked[1] != 3 {
		t.Fatalf("asked for %v tokens", asked)
	}

	// once is enough: a provider that still says no is the app's to see
	asked, stubborn = nil, true
	code, _ = post(t, "/v1/chat/completions", `{"model":"bai/m1","max_tokens":2,"messages":[{"role":"user","content":"hi"}]}`)
	if code != 400 || len(asked) != 2 {
		t.Fatalf("%d after %v", code, asked)
	}
	// and a request that didn't ask for a length isn't sent again
	asked = nil
	code, _ = post(t, "/v1/chat/completions", `{"model":"bai/m1","messages":[{"role":"user","content":"hi"}]}`)
	if code != 400 || len(asked) != 1 {
		t.Fatalf("%d after %v", code, asked)
	}
	stubborn = false
}
