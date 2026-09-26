package provider

// A Grok subscription (SuperGrok, X Premium+) is used the way a Cursor one
// is: through xAI's own Grok Build CLI, which magpie runs behind the gateway
// (see gateway/grok_subscription.go). Here is who the CLI is signed in to,
// the models it offers, the sign-in, which is the CLI's own `login`, and the
// token the gateway's runs borrow from it.

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/catalog"
	"github.com/yetone/magpie/internal/netproxy"
	"github.com/yetone/magpie/internal/proc"
)

// GrokExecutable finds the Grok Build CLI; a var so tests can fake it.
var GrokExecutable = func() string {
	home, _ := os.UserHomeDir()
	if p := filepath.Join(GrokHome(), "bin", "grok"); isFile(p) {
		return p
	}
	if p, err := exec.LookPath("grok"); err == nil && isGrokBuild(p) {
		return p
	}
	if p := filepath.Join(home, ".local", "bin", "grok"); isFile(p) && isGrokBuild(p) {
		return p
	}
	return ""
}

func isFile(p string) bool {
	st, err := os.Stat(p)
	return err == nil && !st.IsDir()
}

// isGrokBuild tells xAI's grok from any other program by that name: its
// installer keeps it under the grok home.
func isGrokBuild(path string) bool {
	real, err := filepath.EvalSymlinks(path)
	return err == nil && strings.Contains(filepath.ToSlash(real), "/.grok/")
}

// GrokHome is where the CLI keeps its sign-in and settings.
func GrokHome() string {
	if h := os.Getenv("GROK_HOME"); h != "" {
		return h
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".grok")
}

// grokCredential is the CLI's sign-in, as its auth.json keeps it.
type grokCredential struct {
	Key       string    `json:"key"`
	Email     string    `json:"email"`
	ExpiresAt time.Time `json:"expires_at"`
	Issuer    string    `json:"oidc_issuer"`
}

// readGrokCredential reads the sign-in the CLI keeps in its home. magpie
// only reads it: the CLI refreshes it with a token that changes each time.
func readGrokCredential(home string) (grokCredential, bool) {
	var all map[string]grokCredential
	if !readJSON(filepath.Join(home, "auth.json"), &all) {
		return grokCredential{}, false
	}
	keys := make([]string, 0, len(all))
	for k := range all {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		if c := all[k]; c.Key != "" {
			return c, true
		}
	}
	return grokCredential{}, false
}

func grokAccount() (Provider, bool) {
	if GrokExecutable() == "" {
		return Provider{}, false
	}
	ls := grokLogins()
	if len(ls) == 0 {
		return Provider{}, false
	}
	home := ls[0].Home
	acct := &Account{Agent: "grok", User: ls[0].User, Plan: ls[0].Plan, Home: home}
	if home == GrokHome() {
		acct.Home = "" // the CLI's own, wherever it is
	}
	acct.models = func() []catalog.Model { return []catalog.Model{{ID: "grok-4.7", Name: "grok-4.7"}} }
	acct.fetch = func(ctx context.Context) ([]catalog.Model, error) {
		ms, err := grokModels(ctx, home)
		if err != nil {
			return nil, err
		}
		return ms, catalog.SaveLive("grok", "", ms)
	}
	return Provider{ID: "grok", Name: "Grok (SuperGrok)", Icon: "xai", Website: "https://x.ai/cli", Account: acct}, true
}

var grokModelL = regexp.MustCompile(`^[*-]\s+([A-Za-z0-9][\w.:-]*)`)

