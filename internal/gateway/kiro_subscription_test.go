package gateway

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/yetone/magpie/internal/claudebridge"
	"github.com/yetone/magpie/internal/provider"
)

// The test binary stands in for the processes a Kiro run starts: kiro-cli
// itself when MAGPIE_FAKE_KIRO is set, and magpie's MCP helper when started
// as one — so the fake talks to the real helper, which calls back into the
// gateway under test.
func TestMain(m *testing.M) {
	if len(os.Args) > 1 && os.Args[1] == "claude-mcp-helper" {
		if err := claudebridge.RunMCP(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		os.Exit(0)
	}
	if mode := os.Getenv("MAGPIE_FAKE_KIRO"); mode != "" {
		os.Exit(fakeKiro(mode))
	}
	os.Exit(m.Run())
}

// fakeKiro speaks ACP as `kiro-cli acp` does, with the shapes the real
// kiro-cli 2.24.1 answered initialize and session/new with. It starts the
// MCP server the agent file names, as Kiro does, and on a prompt either
// calls the caller's tool through it or answers in streamed chunks naming
// the model it was set to.
func fakeKiro(mode string) int {
	signedOut := strings.HasPrefix(mode, "signedout")
	if slices.Equal(os.Args[1:], []string{"whoami", "-f", "json"}) {
		if signedOut {
			fmt.Println(`{"account":null}`)
			return 1
		}
		fmt.Println(`{"accountType":"BuilderId","email":"t@example.com"}`)
		return 0
	}
	if mode == "signedout" {
		fmt.Fprintln(os.Stderr, "error: You are not logged in, please log in with kiro-cli login")
		return 1
	}
	if !slices.Equal(os.Args[1:], []string{"acp", "--agent", "magpie"}) {
		fmt.Fprintln(os.Stderr, "unexpected args", os.Args[1:])
		return 2
	}
	var agent struct {
		Tools          []string `json:"tools"`
		AllowedTools   []string `json:"allowedTools"`
		IncludeMcpJSON bool     `json:"includeMcpJson"`
		MCPServers     map[string]struct {
			Command string   `json:"command"`
			Args    []string `json:"args"`
		} `json:"mcpServers"`
	}
	b, err := os.ReadFile(filepath.Join(".kiro", "agents", "magpie.json"))
	if err != nil || json.Unmarshal(b, &agent) != nil || agent.IncludeMcpJSON {
		fmt.Fprintln(os.Stderr, "bad agent file", err, string(b))
		return 2
	}

	var wmu sync.Mutex
	write := func(v any) {
		b, _ := json.Marshal(v)
		wmu.Lock()
		os.Stdout.Write(append(b, '\n'))
		wmu.Unlock()
	}
	reply := func(id json.RawMessage, result any) {
		write(map[string]any{"jsonrpc": "2.0", "id": id, "result": result})
	}
	var amu sync.Mutex
	answers := map[string]chan json.RawMessage{}
	ask := func(id, method string, params any) json.RawMessage {
		ch := make(chan json.RawMessage, 1)
		amu.Lock()
		answers[id] = ch
		amu.Unlock()
		write(map[string]any{"jsonrpc": "2.0", "id": id, "method": method, "params": params})
		return <-ch
	}
	chunk := func(kind, text string) {
		write(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
			"sessionId": "sess-1", "update": map[string]any{"sessionUpdate": kind, "content": map[string]any{"type": "text", "text": text}}}})
	}

	var mcp *fakeMCP
	model := "auto"
	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 64<<10), 64<<20)
	for in.Scan() {
		var msg struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Params json.RawMessage `json:"params"`
			Result json.RawMessage `json:"result"`
		}
		if json.Unmarshal(in.Bytes(), &msg) != nil {
			continue
		}
		if msg.Method == "" {
			var id string
			_ = json.Unmarshal(msg.ID, &id)
			amu.Lock()
			ch := answers[id]
			amu.Unlock()
			if ch != nil {
				ch <- msg.Result
			}
			continue
		}
		switch msg.Method {
		case "initialize":
			reply(msg.ID, json.RawMessage(`{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true,"audio":false,"embeddedContext":false},"mcpCapabilities":{"http":true,"sse":false},"sessionCapabilities":{},"auth":{}},"authMethods":[],"agentInfo":{"name":"Kiro CLI Agent","title":"Kiro CLI Agent","version":"2.24.1"}}`))
		case "session/new":
			if srv, ok := agent.MCPServers["magpie"]; ok {
				if !slices.Equal(agent.Tools, []string{"@magpie"}) || !slices.Equal(agent.AllowedTools, []string{"@magpie"}) {
					write(map[string]any{"jsonrpc": "2.0", "id": msg.ID, "error": map[string]any{"code": -32603, "message": "Internal error", "data": "agent has more than magpie's tools"}})
					continue
				}
				if mcp, err = startFakeMCP(srv.Command, srv.Args); err != nil {
					fmt.Fprintln(os.Stderr, err)
					return 2
				}
			} else if len(agent.Tools) > 0 {
				fmt.Fprintln(os.Stderr, "tools without a server", agent.Tools)
				return 2
			}
			reply(msg.ID, json.RawMessage(`{"sessionId":"sess-1","modes":{"currentModeId":"magpie","availableModes":[{"id":"magpie","name":"magpie","description":"magpie"}]}}`))
			write(map[string]any{"jsonrpc": "2.0", "method": "_kiro.dev/metadata", "params": map[string]any{"sessionId": "sess-1", "contextUsagePercentage": 0}})
			if mcp != nil {
				write(map[string]any{"jsonrpc": "2.0", "method": "_kiro.dev/mcp/server_initialized", "params": map[string]any{"sessionId": "sess-1", "serverName": "magpie"}})
			}
		case "session/set_model":
			var p struct {
				ModelID string `json:"modelId"`
			}
			_ = json.Unmarshal(msg.Params, &p)
			model = p.ModelID
			reply(msg.ID, map[string]any{})
		case "session/prompt":
			id := msg.ID
			go func() {
				if signedOut {
					// what kiro-cli-chat does when started signed out
					write(map[string]any{"jsonrpc": "2.0", "id": id, "error": map[string]any{"code": -32603, "message": "Internal error", "data": "Encountered an error in the response stream: An unknown error occurred: dispatch failure"}})
					return
				}
				if mcp == nil {
					chunk("agent_thought_chunk", "thinking")
					for _, t := range []string{"hello ", "from ", "kiro", " [model=" + model + "]"} {
						chunk("agent_message_chunk", t)
					}
					reply(id, map[string]any{"stopReason": "end_turn"})
					return
				}
				// Kiro asks before a tool it doesn't trust: the gateway
				// refuses one of its own and allows the caller's
				perm := func(n, title string) string {
					res := ask(n, "session/request_permission", map[string]any{"sessionId": "sess-1",
						"toolCall": map[string]any{"toolCallId": n, "title": title},
						"options":  []map[string]any{{"optionId": "yes", "name": "Yes", "kind": "allow_once"}, {"optionId": "no", "name": "No", "kind": "reject_once"}}})
					var r struct {
						Outcome struct{ Outcome, OptionID string }
					}
					_ = json.Unmarshal(res, &r)
					return r.Outcome.Outcome + ":" + r.Outcome.OptionID
				}
				if got := perm("p1", "execute_bash"); got != "selected:no" {
					chunk("agent_message_chunk", "shell was allowed: "+got)
				}
				if got := perm("p2", "get_weather"); got != "selected:yes" {
					chunk("agent_message_chunk", "tool was refused: "+got)
				}
				out, err := mcp.call("tools/call", map[string]any{"name": "get_weather", "arguments": map[string]any{"city": "Paris"}})
				if err != nil {
					write(map[string]any{"jsonrpc": "2.0", "id": id, "error": map[string]any{"code": -32603, "message": "Internal error", "data": err.Error()}})
					return
				}
				var res struct {
					Content []struct{ Text string } `json:"content"`
				}
				_ = json.Unmarshal(out, &res)
				text := ""
				for _, c := range res.Content {
					text += c.Text
				}
				chunk("agent_message_chunk", "weather: ")
				chunk("agent_message_chunk", text)
				reply(id, map[string]any{"stopReason": "end_turn"})
			}()
		default:
			write(map[string]any{"jsonrpc": "2.0", "id": msg.ID, "error": map[string]any{"code": -32601, "message": "Method not found"}})
		}
	}
	return 0
}

