package gateway

// A Kiro subscription runs through the genuine kiro-cli in ACP mode, the way
// a Devin one runs through `devin acp` (devin_subscription.go), and speaks
// the same protocol over the same connection: initialize, session/new,
// session/set_model, then session/prompt, the answer streaming back as
// session/update notifications. The caller's tools are the magpie MCP
// server, whose calls park in the stdio helper until the caller sends
// results.
//
// Kiro's own tools (shell, files, the web, the user's MCP servers) are kept
// out by the agent it runs as: one of magpie's, written into the empty
// workspace, whose only tools are the magpie server's. A permission prompt
// for anything else is refused.
//
// It runs in the user's home, as the Claude bridge does: the sign-in lives
// in kiro-cli's database under the platform's data directory, which is not
// found through HOME on every OS, and the agent file already leaves out the
// user's agents, MCP servers, steering and hooks.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/netproxy"
	"github.com/yetone/magpie/internal/proc"
	"github.com/yetone/magpie/internal/provider"
)

// kiroHandshake bounds initialize/session/new: kiro-cli takes a while to
// start (twenty seconds and more was seen for initialize), but one that
// answers nothing in this long is dead.
var kiroHandshake = 90 * time.Second

// kiroMCPWait is how long a session with tools waits for Kiro to say the
// magpie server is up before prompting anyway.
var kiroMCPWait = 30 * time.Second

var errKiroSignedOut = errors.New("kiro-cli is not signed in; run `kiro-cli login`, or give the Kiro provider an API key")

// kiroAgent is the name of the agent magpie writes into the workspace.
const kiroAgent = "magpie"