// grokModels lists what the account signed in in home can use, as `grok
// models` prints it.
func grokModels(ctx context.Context, home string) ([]catalog.Model, error) {
	path := GrokExecutable()
	if path == "" {
		return nil, errorf("Grok Build is not installed")
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	cmd := proc.CommandContext(ctx, path, "models")
	cmd.Dir, _ = os.UserHomeDir()
	cmd.Env = netproxy.Env(grokOwnEnv(os.Environ(), home))
	out, err := cmd.Output()
	if err != nil {
		return nil, errorf("grok models: %v", err)
	}
	return parseGrokModels(string(out)), nil
}

func parseGrokModels(out string) []catalog.Model {
	var ms []catalog.Model
	s := bufio.NewScanner(strings.NewReader(ansi.ReplaceAllString(out, "")))
	for s.Scan() {
		if m := grokModelL.FindStringSubmatch(strings.TrimSpace(s.Text())); m != nil {
			ms = append(ms, catalog.Model{ID: m[1], Name: m[1]})
		}
	}
	return ms
}

// grokRefreshMargin is how close to its expiry a borrowed token has the CLI
// refresh it first.
const grokRefreshMargin = 5 * time.Minute

var grokRefresh sync.Mutex

// GrokToken writes the CLI's access token for a Grok run in magpie's home,
// as an external auth provider answers (GROK_AUTH_PROVIDER_COMMAND).
func GrokToken(w io.Writer, home, binary string, expired bool) error {
	c, err := grokAccessToken(home, binary, expired)
	if err != nil {
		return err
	}
	left := int(time.Until(c.ExpiresAt).Seconds())
	if c.ExpiresAt.IsZero() {
		left = 3600
	}
	return json.NewEncoder(w).Encode(map[string]any{"access_token": c.Key, "expires_in": left, "issuer": c.Issuer})
}

// grokAccessToken is the CLI's sign-in, with a token still good. One about
// to expire is first refreshed by the CLI in its own home: a `grok models`
// there renews it the way any use of the CLI does; so is one a run has
// found expired.
func grokAccessToken(home, binary string, expired bool) (grokCredential, error) {
	c, ok := readGrokCredential(home)
	if ok && (expired || time.Until(c.ExpiresAt) < grokRefreshMargin) && binary != "" {
		grokRefresh.Lock()
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		cmd := proc.CommandContext(ctx, binary, "models")
		cmd.Dir = filepath.Dir(home)
		cmd.Env = netproxy.Env(grokOwnEnv(os.Environ(), home))
		_ = cmd.Run()
		cancel()
		grokRefresh.Unlock()
		c, ok = readGrokCredential(home)
	}
	if !ok {
		return c, errorf("Grok is not signed in; run `grok login`")
	}
	if !c.ExpiresAt.IsZero() && time.Until(c.ExpiresAt) <= 0 {
		return c, errorf("Grok's sign-in has expired; run `grok login`")
	}
	return c, nil
}

// grokOwnEnv runs the CLI as the user runs it, in its own home.
func grokOwnEnv(env []string, home string) []string {
	out := make([]string, 0, len(env)+1)
	for _, e := range env {
		k, _, _ := strings.Cut(e, "=")
		switch strings.ToUpper(k) {
		case "GROK_HOME", "GROK_AUTH_PROVIDER_COMMAND", "GROK_AUTH_EXPIRED":
			continue
		}
		out = append(out, e)
	}
	return append(out, "GROK_HOME="+home)
}

// startGrokSignIn runs `grok login` with its device code, hands its link to
// the window, and finishes when the CLI says the account is in. With the
// CLI signed in already, a further account signs in in a home of magpie's,
// so the CLI's own sign-in stays as it is.
func startGrokSignIn(s *signInFlow) error {
	path := GrokExecutable()
	if path == "" {
		return errorf("install Grok Build first: curl -fsSL https://x.ai/cli/install.sh | bash")
	}
	if _, ok := readGrokCredential(GrokHome()); !ok {
		return runCLISignIn(s, "grok login", nil, true, nil, func() (string, string, bool) {
			c, ok := readGrokCredential(GrokHome())
			return c.Email, "", ok
		}, path, "login", "--device-auth")
	}
	home, err := newGrokHome()
	if err != nil {
		return err
	}
	err = runCLISignIn(s, "grok login", grokOwnEnv(os.Environ(), home), false, func() { removeGrokHome(home) }, func() (string, string, bool) {
		user, err := addGrokLogin(home)
		return user, "", err == nil
	}, path, "login", "--device-auth")
	if err != nil {
		removeGrokHome(home)
	}
	return err
}

// agentCommand runs an agent's CLI with magpie's proxy.
func agentCommand(ctx context.Context, path string, args ...string) *exec.Cmd {
	cmd := proc.CommandContext(ctx, path, args...)
	cmd.Env = netproxy.Env(nil)
	return cmd
}

// runCLISignIn runs an agent's own login command, hands the first link it
// prints to the window, and finishes when the command does and identity
// says who is signed in. using says whether the agent now uses that
// account; failed, when there is one, undoes what a sign-in that did not
// finish left behind.
func runCLISignIn(s *signInFlow, what string, env []string, using bool, failed func(), identity func() (user, plan string, ok bool), path string, args ...string) error {
	ctx, cancel := context.WithCancel(context.Background())
	cmd := proc.CommandContext(ctx, path, args...)
	cmd.Dir, _ = os.UserHomeDir()
	cmd.Env = netproxy.Env(env)
	out, err := cmd.StdoutPipe()
	if err != nil {
		cancel()
		return err
	}
	cmd.Stderr = cmd.Stdout
	if err := cmd.Start(); err != nil {
		cancel()
		return err
	}
	s.mu.Lock()
	s.stop = cancel
	s.mu.Unlock()
	got := make(chan string, 1)
	go func() {
		sc := bufio.NewScanner(out)
		sent := false
		var tail []string
		for sc.Scan() {
			line := ansi.ReplaceAllString(sc.Text(), "")
			if u := cursorLoginURL.FindString(line); u != "" && !sent {
				sent = true
				got <- u
			}
			if strings.TrimSpace(line) != "" {
				tail = append(tail, strings.TrimSpace(line))
			}
		}
		if !sent {
			close(got)
		}
		err := cmd.Wait()
		cancel()
		forgetAccountCaches()
		if err == nil {
			if user, plan, ok := identity(); ok {
				s.finish(SignInState{State: "done", User: user, Plan: plan, Using: using})
				return
			}
		}
		if failed != nil {
			failed()
		}
		msg := what + " didn't finish"
		if n := len(tail); n > 0 {
			msg = tail[n-1]
		}
		s.finish(SignInState{State: "failed", Error: msg})
	}()
	select {
	case u, ok := <-got:
		if !ok {
			return fmt.Errorf("%s gave no link to open", what)
		}
		s.mu.Lock()
		s.st.URL = u
		s.mu.Unlock()
		return nil
	case <-time.After(30 * time.Second):
		cancel()
		return fmt.Errorf("%s gave no link to open", what)
	}
}

// GrokUser is who the grok with this home is signed in to.
func GrokUser(home string) (string, bool) {
	c, ok := readGrokCredential(home)
	return c.Email, ok
}