// fakeMCP is Kiro's side of the MCP server the agent file names.
type fakeMCP struct {
	mu  sync.Mutex
	in  io.Writer
	out *bufio.Scanner
	seq int
}

func startFakeMCP(command string, args []string) (*fakeMCP, error) {
	cmd := exec.Command(command, args...)
	cmd.Stderr = os.Stderr
	in, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	out, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	m := &fakeMCP{in: in, out: bufio.NewScanner(out)}
	m.out.Buffer(make([]byte, 64<<10), 16<<20)
	if _, err := m.call("initialize", map[string]any{"protocolVersion": "2025-06-18", "capabilities": map[string]any{}, "clientInfo": map[string]any{"name": "kiro", "version": "0"}}); err != nil {
		return nil, err
	}
	res, err := m.call("tools/list", map[string]any{})
	if err != nil {
		return nil, err
	}
	if !strings.Contains(string(res), `"get_weather"`) {
		return nil, fmt.Errorf("tools/list: %s", res)
	}
	return m, nil
}

func (m *fakeMCP) call(method string, params any) (json.RawMessage, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.seq++
	b, _ := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": m.seq, "method": method, "params": params})
	if _, err := m.in.Write(append(b, '\n')); err != nil {
		return nil, err
	}
	for m.out.Scan() {
		var r struct {
			ID     int             `json:"id"`
			Result json.RawMessage `json:"result"`
			Error  *struct {
				Message string `json:"message"`
			} `json:"error"`
		}
		if json.Unmarshal(m.out.Bytes(), &r) != nil || r.ID != m.seq {
			continue
		}
		if r.Error != nil {
			return nil, fmt.Errorf("%s: %s", method, r.Error.Message)
		}
		return r.Result, nil
	}
	return nil, fmt.Errorf("%s: the helper ended", method)
}

