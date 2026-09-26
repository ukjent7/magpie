package provider

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// Saved accounts of an agent not signed in where magpie looks (a magpie
// serve under another HOME) are said to be left out, not dropped silently.
func TestSavedButSignedOut(t *testing.T) {
	isolate(t)
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(home, ".config"))
	t.Setenv("CLAUDE_CONFIG_DIR", "")
	if err := writeLogins([]savedLogin{
		{Agent: "codex", User: "a@x.com", Auth: []byte(`{}`)},
		{Agent: "codex", User: "b@x.com", Auth: []byte(`{}`)},
	}); err != nil {
		t.Fatal(err)
	}
	var codex *Exclusion
	for _, x := range Excluded() {
		if x.Agent == "codex" && x.SignedOut {
			codex = &x
		}
		if x.Agent == "claude" {
			t.Errorf("claude has no saved accounts, yet: %+v", x)
		}
	}
	if codex == nil || !strings.Contains(codex.Why, "2 accounts are") || !strings.Contains(codex.Why, filepath.Join(home, ".codex", "auth.json")) {
		t.Fatalf("codex: %+v", codex)
	}
	// signed in: nothing to say
	os.MkdirAll(filepath.Join(home, ".codex"), 0o700)
	os.WriteFile(filepath.Join(home, ".codex", "auth.json"), []byte(`{"auth_mode":"chatgpt","tokens":{"access_token":"x","account_id":"acc-1"}}`), 0o600)
	for _, x := range Excluded() {
		if x.SignedOut {
			t.Errorf("signed in, yet: %+v", x)
		}
	}
}
