package gateway

// A Grok subscription runs through xAI's Grok Build CLI, the way a Cursor one
// runs through cursor-agent (cursor_subscription.go): the caller's tools are
// an MCP server whose calls park until the caller sends results, and each
// tool_use goes out when the helper calls back — grok reaches MCP tools
// through its own search_tool and use_tool, so its stream never names them.
//
// It runs in a home of magpie's own, where the user's hooks, skills, rules
// and MCP servers (grok's and those it borrows from Claude and Cursor) are
// not. The sign-in is borrowed without being shared: grok there takes its
// token from `magpie grok-token`, which reads the user's own grok home and
// leaves refreshing it to the user's grok.

import (
	"bufio"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/netproxy"
	"github.com/yetone/magpie/internal/proc"
	"github.com/yetone/magpie/internal/provider"
)

// startGrok runs grok for the account signed in in userHome, the CLI's own
// when it is "".
func (b *subscriptionBridge) startGrok(ctx context.Context, req *Request, model, userHome string) (*subscriptionRun, <-chan Event, error) {
	binary := provider.GrokExecutable()
	if binary == "" {
		return nil, nil, errors.New("Grok Build is not installed; install it with `curl -fsSL https://x.ai/cli/install.sh | bash` and run `grok login`")
	}
	exe, err := os.Executable()
	if err != nil {
		return nil, nil, err
	}
	if userHome == "" {
		userHome = provider.GrokHome()
	}
	auth := grokAuthCommand(exe, userHome, binary)
	home, err := grokHome(exe, binary, auth, userHome)
	if err != nil {
		return nil, nil, err
	}
	tmp, err := os.MkdirTemp("", "magpie-grok-")
	if err != nil {
		return nil, nil, err
	}
	cleanup := func() { _ = os.RemoveAll(tmp) }

	tools := bridgeTools(req)
	toolsPath := filepath.Join(tmp, "tools.json")
	toolBytes, _ := json.Marshal(tools)
	if err := os.WriteFile(toolsPath, toolBytes, 0o600); err != nil {
		cleanup()
		return nil, nil, err
	}
	promptPath := filepath.Join(tmp, "prompt.txt")
	if err := os.WriteFile(promptPath, []byte(renderGrokPrompt(req, tools)), 0o600); err != nil {
		cleanup()
		return nil, nil, err
	}
	ws := filepath.Join(tmp, "workspace")
	if err := os.MkdirAll(ws, 0o700); err != nil {
		cleanup()
		return nil, nil, err
	}
	token := randomToken()
	callback := callbackBaseURL() + "/_magpie/claude-mcp/" + token

	// only the ways to the magpie server's tools: no shell, files or web
	args := []string{"--prompt-file", promptPath, "--output-format", "streaming-messages-json", "--include-partial-messages",
		"-m", model, "--tools", "search_tool,use_tool", "--disable-web-search", "--no-subagents", "--no-plan"}
	cmd := proc.CommandContext(context.Background(), binary, args...)
	cmd.Dir = ws
	cmd.Env = netproxy.Env(append(grokEnv(os.Environ(), home, auth), "MAGPIE_MCP_CALLBACK="+callback, "MAGPIE_MCP_TOOLS="+toolsPath))
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

	run := &subscriptionRun{bridge: b, token: token, model: model, cmd: cmd, tmp: tmp, pending: map[string]chan mcpToolResult{}}
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
	go c.readGrok(stdout)
	go func() {
		_ = cmd.Wait()
		run.finish()
	}()
	return run, segment, nil
}

