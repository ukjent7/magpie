package gateway

// A rule with an intent (provider.Rule.Intent) is for a kind of message:
// as a user's turn begins, the group's classifier model is asked which of
// the intents the user's message is — once, before the turn goes anywhere,
// and never again within it. The call goes through magpie itself, so the
// classifier is any model magpie has, with its keys, failover and usage;
// it shows in the usage as magpie's own call. When it can't say
// (it fails, is slow, or answers something else) no intent matches, and
// the trace tells why.

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/provider"
)

// RouterAgent is the User-Agent magpie's own classifier calls carry.
const RouterAgent = "magpie-router/1"

var (
	classifyTimeout = 8 * time.Second
	classifyKeep    = 10 * time.Minute // an answer, for the same message and intents
	classifyRest    = 30 * time.Second // after a failure, before the classifier is asked again
)

// classifier asks a model which of intents text is: the intent, or "" for
// none of them.
type classifier func(model string, intents []string, text string) (string, error)

var classified = struct {
	sync.Mutex
	m      map[string]classifiedAs
	failed map[string]classifyFailure // classifier model → its last failure
}{m: map[string]classifiedAs{}, failed: map[string]classifyFailure{}}

type classifiedAs struct {
	intent string
	at     time.Time
}

type classifyFailure struct {
	err string
	at  time.Time
}

// answerError is a classifier that answered, but not with one of the
// numbers: it is up, so it isn't left to rest for it.
type answerError struct{ error }

// classify is ask's answer for the message, from what was asked before
// when it can be: the same message and intents within classifyKeep. A
// classifier that failed is left to rest for classifyRest rather than
// making every turn wait out its timeout.
func classify(ask classifier, model string, intents []string, text string) (intent string, cached bool, err error) {
	h := sha256.New()
	h.Write([]byte(model + "\x00" + strings.ToLower(strings.Join(intents, "\x00")) + "\x00" + text))
	key := hex.EncodeToString(h.Sum(nil))
	now := time.Now()
	classified.Lock()
	if c, ok := classified.m[key]; ok && now.Sub(c.at) < classifyKeep {
		classified.Unlock()
		return c.intent, true, nil
	}
	if f, ok := classified.failed[model]; ok && now.Sub(f.at) < classifyRest {
		classified.Unlock()
		return "", false, fmt.Errorf("%s failed %s ago (%s); not asked again for now", model, now.Sub(f.at).Round(time.Second), f.err)
	}
	classified.Unlock()
	intent, err = ask(model, intents, text)
	classified.Lock()
	defer classified.Unlock()
	if err != nil {
		if !errors.As(err, new(answerError)) {
			classified.failed[model] = classifyFailure{err: err.Error(), at: time.Now()}
		}
		return "", false, err
	}
	delete(classified.failed, model)
	classified.m[key] = classifiedAs{intent: intent, at: time.Now()}
	if len(classified.m) > 4096 {
		for k, c := range classified.m {
			if time.Since(c.at) > classifyKeep {
				delete(classified.m, k)
			}
		}
	}
	return intent, false, nil
}

var reminders = regexp.MustCompile(`(?s)<system-reminder>.*?</system-reminder>`)

// userText is what the user said to begin the turn: the last user
// message's text, without what agents add to it for the model
// (<system-reminder>…), its middle left out when long.
func userText(req *Request) string {
	var parts []string
	for i := len(req.Messages) - 1; i >= 0; i-- {
		m := req.Messages[i]
		if m.Role != "user" {
			continue
		}
		for _, p := range m.Parts {
			if p.Kind == Text {
				parts = append(parts, p.Text)
			}
		}
		break
	}
	text := strings.TrimSpace(reminders.ReplaceAllString(strings.Join(parts, "\n"), ""))
	const head, tail = 3000, 1000
	if r := []rune(text); len(r) > head+tail {
		text = string(r[:head]) + "\n…\n" + string(r[len(r)-tail:])
	}
	return text
}

const classifyPrompt = "You route a user's message to a coding assistant by what it asks for. " +
	"Given numbered kinds of request and the user's message, answer with the number of the kind the message is, " +
	"or 0 if it is none of them. Answer with the number only."

// classifyBody is the Chat request asking model which of intents text is,
// at effort when it isn't "". A model that reasons does so before it
// answers, and how long varies: 2048 leaves room for that and the number.
func classifyBody(model, effort string, intents []string, text string) []byte {
	var b strings.Builder
	b.WriteString("Kinds:\n")
	for i, in := range intents {
		fmt.Fprintf(&b, "%d. %s\n", i+1, in)
	}
	b.WriteString("\nThe user's message:\n<message>\n")
	b.WriteString(text)
	b.WriteString("\n</message>\n\nThe number of its kind (0 for none):")
	req := map[string]any{
		"model": model,
		"messages": []map[string]string{
			{"role": "system", "content": classifyPrompt},
			{"role": "user", "content": b.String()},
		},
		"stream":      false,
		"temperature": 0,
		"max_tokens":  2048,
	}
	if effort != "" {
		req["reasoning_effort"] = effort
	}
	body, _ := json.Marshal(req)
	return body
}

var firstNumber = regexp.MustCompile(`\d+`)

// readIntent is the intent a classifier's answer names: its first number,
// 0 for none.
func readIntent(answer string, intents []string) (string, error) {
	n, err := strconv.Atoi(firstNumber.FindString(answer))
	if err != nil || n < 0 || n > len(intents) {
		a := []rune(strings.TrimSpace(answer))
		if len(a) > 80 {
			a = append(a[:80], '…')
		}
		return "", answerError{fmt.Errorf("it answered %q, not a number from 0 to %d", string(a), len(intents))}
	}
	if n == 0 {
		return "", nil
	}
	return intents[n-1], nil
}

// classifyEffort is the least reasoning model takes: none when it can go
// without, else its lowest level; "" when its levels aren't known, which
// leaves it to the vendor.
func classifyEffort(model string) string {
	p, m, ok := provider.Resolve(model)
	if !ok {
		return ""
	}
	levels := p.Efforts(m)
	if len(levels) == 0 {
		return ""
	}
	return fitEffort("none", levels)
}

// askClassifier asks model through the gateway itself, as a client would.
func (s *Server) askClassifier(model string, intents []string, text string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), classifyTimeout)
	defer cancel()
	r, err := http.NewRequestWithContext(ctx, http.MethodPost, "http://magpie/v1/chat/completions", nil)
	if err != nil {
		return "", err
	}
	r.Header.Set("Content-Type", "application/json")
	r.Header.Set("User-Agent", RouterAgent)
	w := httptest.NewRecorder()
	s.serve(w, r, provider.Chat, classifyBody(model, classifyEffort(model), intents, text))
	if ctx.Err() != nil {
		return "", fmt.Errorf("%s gave no answer in %s", model, classifyTimeout)
	}
	var out struct {
		Choices []struct {
			Message struct {
				Content string `json:"content"`
			} `json:"message"`
		} `json:"choices"`
		Error *struct {
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &out); err != nil {
		return "", fmt.Errorf("%s: %d, not an answer", model, w.Code)
	}
	if w.Code >= 300 || out.Error != nil {
		msg := http.StatusText(w.Code)
		if out.Error != nil && out.Error.Message != "" {
			msg = out.Error.Message
		}
		return "", fmt.Errorf("%s: %s", model, msg)
	}
	if len(out.Choices) == 0 {
		return "", answerError{fmt.Errorf("%s gave no answer", model)}
	}
	return readIntent(out.Choices[0].Message.Content, intents)
}
