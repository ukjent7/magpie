package provider

// A Cursor subscription is used the way a Claude one is: through the vendor's
// own agent. Cursor's API is private and bound to its clients, so magpie runs
// the genuine cursor-agent CLI (see gateway/cursor_subscription.go) with the
// account it is signed in to; here is only who that account is, the models it
// offers, and the sign-in, which is cursor-agent's own `login`.

import (
	"bufio"
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/catalog"
)

// CursorExecutable finds the cursor-agent CLI; a var so tests can fake it.
var CursorExecutable = func() string {
	for _, name := range []string{"cursor-agent", "agent"} {
		if p, err := exec.LookPath(name); err == nil && (name == "cursor-agent" || isCursorAgent(p)) {
			return p
		}
	}
	home, _ := os.UserHomeDir()
	for _, p := range []string{filepath.Join(home, ".local", "bin", "cursor-agent"), "/usr/local/bin/cursor-agent", "/opt/homebrew/bin/cursor-agent"} {
		if st, err := os.Stat(p); err == nil && !st.IsDir() {
			return p
		}
	}
	return ""
}

// isCursorAgent tells Cursor's `agent` from any other program by that name.
func isCursorAgent(path string) bool {
	real, err := filepath.EvalSymlinks(path)
	return err == nil && strings.Contains(real, "cursor-agent")
}

var cursorStatus struct {
	sync.Mutex
	at         time.Time
	refreshing bool
	user, plan string
	ok         bool
}

// cursorIdentity asks cursor-agent who is signed in. `about` takes a few
// seconds, so after the first answer a stale one is served while a fresh one
// is fetched behind it.
func cursorIdentity() (user, plan string, ok bool) {
	cursorStatus.Lock()
	defer cursorStatus.Unlock()
	if cursorStatus.at.IsZero() {
		cursorStatus.user, cursorStatus.plan, cursorStatus.ok = askCursorIdentity()
		cursorStatus.at = time.Now()
	} else if time.Since(cursorStatus.at) > time.Minute && !cursorStatus.refreshing {
		cursorStatus.refreshing = true
		go func() {
			u, p, ok := askCursorIdentity()
			cursorStatus.Lock()
			cursorStatus.user, cursorStatus.plan, cursorStatus.ok = u, p, ok
			cursorStatus.at, cursorStatus.refreshing = time.Now(), false
			cursorStatus.Unlock()
		}()
	}
	return cursorStatus.user, cursorStatus.plan, cursorStatus.ok
}

func forgetCursorStatus() {
	cursorStatus.Lock()
	cursorStatus.at = time.Time{}
	cursorStatus.Unlock()
}

