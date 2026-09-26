// Package redact keeps secrets on the machine. What the gateway sends a
// vendor has its secrets (API keys, private keys, tokens, passwords in
// connection strings and assignments), and when asked personal data and the
// user's own words, swapped for placeholders like {{API_KEY_k3v9x2mq}}; what
// the vendor says back has them swapped back, streams included, so the agent
// and the user see the real thing while the vendor never does.
//
// A placeholder is the same for the same value every time (a keyed hash of
// it), so a conversation's history reads the same turn after turn — what a
// vendor cached of it stays good, and a thinking block's signature still
// matches the text it was made for — and one placeholder is never two
// values. The values are held in memory only, by placeholder, as requests
// bring them; a restart loses nothing, since the next request brings them
// again.
package redact

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base32"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strings"
	"sync"
)

// Options says what to mask.
type Options struct {
	Secrets  bool     // API keys, private keys, tokens, passwords
	Personal bool     // emails, phone numbers, ID and bank card numbers
	Words    []string // the user's own: names, codenames, hosts
}

// rule finds one kind of value. The match is group 1 when the pattern has
// one, else all of it; bound are the characters that may not touch it on
// either side (a key inside a longer word is no key); ok checks it further.
type rule struct {
	kind     string
	re       *regexp.Regexp
	markers  []string // one of these is in any text it can match
	bound    string
	ok       func(string) bool
	personal bool
}

const (
	alnum   = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
	tokenCh = alnum + "_-"
	digits  = "0123456789"
)

var rules = []rule{
	{kind: "PRIVATE_KEY", re: regexp.MustCompile(`-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----[\s\S]+?-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----`), markers: []string{"PRIVATE KEY"}},
	{kind: "API_KEY", re: regexp.MustCompile(`sk-(?:ant-|proj-|or-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}`), markers: []string{"sk-"}, bound: tokenCh},
	{kind: "API_KEY", re: regexp.MustCompile(`(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|glpat-[A-Za-z0-9_-]{20,})`), markers: []string{"gh", "github_pat_", "glpat-"}, bound: tokenCh},
	{kind: "API_KEY", re: regexp.MustCompile(`AIza[0-9A-Za-z_-]{35}`), markers: []string{"AIza"}, bound: tokenCh},
	{kind: "API_KEY", re: regexp.MustCompile(`xox[abposr]-[0-9A-Za-z-]{10,}`), markers: []string{"xox"}, bound: tokenCh},
	{kind: "API_KEY", re: regexp.MustCompile(`(?:sk|rk)_live_[0-9A-Za-z]{20,}`), markers: []string{"_live_"}, bound: tokenCh},
	{kind: "API_KEY", re: regexp.MustCompile(`(?:AKIA|ASIA)[0-9A-Z]{16}`), markers: []string{"AKIA", "ASIA"}, bound: alnum},
	{kind: "API_KEY", re: regexp.MustCompile(`(?:hf_[A-Za-z0-9]{30,}|gsk_[A-Za-z0-9]{40,}|xai-[A-Za-z0-9]{40,}|npm_[A-Za-z0-9]{36}|pypi-[A-Za-z0-9_-]{50,}|dop_v1_[a-f0-9]{64}|SG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43})`),
		markers: []string{"hf_", "gsk_", "xai-", "npm_", "pypi-", "dop_v1_", "SG."}, bound: tokenCh},
	{kind: "TOKEN", re: regexp.MustCompile(`eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}`), markers: []string{"eyJ"}, bound: tokenCh},
	// the password of user:password@host
	{kind: "PASSWORD", re: regexp.MustCompile(`[A-Za-z][A-Za-z0-9+.-]*://[^\s:@/?#"'<>]+:([^\s@/?#"'<>]{3,})@`), markers: []string{"://"}, ok: notAVariable},
	// password = …, API_KEY: "…", as .env files and configs have them
	{kind: "SECRET", re: regexp.MustCompile(`(?i)[A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credential)[A-Za-z0-9_.-]*["']?[ \t]*[:=][ \t]*["']?([A-Za-z0-9_\-./+=~!@#%^&*]{8,})`),
		markers: []string{"pass", "PASS", "Pass", "secret", "SECRET", "Secret", "token", "TOKEN", "Token", "key", "KEY", "Key", "credential", "CREDENTIAL", "Credential"}, ok: secretValue},

	{kind: "EMAIL", re: regexp.MustCompile(`[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}`), markers: []string{"@"}, bound: alnum + "._%+-", ok: realEmail, personal: true},
	{kind: "ID_CARD", re: regexp.MustCompile(`[1-9]\d{5}(?:18|19|20)\d{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12]\d|3[01])\d{3}[\dXx]`), bound: alnum, ok: chineseID, personal: true},
	{kind: "PHONE", re: regexp.MustCompile(`(?:\+?86[- ]?)?1[3-9]\d{9}`), bound: digits, personal: true},
	{kind: "BANK_CARD", re: regexp.MustCompile(`[3-6]\d{3}(?:[ -]?\d{4}){2,3}(?:[ -]?\d{1,3})?`), bound: digits, ok: luhn, personal: true},
}