func (b *subscriptionBridge) startKiro(ctx context.Context, req *Request, model, key string) (*subscriptionRun, <-chan Event, error) {
	binary := provider.KiroExecutable()
	if binary == "" {
		return nil, nil, errors.New("kiro-cli is not installed; install kiro-cli from https://kiro.dev/cli and sign in with `kiro-cli login`")
	}
	exe, err := os.Executable()
	if err != nil {
		return nil, nil, err
	}
	tmp, err := os.MkdirTemp("", "magpie-kiro-")
	if err != nil {
		return nil, nil, err
	}
	cleanup := func() { _ = os.RemoveAll(tmp) }

	tools := bridgeTools(req)
	if len(tools) > 0 {
		tools = append(tools, bridgeTool{Name: waitTool,
			Description: "Wait for a tool call that is still running in the user's environment, and get its result. Only for a call whose result said it is still running.",
			InputSchema: json.RawMessage(`{"type":"object","properties":{"call":{"type":"string","description":"the id the still-running result gave"}},"required":["call"]}`)})
	}
	toolsPath := filepath.Join(tmp, "tools.json")
	toolBytes, _ := json.Marshal(tools)
	if err := os.WriteFile(toolsPath, toolBytes, 0o600); err != nil {
		cleanup()
		return nil, nil, err
	}
	token := randomToken()
	callback := callbackBaseURL() + "/_magpie/claude-mcp/" + token
	ws := filepath.Join(tmp, "workspace")
	if err := writeKiroAgent(ws, exe, callback, toolsPath, len(tools) > 0); err != nil {
		cleanup()
		return nil, nil, err
	}

	cmd := proc.CommandContext(context.Background(), provider.KiroChat(binary), "acp", "--agent", kiroAgent)
	cmd.Dir = ws
	cmd.Env = netproxy.Env(provider.KiroEnv(kiroEnv(os.Environ()), key))
	stdin, err := cmd.StdinPipe()
	if err != nil {
		cleanup()
		return nil, nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		cleanup()
		return nil, nil, err
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		cleanup()
		return nil, nil, err
	}

	run := &subscriptionRun{bridge: b, token: token, model: model, cmd: cmd, tmp: tmp, pending: map[string]chan mcpToolResult{},
		patience: devinPatience, late: map[string]chan mcpToolResult{}}
	c := &cursorTurn{run: run}
	run.onCall = c.called
	run.resume = c.resumed
	run.begin = func() Event { return Event{Kind: KStart, MsgID: "msg_" + randomToken()[:24], Model: model} }
	run.timer = time.AfterFunc(30*time.Minute, run.abort)
	segment := run.attach()
	run.emit(run.begin())
	b.mu.Lock()
	b.runs[token] = run
	b.mu.Unlock()

	if err := cmd.Start(); err != nil {
		b.removeRun(run)
		return nil, nil, err
	}
	go func() {
		_, _ = io.Copy(&lockedWriter{run: run}, io.LimitReader(stderr, 1<<20))
	}()
	st := &kiroState{ready: make(chan struct{})}
	names := map[string]bool{}
	for _, t := range tools {
		names[t.Name] = true
	}
	conn := &devinConn{stdin: stdin, pending: map[int64]chan devinReply{}, run: run,
		permit: func(params json.RawMessage) any { return kiroPermit(params, names) },
		notify: st.see}
	go conn.read(stdout)
	exited := make(chan struct{})
	go func() {
		_ = cmd.Wait()
		close(exited)
		run.finish()
	}()

	shake, cancel := context.WithTimeout(ctx, kiroHandshake)
	defer cancel()
	fail := func(what string, err error) (*subscriptionRun, <-chan Event, error) {
		// the CLI says on stderr why it quit before answering: signed out
		// is the one worth putting in words of magpie's
		select {
		case <-exited:
		case <-time.After(time.Second):
		}
		run.mu.Lock()
		said := strings.TrimSpace(run.stderr.String())
		run.mu.Unlock()
		run.abort()
		if strings.Contains(strings.ToLower(said), "not logged in") || !provider.KiroSignedIn(key) {
			return nil, nil, errKiroSignedOut
		}
		if err == nil {
			return nil, nil, errors.New(what)
		}
		if said != "" && errors.Is(err, errACPEnded) {
			return nil, nil, fmt.Errorf("%s: %s", what, lastLine(said))
		}
		return nil, nil, fmt.Errorf("%s: %w", what, err)
	}
	if _, err := conn.call(shake, "initialize", map[string]any{
		"protocolVersion":    1,
		"clientInfo":         map[string]any{"name": "magpie", "version": "0"},
		"clientCapabilities": map[string]any{},
	}); err != nil {
		return fail("kiro acp initialize", err)
	}
	// the magpie server is named in the agent file, where its tools are
	// the agent's only ones, rather than here
	res, err := conn.call(shake, "session/new", map[string]any{"cwd": ws, "mcpServers": []any{}})
	if err != nil {
		return fail("kiro acp session/new", err)
	}
	var sess struct {
		SessionID string `json:"sessionId"`
	}
	if json.Unmarshal(res, &sess) != nil || sess.SessionID == "" {
		return fail("kiro acp session/new: no session id", nil)
	}
	if model != "" {
		if _, err := conn.call(shake, "session/set_model", map[string]any{"sessionId": sess.SessionID, "modelId": model}); err != nil {
			return fail("Kiro can't use "+model, err)
		}
	}
	if len(tools) > 0 {
		// prompting before the server is up would leave the model without
		// the caller's tools; Kiro says when it is, or why it isn't
		select {
		case <-st.ready:
		case <-time.After(kiroMCPWait):
		case <-shake.Done():
		}
		if why := st.failed(); why != "" {
			run.abort()
			return nil, nil, errors.New(why)
		}
	}

	prompt, err := renderACPPrompt(req, len(tools) > 0, "Kiro")
	if err != nil {
		return fail("kiro", err)
	}
	_, reply, err := conn.send("session/prompt", map[string]any{"sessionId": sess.SessionID, "prompt": prompt})
	if err != nil {
		return fail("kiro acp session/prompt", err)
	}
	go func() {
		res, err := conn.await(context.Background(), reply)
		if err != nil {
			msg := err.Error()
			if limited := st.rateLimited(); limited != "" {
				msg = "rate limited: " + limited
			} else if !provider.KiroSignedIn(key) {
				// signed out, kiro-cli-chat still starts a session, and
				// its prompt fails with only "dispatch failure"
				msg = errKiroSignedOut.Error()
			}
			run.emit(Event{Kind: KError, Text: "kiro: " + msg})
		} else {
			var done struct {
				StopReason string `json:"stopReason"`
				Usage      struct {
					Input     int `json:"inputTokens"`
					Output    int `json:"outputTokens"`
					CacheRead int `json:"cachedReadTokens"`
				} `json:"usage"`
			}
			_ = json.Unmarshal(res, &done)
			run.emit(Event{Kind: KUsage, Usage: Usage{Input: done.Usage.Input, Output: done.Usage.Output, CacheRead: done.Usage.CacheRead}})
			run.emit(Event{Kind: KStop, Stop: stopFromACP(done.StopReason)})
		}
		run.endSegment()
		time.AfterFunc(2*time.Second, run.abort)
	}()
	return run, segment, nil
}

