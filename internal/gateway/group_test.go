package gateway

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/yetone/magpie/internal/provider"
)

// keyed is a vendor that says which key each request came with, and
// answers as told for each.
type keyed struct {
	tried []string
	fail  map[string]int // key → status to fail with
}

func (k *keyed) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	io.ReadAll(r.Body)
	key := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	k.tried = append(k.tried, key)
	w.Header().Set("Content-Type", "application/json")
	if c := k.fail[key]; c != 0 {
		w.WriteHeader(c)
		io.WriteString(w, `{"error":{"message":"slow down"}}`)
		return
	}
	io.WriteString(w, `{"id":"x","choices":[{"index":0,"message":{"role":"assistant","content":"from `+key+`"},"finish_reason":"stop"}],`+
		`"usage":{"prompt_tokens":3000,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":2500}}}`)
}

func fresh(t *testing.T) {
	t.Helper()
	t.Setenv("HOME", t.TempDir()) // no agent signed in
	t.Setenv("XDG_CONFIG_HOME", t.TempDir())
	t.Setenv("XDG_CACHE_HOME", t.TempDir())
	restingUntil.Lock()
	restingUntil.m = map[string]time.Time{}
	restingUntil.Unlock()
	sticks.Lock()
	sticks.m = map[string]stick{}
	sticks.Unlock()
	turnRules.Lock()
	turnRules.m = map[string]turnRule{}
	turnRules.Unlock()
	classified.Lock()
	classified.m, classified.failed = map[string]classifiedAs{}, map[string]classifyFailure{}
	classified.Unlock()
}

func serveOn(t *testing.T, id, key string, models []string, v http.Handler, keys ...string) {
	t.Helper()
	up := httptest.NewServer(v)
	t.Cleanup(up.Close)
	p := provider.Provider{ID: id, Name: strings.ToUpper(id), Key: key, Models: models, Chat: up.URL + "/v1"}
	for _, k := range keys {
		p.Keys = append(p.Keys, provider.KeyAccount{Key: k})
	}
	if err := provider.Save(p); err != nil {
		t.Fatal(err)
	}
}

func postAs(t *testing.T, s *Server, session, body string) (int, string) {
	t.Helper()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body))
	if session != "" {
		req.Header.Set("x-session-id", session)
	}
	s.Handler().ServeHTTP(rec, req)
	return rec.Code, rec.Body.String()
}

// A model two providers serve is a group magpie finds; a group the user
// makes routes over its members in the order it names them, the next one
// taking a request the one before can't.
func TestRoutingGroups(t *testing.T) {
	fresh(t)
	a, b := &keyed{}, &keyed{fail: map[string]int{"kb": 429}}
	serveOn(t, "a", "ka", []string{"m", "only-a"}, a)
	serveOn(t, "b", "kb", []string{"vendor/m"}, b)

	var ids []string
	for _, e := range provider.Catalog() {
		if e.Provider.Account == nil { // signed-in agents the machine may have aside
			ids = append(ids, e.ID)
		}
	}
	if got := strings.Join(ids, " "); got != "group/auto-m a/m a/only-a b/vendor/m" {
		t.Fatalf("catalog: %s", got)
	}
	if err := provider.SaveGroup(provider.Group{Name: "Mine", Members: []string{"b/vendor/m", "a/only-a"}, Routing: provider.Ordered}); err != nil {
		t.Fatal(err)
	}
	if p, m, ok := provider.Resolve("group/mine"); !ok || p.ID != "b" || m != "vendor/m" {
		t.Fatalf("resolve: %v %s %s", ok, p.ID, m)
	}

	s := New()
	code, body := postAs(t, s, "", `{"model":"group/mine","messages":[{"role":"user","content":"hi"}]}`)
	if code != 200 || !strings.Contains(body, "from ka") || strings.Join(b.tried, ",") != "kb" {
		t.Fatalf("group: %d %s, b tried %v", code, body, b.tried)
	}
	r := s.trace.routes[len(s.trace.routes)-1]
	if r.Group == nil || r.Group.ID != "mine" || len(r.Order) != 2 || r.Order[0].Provider != "b" || r.Order[1].Model != "only-a" {
		t.Fatalf("trace: %+v", r)
	}

	// removing one magpie found hides it; showing it brings it back
	if err := provider.DeleteGroup("auto-m"); err != nil {
		t.Fatal(err)
	}
	if _, _, ok := provider.Resolve("group/auto-m"); ok {
		t.Fatal("hidden group still served")
	}
	provider.ShowGroup("auto-m")
	if _, _, ok := provider.Resolve("group/auto-m"); !ok {
		t.Fatal("shown group not served")
	}

	// one serving only through its groups leaves the list, not the groups
	pb, err := provider.Find("b")
	if err != nil {
		t.Fatal(err)
	}
	pb.Unlisted = true
	if err := provider.Save(*pb); err != nil {
		t.Fatal(err)
	}
	for _, e := range provider.Catalog() {
		if e.Provider.ID == "b" && !strings.HasPrefix(e.ID, provider.GroupPrefix) {
			t.Fatalf("unlisted b listed as %s", e.ID)
		}
	}
	if p, m, ok := provider.Resolve("group/mine"); !ok || p.ID != "b" || m != "vendor/m" {
		t.Fatalf("unlisted member: %v %s %s", ok, p.ID, m)
	}
	if _, _, ok := provider.Resolve("b/vendor/m"); !ok {
		t.Fatal("unlisted b not served by its id")
	}
}