// placeholderRe is a placeholder as Mask writes it.
var placeholderRe = regexp.MustCompile(`\{\{[A-Z][A-Z_]*_[a-z2-7]{8}\}\}`)

// notAVariable turns away what stands in for a value: ${PASS}, <password>, ****.
func notAVariable(v string) bool {
	if strings.ContainsAny(v[:1], "$%{<[*") {
		return false
	}
	return strings.Trim(v, "*xX.") != ""
}

// secretValue is a value that looks like a secret, not code: letters and
// digits both, and not a call, a reference or a placeholder of its own.
func secretValue(v string) bool {
	if !notAVariable(v) || strings.HasPrefix(v, "process.env") || strings.HasPrefix(v, "os.") {
		return false
	}
	return strings.ContainsAny(v, digits) && strings.IndexFunc(v, func(r rune) bool { return r >= 'A' && r <= 'Z' || r >= 'a' && r <= 'z' }) >= 0
}

func realEmail(v string) bool {
	host := strings.ToLower(v[strings.LastIndexByte(v, '@')+1:])
	for _, fake := range []string{"example.com", "example.org", "example.net", "localhost", ".test", ".invalid", ".example"} {
		if host == strings.TrimPrefix(fake, ".") || strings.HasSuffix(host, fake) {
			return false
		}
	}
	return !strings.HasPrefix(host, "noreply") && !strings.Contains(v, "noreply@")
}

func chineseID(v string) bool {
	w := []int{7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2}
	sum := 0
	for i := 0; i < 17; i++ {
		sum += int(v[i]-'0') * w[i]
	}
	return strings.ToUpper(v[17:]) == string("10X98765432"[sum%11])
}

func luhn(v string) bool {
	var ds []int
	for _, r := range v {
		if r >= '0' && r <= '9' {
			ds = append(ds, int(r-'0'))
		}
	}
	if len(ds) < 15 || len(ds) > 19 {
		return false
	}
	sum := 0
	for i := range ds {
		d := ds[len(ds)-1-i]
		if i%2 == 1 {
			if d *= 2; d > 9 {
				d -= 9
			}
		}
		sum += d
	}
	return sum%10 == 0
}

// ---- placeholders ---------------------------------------------------------------

var (
	mu     sync.RWMutex
	values = map[string]string{} // placeholder → value
	keyOf  []byte
)

// maxValues bounds what is held; past it the oldest half is let go, and a
// request that still has them brings them back.
const maxValues = 200_000

func key() []byte {
	mu.RLock()
	k := keyOf
	mu.RUnlock()
	if k != nil {
		return k
	}
	mu.Lock()
	defer mu.Unlock()
	if keyOf != nil {
		return keyOf
	}
	keyOf = loadKey()
	return keyOf
}

// KeyPath is where the key placeholders are made with is kept, so they
// stay the same across restarts. Set by the gateway's owner; "" keeps it in
// memory only.
var KeyPath string

func loadKey() []byte {
	if KeyPath != "" {
		if b, err := os.ReadFile(KeyPath); err == nil && len(b) >= 32 {
			return b[:32]
		}
	}
	b := make([]byte, 32)
	_, _ = rand.Read(b)
	if KeyPath != "" {
		_ = os.MkdirAll(filepath.Dir(KeyPath), 0o700)
		_ = os.WriteFile(KeyPath, b, 0o600)
	}
	return b
}

var b32 = base32.StdEncoding.WithPadding(base32.NoPadding)

func placeholder(kind, v string) string {
	h := hmac.New(sha256.New, key())
	h.Write([]byte(kind + "\x00" + v))
	p := "{{" + kind + "_" + strings.ToLower(b32.EncodeToString(h.Sum(nil))[:8]) + "}}"
	mu.Lock()
	if _, ok := values[p]; !ok {
		if len(values) >= maxValues {
			n := 0
			for k := range values {
				delete(values, k)
				if n++; n >= maxValues/2 {
					break
				}
			}
		}
		values[p] = v
	}
	mu.Unlock()
	return p
}