// grokAuthCommand is how grok in magpie's home asks for the user's token.
func grokAuthCommand(exe, userHome, binary string) string {
	return shellQuote(exe) + " grok-token " + shellQuote(userHome) + " " + shellQuote(binary)
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

var grokHomeMu sync.Mutex

// grokHome is the home grok runs in: magpie's, kept between runs so its
// caches are, with a config of magpie's and a sign-in through auth. Each
// account has its own, so none signs in again for another.
func grokHome(exe, binary, auth, userHome string) (string, error) {
	grokHomeMu.Lock()
	defer grokHomeMu.Unlock()
	base, err := os.UserCacheDir()
	if err != nil {
		base = os.TempDir()
	}
	home := filepath.Join(base, "magpie", "grok-home")
	if userHome != provider.GrokHome() {
		sum := sha256.Sum256([]byte(userHome))
		home += "-" + hex.EncodeToString(sum[:6])
	}
	dir := filepath.Join(home, ".grok")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	cfg := grokConfig(exe)
	path := filepath.Join(dir, "config.toml")
	if cur, err := os.ReadFile(path); err != nil || !strings.HasPrefix(string(cur), cfg) {
		if err := os.WriteFile(path, []byte(cfg), 0o600); err != nil {
			return "", err
		}
	}
	// grok signs in to an external provider once; the account it keeps
	// must be the one the user's grok is signed in to
	user, ok := provider.GrokUser(userHome)
	if !ok {
		return "", errors.New("Grok is not signed in; run `grok login`")
	}
	if cur, ok := provider.GrokUser(dir); ok && strings.EqualFold(cur, user) {
		return home, nil
	}
	_ = os.Remove(filepath.Join(dir, "auth.json"))
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	cmd := proc.CommandContext(ctx, binary, "login")
	cmd.Dir = home
	cmd.Env = netproxy.Env(grokEnv(os.Environ(), home, auth))
	out, err := cmd.CombinedOutput()
	if _, ok := provider.GrokUser(dir); !ok {
		msg := strings.TrimSpace(string(out))
		if i := strings.LastIndex(msg, "\n"); i >= 0 {
			msg = msg[i+1:]
		}
		if msg == "" && err != nil {
			msg = err.Error()
		}
		return "", fmt.Errorf("grok couldn't sign in: %s", msg)
	}
	return home, nil
}

// grokConfig turns off all grok would load that is the user's rather than
// the caller's, and has the magpie server's calls wait as long as the
// caller takes.
func grokConfig(exe string) string {
	var b strings.Builder
	b.WriteString("# written by magpie for the Grok runs behind its gateway\n")
	b.WriteString("[cli]\nauto_update = false\n\n")
	b.WriteString("[marketplace]\ndefault_skills_installs_purged = true\nofficial_marketplace_auto_installed = true\n\n")
	for _, vendor := range []string{"claude", "cursor"} {
		fmt.Fprintf(&b, "[compat.%s]\nmcps = false\nskills = false\nhooks = false\nrules = false\nagents = false\n\n", vendor)
	}
	b.WriteString("[compat.codex]\nskills = false\n\n")
	fmt.Fprintf(&b, "[mcp_servers.magpie]\ncommand = %s\nargs = [\"claude-mcp-helper\"]\nstartup_timeout_sec = 30\ntool_timeout_sec = 86400\n\n", strconv.Quote(exe))
	b.WriteString("[mcp]\nmax_output_bytes = 4194304\n\n")
	b.WriteString("[permission]\nallow = [\"MCPTool(magpie__*)\"]\n")
	return b.String()
}

func grokEnv(env []string, home, auth string) []string {
	out := make([]string, 0, len(env)+4)
	for _, e := range env {
		k, _, _ := strings.Cut(e, "=")
		k = strings.ToUpper(k)
		if k == "HOME" || k == "USERPROFILE" || k == "XAI_API_KEY" || strings.HasPrefix(k, "GROK_") || strings.HasPrefix(k, "MAGPIE_MCP_") {
			continue
		}
		out = append(out, e)
	}
	out = append(out, "HOME="+home, "GROK_AUTH_PROVIDER_COMMAND="+auth, "GROK_AUTH_EARLY_INVALIDATION_SECS=60")
	if runtime.GOOS == "windows" {
		out = append(out, "USERPROFILE="+home)
	}
	return out
}

// renderGrokPrompt is the conversation as one text, with the caller's
// tools named the way grok calls them.
func renderGrokPrompt(req *Request, tools []bridgeTool) string {
	blocks, _ := renderClaudePrompt(req)
	var b strings.Builder
	if len(tools) > 0 {
		names := make([]string, len(tools))
		for i, t := range tools {
			names[i] = "magpie__" + t.Name
		}
		b.WriteString("<external_system_instructions>\nThe only tools in this session are those of the magpie MCP server; Grok's own tools (shell, reading, editing and searching files, the web) are turned off, and this workspace is empty. Call them with use_tool, by these names: " + strings.Join(names, ", ") + ". Call one whenever the conversation needs it.\n</external_system_instructions>\n\n")
	}
	for _, bl := range blocks {
		if t, ok := bl["text"].(string); ok {
			b.WriteString(t)
		}
	}
	return b.String()
}

// readGrok follows grok's stream: Claude's stream events, then the whole
// message again, then the result.
func (c *cursorTurn) readGrok(rd io.Reader) {
	s := bufio.NewScanner(rd)
	s.Buffer(make([]byte, 64<<10), 64<<20)
	var thought, said bool // this message's thinking and text came in pieces
	for s.Scan() {
		var e struct {
			Type    string          `json:"type"`
			Parent  *string         `json:"parent_tool_use_id"`
			IsError bool            `json:"is_error"`
			Result  string          `json:"result"`
			Errors  []string        `json:"errors"`
			Event   json.RawMessage `json:"event"`
			Message struct {
				Content []struct {
					Type     string `json:"type"`
					Text     string `json:"text"`
					Thinking string `json:"thinking"`
				} `json:"content"`
			} `json:"message"`
			Usage grokUsage `json:"usage"`
		}
		if json.Unmarshal(s.Bytes(), &e) != nil || (e.Parent != nil && *e.Parent != "") {
			continue
		}
		switch e.Type {
		case "stream_event":
			var ev struct {
				Type  string `json:"type"`
				Delta struct {
					Type     string `json:"type"`
					Text     string `json:"text"`
					Thinking string `json:"thinking"`
				} `json:"delta"`
			}
			if json.Unmarshal(e.Event, &ev) != nil {
				continue
			}
			switch {
			case ev.Type == "message_start":
				thought, said = false, false
			case ev.Delta.Type == "thinking_delta" && ev.Delta.Thinking != "":
				thought = true
				c.run.emit(Event{Kind: KThink, Text: ev.Delta.Thinking})
			case ev.Delta.Type == "text_delta" && ev.Delta.Text != "":
				said = true
				c.run.emit(Event{Kind: KText, Text: ev.Delta.Text})
			}
		case "assistant":
			// what came whole and not in pieces
			for _, p := range e.Message.Content {
				if p.Type == "thinking" && p.Thinking != "" && !thought {
					c.run.emit(Event{Kind: KThink, Text: p.Thinking})
				}
				if p.Type == "text" && p.Text != "" && !said {
					c.run.emit(Event{Kind: KText, Text: p.Text})
				}
			}
		case "result":
			c.mu.Lock()
			c.done = true
			c.mu.Unlock()
			if e.IsError {
				msg := strings.Join(e.Errors, "; ")
				if msg == "" {
					msg = e.Result
				}
				if msg == "" {
					msg = c.run.lastError("grok ended with an error")
				}
				c.run.emit(Event{Kind: KError, Text: msg})
			} else {
				c.run.emit(Event{Kind: KUsage, Usage: e.Usage.usage()})
				c.run.emit(Event{Kind: KStop, Stop: "stop"})
			}
			c.run.endSegment()
			time.AfterFunc(2*time.Second, c.run.abort)
		}
	}
	c.mu.Lock()
	done := c.done
	c.mu.Unlock()
	if !done {
		c.run.emit(Event{Kind: KError, Text: c.run.lastError("grok ended without an answer")})
	}
}

// grokUsage is the Messages API's usage, whose input leaves out the cache.
type grokUsage struct {
	Input      int `json:"input_tokens"`
	Output     int `json:"output_tokens"`
	CacheRead  int `json:"cache_read_input_tokens"`
	CacheWrite int `json:"cache_creation_input_tokens"`
}

func (u grokUsage) usage() Usage {
	return Usage{Input: u.Input, Output: u.Output, CacheRead: u.CacheRead, CacheWrite: u.CacheWrite}
}