func askCursorIdentity() (user, plan string, ok bool) {
	path := CursorExecutable()
	if path == "" {
		return "", "", false
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	out, _ := agentCommand(ctx, path, "about", "--format", "json").Output()
	var about struct {
		SubscriptionTier string `json:"subscriptionTier"`
		UserEmail        string `json:"userEmail"`
	}
	if json.Unmarshal(out, &about) != nil || strings.TrimSpace(about.UserEmail) == "" {
		return "", "", false
	}
	return strings.TrimSpace(about.UserEmail), strings.TrimSpace(about.SubscriptionTier), true
}

func cursorAccount() (Provider, bool) {
	user, plan, ok := cursorIdentity()
	if !ok {
		return Provider{}, false
	}
	acct := &Account{Agent: "cursor", User: user, Plan: plan}
	acct.models = func() []catalog.Model { return []catalog.Model{{ID: "auto", Name: "Auto"}} }
	acct.fetch = func(ctx context.Context) ([]catalog.Model, error) {
		ms, err := cursorModels(ctx)
		if err != nil {
			return nil, err
		}
		return ms, catalog.SaveLive("cursor", "", ms)
	}
	return Provider{ID: "cursor", Name: "Cursor", Icon: "cursor", Website: "https://cursor.com", Account: acct}, true
}

var (
	ansi         = regexp.MustCompile(`\x1b\[[0-9;?]*[A-Za-z]`)
	cursorModelL = regexp.MustCompile(`^([A-Za-z0-9][\w.:-]*) - (.+)$`)
)

// cursorModels lists what the account can use, as `cursor-agent models`
// prints it: "id - Name", one a line.
func cursorModels(ctx context.Context) ([]catalog.Model, error) {
	path := CursorExecutable()
	if path == "" {
		return nil, errorf("cursor-agent is not installed")
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	out, err := agentCommand(ctx, path, "models").Output()
	if err != nil {
		return nil, errorf("cursor-agent models: %v", err)
	}
	return parseCursorModels(string(out)), nil
}

func parseCursorModels(out string) []catalog.Model {
	var ms []catalog.Model
	s := bufio.NewScanner(strings.NewReader(ansi.ReplaceAllString(out, "")))
	for s.Scan() {
		m := cursorModelL.FindStringSubmatch(strings.TrimSpace(s.Text()))
		if m == nil {
			continue
		}
		name := strings.TrimSpace(strings.TrimSuffix(strings.TrimSpace(m[2]), "(default)"))
		name = strings.TrimSpace(strings.TrimSuffix(name, "(current)"))
		ms = append(ms, catalog.Model{ID: m[1], Name: name, Context: cursorContext(m[1], name)})
	}
	return ms
}

// cursorDefaultContext is the context Cursor gives a model it doesn't name
// as a 1M one.
const cursorDefaultContext = 200_000

var (
	cursorMillion = regexp.MustCompile(`\b(\d+)M\b`)
	cursorVariant = regexp.MustCompile(`-(fast|none|low|medium|high|xhigh|extra-high|max|thinking)$`)
)

// cursorContext is how much of a conversation Cursor lets a model hold. Its
// ids ("claude-opus-5-5-high-fast") are its own, so no catalog knows them,
// and an agent given none took every Cursor model for its own default: a
// 1M one compacted at a fifth of it, and a 200K one — Cursor's own, where
// the name doesn't say 1M — was sent more than Cursor keeps. The name says
// which is which ("Claude Opus 5.5 1M"); under it, a model known to hold
// less keeps its own.
func cursorContext(id, name string) int {
	if m := cursorMillion.FindStringSubmatch(name); m != nil {
		n, _ := strconv.Atoi(m[1])
		return n * 1_000_000
	}
	base := strings.TrimPrefix(id, "cursor-")
	for {
		b := cursorVariant.ReplaceAllString(base, "")
		if b == base {
			break
		}
		base = b
	}
	if base == "auto" { // Cursor's pick, not a model of that name
		return cursorDefaultContext
	}
	if n := catalog.ContextOf(base); n > 0 && n < cursorDefaultContext {
		return n
	}
	return cursorDefaultContext
}

// withCursorContexts fills in a saved list's contexts, fetched before
// magpie kept them.
func withCursorContexts(ms []catalog.Model) []catalog.Model {
	out := slices.Clone(ms)
	for i, m := range out {
		if m.Context == 0 {
			out[i].Context = cursorContext(m.ID, m.Name)
		}
	}
	return out
}

var cursorLoginURL = regexp.MustCompile(`https://\S+`)

// startCursorSignIn runs `cursor-agent login` without its browser, hands its
// link to the window, and finishes when the CLI says the account is in.
func startCursorSignIn(s *signInFlow) error {
	path := CursorExecutable()
	if path == "" {
		return errorf("install Cursor's CLI first: curl https://cursor.com/install -fsS | bash")
	}
	return runCLISignIn(s, "cursor-agent login", append(os.Environ(), "NO_OPEN_BROWSER=1"), true, nil, func() (string, string, bool) {
		forgetCursorStatus()
		return askCursorIdentity()
	}, path, "login")
}