// Known says there are values to put back.
func Known() bool {
	mu.RLock()
	defer mu.RUnlock()
	return len(values) > 0
}

// ---- masking --------------------------------------------------------------------

type span struct {
	start, end int
	kind       string
}

// Mask swaps what o covers in s for placeholders, and says how many.
func Mask(s string, o Options) (string, int) {
	if len(s) < 3 {
		return s, 0
	}
	var found []span
	for _, w := range o.Words {
		if w = strings.TrimSpace(w); len(w) < 2 {
			continue
		}
		for i := 0; ; {
			j := strings.Index(s[i:], w)
			if j < 0 {
				break
			}
			found = append(found, span{i + j, i + j + len(w), "TERM"})
			i += j + len(w)
		}
	}
	for _, r := range rules {
		if r.personal && !o.Personal || !r.personal && !o.Secrets {
			continue
		}
		if len(r.markers) > 0 && !containsAny(s, r.markers) {
			continue
		}
		for _, m := range r.re.FindAllStringSubmatchIndex(s, -1) {
			a, b := m[0], m[1]
			if len(m) >= 4 && m[2] >= 0 {
				a, b = m[2], m[3]
			}
			if r.bound != "" && (a > 0 && strings.IndexByte(r.bound, s[a-1]) >= 0 || b < len(s) && strings.IndexByte(r.bound, s[b]) >= 0) {
				continue
			}
			if r.ok != nil && !r.ok(s[a:b]) {
				continue
			}
			found = append(found, span{a, b, r.kind})
		}
	}
	if len(found) == 0 {
		return s, 0
	}
	// nothing inside a placeholder already there
	if strings.Contains(s, "{{") {
		for _, p := range placeholderRe.FindAllStringIndex(s, -1) {
			found = slices.DeleteFunc(found, func(f span) bool { return f.start < p[1] && f.end > p[0] })
		}
		if len(found) == 0 {
			return s, 0
		}
	}
	// the first to start, and of those the longest, wins where they overlap
	sort.Slice(found, func(i, j int) bool {
		if found[i].start != found[j].start {
			return found[i].start < found[j].start
		}
		return found[i].end > found[j].end
	})
	var b strings.Builder
	last, n := 0, 0
	for _, f := range found {
		if f.start < last {
			continue
		}
		b.WriteString(s[last:f.start])
		b.WriteString(placeholder(f.kind, s[f.start:f.end]))
		last, n = f.end, n+1
	}
	b.WriteString(s[last:])
	return b.String(), n
}

func containsAny(s string, subs []string) bool {
	for _, x := range subs {
		if strings.Contains(s, x) {
			return true
		}
	}
	return false
}

// ---- restoring ------------------------------------------------------------------

// Restore puts the values back in s. escaped is for text that is itself
// JSON (a tool call's arguments), where a value goes in escaped.
func Restore(s string, escaped bool) string {
	if !strings.Contains(s, "{{") {
		return s
	}
	mu.RLock()
	defer mu.RUnlock()
	return placeholderRe.ReplaceAllStringFunc(s, func(p string) string {
		v, ok := values[p]
		if !ok {
			return p
		}
		if escaped {
			return jsonEscape(v)
		}
		return v
	})
}

// partialTail is how much of the end of s may be the start of a
// placeholder the next piece of a stream finishes.
func partialTail(s string) int {
	i := strings.LastIndexByte(s, '{')
	if i < 0 {
		return 0
	}
	// the {{ that opens it
	if i > 0 && s[i-1] == '{' {
		i--
	}
	t := s[i:]
	if len(t) > 32 || strings.Contains(t, "}}") {
		return 0
	}
	// {, {{, {{A…, {{API_KEY_ab…, {{API_KEY_abcdefgh}
	if t == "{" || t == "{{" {
		return len(t)
	}
	if !strings.HasPrefix(t, "{{") {
		return 0
	}
	body := strings.TrimSuffix(t[2:], "}")
	if body == "" || body[0] < 'A' || body[0] > 'Z' {
		return 0
	}
	for j := 0; j < len(body); j++ {
		c := body[j]
		if !(c >= 'A' && c <= 'Z' || c == '_' || c >= 'a' && c <= 'z' || c >= '2' && c <= '7') {
			return 0
		}
	}
	return len(t)
}