// writeKiroAgent writes the agent kiro-cli runs as into the workspace's
// .kiro/agents, where a local agent wins over one of the user's by the same
// name. Its tools are the magpie server's and nothing else — "@magpie" is
// every tool of that server — and they are trusted, so none of them asks
// for permission; the user's own MCP servers (mcp.json), steering and hooks
// are left out.
func writeKiroAgent(ws, exe, callback, toolsPath string, tools bool) error {
	dir := filepath.Join(ws, ".kiro", "agents")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	agent := map[string]any{
		"name":           kiroAgent,
		"description":    "magpie: the caller's tools only",
		"prompt":         "",
		"tools":          []string{},
		"allowedTools":   []string{},
		"includeMcpJson": false,
		"resources":      []string{},
		"hooks":          map[string]any{},
		"mcpServers":     map[string]any{},
	}
	if tools {
		agent["tools"] = []string{"@magpie"}
		agent["allowedTools"] = []string{"@magpie"}
		agent["mcpServers"] = map[string]any{"magpie": map[string]any{
			"command": exe,
			"args":    []string{"claude-mcp-helper", callback, toolsPath},
			"env":     map[string]string{},
		}}
	}
	b, _ := json.MarshalIndent(agent, "", "  ")
	return os.WriteFile(filepath.Join(dir, kiroAgent+".json"), b, 0o600)
}

// kiroEnv drops what a magpie run above this one left in the environment
// for its own helper.
func kiroEnv(env []string) []string {
	out := make([]string, 0, len(env))
	for _, e := range env {
		if strings.HasPrefix(strings.ToUpper(e), "MAGPIE_MCP_") {
			continue
		}
		out = append(out, e)
	}
	return out
}

// kiroPermit answers a permission prompt: a call of one of the caller's
// tools is allowed once, anything else refused. The agent file trusts the
// magpie tools already, so a prompt is only expected for something else.
func kiroPermit(params json.RawMessage, names map[string]bool) any {
	var p struct {
		ToolCall struct {
			Title string `json:"title"`
		} `json:"toolCall"`
		Options []struct {
			OptionID string `json:"optionId"`
			Kind     string `json:"kind"`
		} `json:"options"`
	}
	_ = json.Unmarshal(params, &p)
	want := []string{"reject_once", "reject_always"}
	if kiroBridgeCall(p.ToolCall.Title, names) {
		want = []string{"allow_once", "allow_always"}
	}
	for _, kind := range want {
		for _, o := range p.Options {
			if o.Kind == kind && o.OptionID != "" {
				return map[string]any{"outcome": map[string]any{"outcome": "selected", "optionId": o.OptionID}}
			}
		}
	}
	return map[string]any{"outcome": map[string]any{"outcome": "cancelled"}}
}

// kiroBridgeCall says whether a tool call's title names one of the caller's
// tools, bare or under the magpie server's name.
func kiroBridgeCall(title string, names map[string]bool) bool {
	t := strings.TrimSpace(title)
	for _, prefix := range []string{"@magpie/", "mcp__magpie__", "magpie___", "magpie__", "magpie/"} {
		if rest, ok := strings.CutPrefix(t, prefix); ok {
			t = rest
			break
		}
	}
	return t != "" && names[t]
}

// kiroState is what Kiro's own notifications say about a session: whether
// the magpie server came up, and what went wrong.
type kiroState struct {
	mu      sync.Mutex
	ready   chan struct{}
	once    sync.Once
	why     string
	limited string
}

func (s *kiroState) see(method string, params json.RawMessage) {
	switch method {
	case "_kiro.dev/mcp/server_initialized":
		s.once.Do(func() { close(s.ready) })
	case "_kiro.dev/mcp/server_init_failure":
		s.fail("Kiro couldn't start magpie's tool server: " + kiroText(params))
	case "_kiro.dev/mcp/governance_disabled":
		// an organisation's policy turns MCP off, or Kiro couldn't read
		// the policy — either way the caller's tools can't reach the model
		s.fail("Kiro has MCP servers turned off for this account, so the caller's tools can't be used through it")
	case "_kiro.dev/error/rate_limit":
		s.mu.Lock()
		s.limited = kiroText(params)
		s.mu.Unlock()
	}
}

func (s *kiroState) fail(why string) {
	s.mu.Lock()
	if s.why == "" {
		s.why = why
	}
	s.mu.Unlock()
	s.once.Do(func() { close(s.ready) })
}

func (s *kiroState) failed() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.why
}

func (s *kiroState) rateLimited() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.limited
}

// kiroText is the words in a notification of Kiro's: its message, error or
// reason, else the whole of it.
func kiroText(params json.RawMessage) string {
	var p map[string]any
	if json.Unmarshal(params, &p) == nil {
		for _, k := range []string{"message", "error", "reason", "details"} {
			if s, ok := p[k].(string); ok && s != "" {
				return s
			}
		}
	}
	return strings.TrimSpace(string(params))
}

func lastLine(s string) string {
	s = strings.TrimSpace(s)
	if i := strings.LastIndexByte(s, '\n'); i >= 0 {
		return strings.TrimSpace(s[i+1:])
	}
	return s
}