// A conversation stays with the key that answered it: within a turn
// always, across turns while the vendor read enough of it from its cache,
// or not at all when its affinity is off — rotating otherwise.
func TestAffinity(t *testing.T) {
	const (
		first = `{"model":"aff/m","messages":[{"role":"user","content":"hi"}]}`
		again = `{"model":"aff/m","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"yo"},{"role":"user","content":"more"}]}`
		tool  = `{"model":"aff/m","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"ls","arguments":"{}"}}]},{"role":"tool","tool_call_id":"c1","content":"a.go"}]}`
	)
	for _, c := range []struct {
		routing  string
		affinity string
		body     string
		want     string // the key the second request goes to
		why      string
	}{
		{"", "", again, "k1", "cache"},
		{provider.Rotate, "", again, "k2", "new-turn"}, // in turn spreads the turns
		{provider.Rotate, "", tool, "k1", "turn"},
		{provider.Rotate, provider.AffinityTurn, again, "k2", "new-turn"},
		{provider.Rotate, provider.AffinityTurn, tool, "k1", "turn"},
		{provider.Rotate, provider.AffinitySession, again, "k1", "session"},
		{provider.Rotate, provider.AffinityOff, again, "k2", "off"},
	} {
		fresh(t)
		v := &keyed{}
		serveOn(t, "aff", "k1", []string{"m"}, v, "k2")
		provider.SetRouting("aff", c.routing)
		provider.SetAffinity("aff", c.affinity)
		routed.Lock()
		delete(routed.turn, "aff")
		routed.Unlock()
		s := New()
		postAs(t, s, "s1", first)
		postAs(t, s, "s1", c.body)
		r := s.trace.routes[len(s.trace.routes)-1]
		if got := v.tried[len(v.tried)-1]; got != c.want || r.Affinity == nil || r.Affinity.Why != c.why {
			t.Errorf("%s %q: went to %s (%+v), want %s by %s", c.routing, c.affinity, got, r.Affinity, c.want, c.why)
		}
	}

	// in turn, each turn goes to the one after the last turn's, whatever
	// came between: the tool results within it, another model's requests
	fresh(t)
	v0 := &keyed{}
	serveOn(t, "aff", "k1", []string{"m", "small"}, v0, "k2", "k3")
	provider.SetRouting("aff", provider.Rotate)
	s0 := New()
	var turns []string
	msgs := `{"role":"user","content":"hi"}`
	for i := range 4 {
		if i > 0 {
			msgs += `,{"role":"assistant","content":"yo"},{"role":"user","content":"more"}`
		}
		postAs(t, s0, "s1", `{"model":"aff/m","messages":[`+msgs+`]}`)
		turns = append(turns, v0.tried[len(v0.tried)-1])
		postAs(t, s0, "s1", `{"model":"aff/small","messages":[{"role":"user","content":"title?"}]}`)
		postAs(t, s0, "s1", `{"model":"aff/m","messages":[`+msgs+`,{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"ls","arguments":"{}"}}]},{"role":"tool","tool_call_id":"c1","content":"a.go"}]}`)
		if got := v0.tried[len(v0.tried)-1]; got != turns[i] {
			t.Fatalf("turn %d's tool result went to %s, not %s", i, got, turns[i])
		}
	}
	for i := 1; i < len(turns); i++ {
		if want := map[string]string{"k1": "k2", "k2": "k3", "k3": "k1"}[turns[i-1]]; turns[i] != want {
			t.Fatalf("turns went %v", turns)
		}
	}

	// another session starts fresh, and one resting is left
	fresh(t)
	v := &keyed{}
	serveOn(t, "aff", "k1", []string{"m"}, v, "k2")
	provider.SetAffinity("aff", provider.AffinitySession)
	s := New()
	postAs(t, s, "s1", first)
	restingUntil.Lock()
	restingUntil.m["aff#"+provider.KeyID("k1")] = time.Now().Add(time.Minute)
	restingUntil.Unlock()
	postAs(t, s, "s1", again)
	if r := s.trace.routes[len(s.trace.routes)-1]; v.tried[1] != "k2" || r.Affinity.Why != "resting" {
		t.Fatalf("resting: %v %+v", v.tried, r.Affinity)
	}
	postAs(t, s, "s2", first)
	if r := s.trace.routes[len(s.trace.routes)-1]; r.Affinity.Why != "first" {
		t.Fatalf("new session: %+v", r.Affinity)
	}
}

// Keys made for different protocols are not one pool: in turn goes round
// the keys made for the protocol that suits the request, and a key made for
// another is tried only after them.
func TestKeyPoolsByProtocol(t *testing.T) {
	fresh(t)
	v := &keyed{}
	up := httptest.NewServer(v)
	t.Cleanup(up.Close)
	p := provider.Provider{ID: "mix", Name: "MIX", Key: "k1", KeyProtocol: provider.Chat, Models: []string{"m"}, Chat: up.URL + "/v1",
		Keys: []provider.KeyAccount{{Key: "k2"}, {Key: "k3", Protocol: provider.Chat}}, Routing: provider.Rotate}
	if err := provider.Save(p); err != nil {
		t.Fatal(err)
	}
	s := New()
	for range 4 {
		postAs(t, s, "", `{"model":"mix/m","messages":[{"role":"user","content":"hi"}]}`)
	}
	if got := strings.Join(v.tried, ","); got != "k1,k3,k1,k3" {
		t.Fatalf("in turn went %s", got)
	}
	r := s.trace.routes[len(s.trace.routes)-1]
	if last := r.Order[len(r.Order)-1]; !last.Aside || last.Who == "" || r.Order[0].Aside {
		t.Fatalf("trace: %+v", r.Order)
	}
	v.tried, v.fail = nil, map[string]int{"k1": 429, "k3": 429}
	code, body := postAs(t, s, "", `{"model":"mix/m","messages":[{"role":"user","content":"hi"}]}`)
	if code != 200 || !strings.Contains(body, "from k2") {
		t.Fatalf("aside key: %d %s, tried %v", code, body, v.tried)
	}
}