// kiroServer is a gateway whose Kiro is the fake: the MCP helper's
// callbacks reach it at MAGPIE_ADDR.
func kiroServer(t *testing.T, mode string) *Server {
	t.Helper()
	t.Setenv("HOME", t.TempDir())
	t.Setenv("XDG_CONFIG_HOME", t.TempDir())
	t.Setenv("XDG_CACHE_HOME", t.TempDir())
	t.Setenv("MAGPIE_FAKE_KIRO", mode)
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	was := provider.KiroExecutable
	provider.KiroExecutable = func() string { return exe }
	t.Cleanup(func() { provider.KiroExecutable = was })
	s := New()
	srv := httptest.NewServer(s.Handler())
	t.Cleanup(srv.Close)
	t.Setenv("MAGPIE_ADDR", strings.TrimPrefix(srv.URL, "http://"))
	return s
}

var kiroProvider = provider.Provider{ID: "kiro", Name: "Kiro", Account: &provider.Account{Agent: "kiro", User: "u"}}

func askKiro(t *testing.T, s *Server, model, body string) (int, string) {
	t.Helper()
	rec := httptest.NewRecorder()
	var call Call
	code, msg := s.attempt(rec, httptest.NewRequest("POST", "/v1/messages", strings.NewReader(body)), provider.Anthropic, kiroProvider, model, []byte(body), &call)
	if code != 200 {
		return code, msg
	}
	return code, rec.Body.String()
}

func TestKiroTextTurn(t *testing.T) {
	s := kiroServer(t, "on")
	code, out := askKiro(t, s, "auto", `{"model":"auto","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}`)
	if code != 200 {
		t.Fatalf("%d %s", code, out)
	}
	var res struct {
		Content []struct {
			Type     string
			Text     string
			Thinking string
		}
		StopReason string `json:"stop_reason"`
	}
	if err := json.Unmarshal([]byte(out), &res); err != nil {
		t.Fatal(err, out)
	}
	var text, think string
	for _, c := range res.Content {
		text += c.Text
		think += c.Thinking
	}
	if text != "hello from kiro [model=auto]" || think != "thinking" || res.StopReason != "end_turn" {
		t.Fatalf("text=%q think=%q stop=%q in %s", text, think, res.StopReason, out)
	}
}

func TestKiroStreamsChunks(t *testing.T) {
	s := kiroServer(t, "on")
	code, out := askKiro(t, s, "auto", `{"model":"auto","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}`)
	if code != 200 {
		t.Fatalf("%d %s", code, out)
	}
	var deltas []string
	for _, line := range strings.Split(out, "\n") {
		data, ok := strings.CutPrefix(line, "data: ")
		if !ok {
			continue
		}
		var ev struct {
			Type  string
			Delta struct{ Type, Text string }
		}
		if json.Unmarshal([]byte(data), &ev) == nil && ev.Delta.Type == "text_delta" {
			deltas = append(deltas, ev.Delta.Text)
		}
	}
	if !slices.Equal(deltas, []string{"hello ", "from ", "kiro", " [model=auto]"}) {
		t.Fatalf("deltas = %q in\n%s", deltas, out)
	}
	if !strings.Contains(out, "thinking_delta") || !strings.Contains(out, "message_stop") {
		t.Fatalf("stream lacks thinking or its end:\n%s", out)
	}
}

func TestKiroSetsTheModel(t *testing.T) {
	s := kiroServer(t, "on")
	code, out := askKiro(t, s, "claude-sonnet-4.5", `{"model":"claude-sonnet-4.5","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}`)
	if code != 200 || !strings.Contains(out, "[model=claude-sonnet-4.5]") {
		t.Fatalf("%d %s", code, out)
	}
}

// A tool call goes out through the MCP helper as the caller's tool_use; the
// caller's next request brings the result back to the same Kiro, which goes
// on with it.
func TestKiroToolRoundTrip(t *testing.T) {
	s := kiroServer(t, "on")
	tools := `"tools":[{"name":"get_weather","description":"Weather for a city","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]`
	first := `{"model":"auto","max_tokens":100,` + tools + `,"messages":[{"role":"user","content":"weather in Paris?"}]}`
	code, out := askKiro(t, s, "auto", first)
	if code != 200 {
		t.Fatalf("%d %s", code, out)
	}
	var res struct {
		Content []struct {
			Type  string
			ID    string
			Name  string
			Text  string
			Input map[string]any
		}
		StopReason string `json:"stop_reason"`
	}
	if err := json.Unmarshal([]byte(out), &res); err != nil {
		t.Fatal(err, out)
	}
	var use struct {
		ID, Name string
		Input    map[string]any
	}
	for _, c := range res.Content {
		if c.Type == "tool_use" {
			use.ID, use.Name, use.Input = c.ID, c.Name, c.Input
		}
		if c.Type == "text" && c.Text != "" {
			t.Fatalf("said %q before the tool (a permission went the wrong way?)", c.Text)
		}
	}
	if use.Name != "get_weather" || use.Input["city"] != "Paris" || res.StopReason != "tool_use" {
		t.Fatalf("no tool call: %s", out)
	}
	input, _ := json.Marshal(use.Input)
	second := `{"model":"auto","max_tokens":100,` + tools + `,"messages":[` +
		`{"role":"user","content":"weather in Paris?"},` +
		`{"role":"assistant","content":[{"type":"tool_use","id":"` + use.ID + `","name":"get_weather","input":` + string(input) + `}]},` +
		`{"role":"user","content":[{"type":"tool_result","tool_use_id":"` + use.ID + `","content":"sunny"}]}]}`
	code, out = askKiro(t, s, "auto", second)
	if code != 200 {
		t.Fatalf("%d %s", code, out)
	}
	var fin struct {
		Content []struct{ Text string }
	}
	_ = json.Unmarshal([]byte(out), &fin)
	text := ""
	for _, c := range fin.Content {
		text += c.Text
	}
	if text != "weather: sunny" {
		t.Fatalf("after the result: %q in %s", text, out)
	}
}

func TestKiroNotInstalled(t *testing.T) {
	was := provider.KiroExecutable
	provider.KiroExecutable = func() string { return "" }
	t.Cleanup(func() { provider.KiroExecutable = was })
	b := newSubscriptionBridge()
	_, _, err := b.startKiro(t.Context(), &Request{Messages: []Message{{Role: "user", Parts: []Part{{Kind: Text, Text: "hi"}}}}}, "auto", "")
	if err == nil || !strings.Contains(err.Error(), "install kiro-cli from https://kiro.dev/cli") {
		t.Fatalf("err = %v", err)
	}
}

// Signed out, the kiro-cli launcher refuses to start; kiro-cli-chat starts
// and fails on the prompt with nothing about signing in. Either way the
// caller is told to sign in.
func TestKiroSignedOut(t *testing.T) {
	for _, mode := range []string{"signedout", "signedout-chat"} {
		s := kiroServer(t, mode)
		code, msg := askKiro(t, s, "auto", `{"model":"auto","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}`)
		if code != 502 || !strings.Contains(msg, "not signed in") || !strings.Contains(msg, "kiro-cli login") {
			t.Fatalf("%s: %d %s", mode, code, msg)
		}
	}
}

func TestKiroPermit(t *testing.T) {
	names := map[string]bool{"get_weather": true, waitTool: true}
	opts := `"options":[{"optionId":"a","kind":"allow_once"},{"optionId":"aa","kind":"allow_always"},{"optionId":"r","kind":"reject_once"}]`
	for title, want := range map[string]string{
		"get_weather":          "a",
		"@magpie/get_weather":  "a",
		"magpie___" + waitTool: "a",
		"execute_bash":         "r",
		"fs_write":             "r",
		"":                     "r",
		"other/get_weather":    "r",
	} {
		got, _ := json.Marshal(kiroPermit(json.RawMessage(`{"toolCall":{"title":"`+title+`"},`+opts+`}`), names))
		if !strings.Contains(string(got), `"optionId":"`+want+`"`) {
			t.Errorf("%q: %s", title, got)
		}
	}
	// no reject option to pick: the prompt is cancelled, never allowed
	got, _ := json.Marshal(kiroPermit(json.RawMessage(`{"toolCall":{"title":"execute_bash"},"options":[{"optionId":"a","kind":"allow_once"}]}`), names))
	if !strings.Contains(string(got), `"cancelled"`) {
		t.Errorf("no reject option: %s", got)
	}
}

func TestKiroAgentFileHasOnlyTheCallersTools(t *testing.T) {
	ws := filepath.Join(t.TempDir(), "ws")
	if err := writeKiroAgent(ws, "/bin/magpie", "http://127.0.0.1:1/cb", "/tmp/tools.json", true); err != nil {
		t.Fatal(err)
	}
	b, _ := os.ReadFile(filepath.Join(ws, ".kiro", "agents", "magpie.json"))
	var a map[string]any
	if err := json.Unmarshal(b, &a); err != nil {
		t.Fatal(err)
	}
	if fmt.Sprint(a["tools"]) != "[@magpie]" || fmt.Sprint(a["allowedTools"]) != "[@magpie]" || a["includeMcpJson"] != false || a["name"] != "magpie" {
		t.Fatalf("agent = %s", b)
	}
	if !strings.Contains(string(b), `"claude-mcp-helper"`) {
		t.Fatalf("no helper: %s", b)
	}
	if err := writeKiroAgent(ws, "/bin/magpie", "cb", "t", false); err != nil {
		t.Fatal(err)
	}
	b, _ = os.ReadFile(filepath.Join(ws, ".kiro", "agents", "magpie.json"))
	if strings.Contains(string(b), "claude-mcp-helper") || !strings.Contains(string(b), `"tools": []`) {
		t.Fatalf("no tools, yet: %s", b)
	}
}

func TestACPErrorCarriesData(t *testing.T) {
	var e devinRPCError
	_ = json.Unmarshal([]byte(`{"code":-32603,"message":"Internal error","data":"Encountered an error in the response stream: The bearer token included in the request is invalid."}`), &e)
	if got := e.Error(); got != "Internal error: Encountered an error in the response stream: The bearer token included in the request is invalid." {
		t.Fatalf("got %q", got)
	}
	var bare devinRPCError
	_ = json.Unmarshal([]byte(`{"code":-32601,"message":"Method not found"}`), &bare)
	if bare.Error() != "Method not found" {
		t.Fatal(bare.Error())
	}
}

func TestKiroEnvDropsHelperLeftovers(t *testing.T) {
	got := strings.Join(provider.KiroEnv(kiroEnv([]string{"PATH=/bin", "MAGPIE_MCP_CALLBACK=x", "KIRO_API_KEY=mine", "KEEP=1"}), "saved"), "\n")
	if strings.Contains(got, "MAGPIE_MCP_") || strings.Contains(got, "KIRO_API_KEY=mine") || !strings.Contains(got, "KIRO_API_KEY=saved") || !strings.Contains(got, "KEEP=1") {
		t.Fatalf("env = %q", got)
	}
}
