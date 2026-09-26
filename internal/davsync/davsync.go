// Package davsync keeps magpie's setup the same on every computer through
// a WebDAV folder. What goes there is a backup (see internal/backup),
// sealed with a passphrase before it leaves the computer, so the server
// only ever holds a file it can't read.
//
// The magpie serving the gateway syncs every few minutes. The setup is
// taken in four parts: providers (with their pictures and groups),
// settings, profiles and the agents' models. A part changed only here is
// pushed; one changed only on the server is brought in; one changed on
// both since the last sync keeps the newer, and the one it replaced is
// saved in the sync folder beside magpie's files and named in a notice.
// What the server holds is mirrored: a provider removed on one computer
// goes from the others.
package davsync

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/yetone/magpie/internal/agent"
	"github.com/yetone/magpie/internal/backup"
	"github.com/yetone/magpie/internal/edit"
	"github.com/yetone/magpie/internal/profile"
	"github.com/yetone/magpie/internal/provider"
	"github.com/yetone/magpie/internal/settings"
)

// Every is how often the gateway's magpie syncs.
const Every = 3 * time.Minute

// Parts, in the order they are told.
var Parts = []string{"providers", "settings", "profiles", "agents"}

// Config is the sync's setup, kept in sync.json beside magpie's other
// files, readable by the user alone, as provider keys are.
type Config struct {
	URL        string `json:"url"`
	User       string `json:"user,omitempty"`
	Password   string `json:"password,omitempty"`
	Passphrase string `json:"passphrase"`
	Keys       bool   `json:"keys"`   // providers carry their API keys
	Agents     bool   `json:"agents"` // the agents' models go too
}

// Notice says what a sync replaced when a part had changed on both sides,
// or when this computer first joined a folder with a setup in it.
type Notice struct {
	At    time.Time `json:"at"`
	Here  []string  `json:"here,omitempty"`  // this computer's parts, replaced by the server's
	There []string  `json:"there,omitempty"` // the server's parts, replaced by this computer's
	Saved string    `json:"saved,omitempty"` // the folder the replaced copies are kept in
}

// state is what the last sync saw: the parts here and on the server, by
// hash, and the server's file.
type state struct {
	Key    string            `json:"key"` // the address, user and passphrase it was for
	Last   time.Time         `json:"last,omitzero"`
	Error  string            `json:"error,omitempty"`
	Notice *Notice           `json:"notice,omitempty"`
	Sum    string            `json:"sum,omitempty"` // the server's file, hashed
	Local  map[string]string `json:"local,omitempty"`
	Remote map[string]string `json:"remote,omitempty"`
}

func path(name string) string { return filepath.Join(settings.Dir(), name) }

// Load reads the setup; false when sync is off.
func Load() (Config, bool) {
	var c Config
	b, err := os.ReadFile(path("sync.json"))
	if err != nil || json.Unmarshal(b, &c) != nil || c.URL == "" {
		return Config{}, false
	}
	return c, true
}

// Configure turns sync on, or changes it. A password or passphrase left
// empty keeps the one set before.
func Configure(c Config) error {
	c.URL, c.User = strings.TrimSpace(c.URL), strings.TrimSpace(c.User)
	if _, err := newDAV(c); err != nil {
		return err
	}
	if old, ok := Load(); ok {
		if c.Password == "" {
			c.Password = old.Password
		}
		if c.Passphrase == "" {
			c.Passphrase = old.Passphrase
		}
	}
	if c.Passphrase == "" {
		return errors.New("sync needs a passphrase: the file is sealed with it before it leaves this computer")
	}
	if c.Password != "" && c.Passphrase == c.Password {
		// the server is sent the password: with it, it could open the file
		return errors.New("the passphrase is the server's password: the server is sent the password, and could open the file with it. Pick a passphrase of its own")
	}
	b, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return err
	}
	os.MkdirAll(settings.Dir(), 0o755)
	if err := edit.WriteAtomic(path("sync.json"), b); err != nil {
		return err
	}
	return os.Chmod(path("sync.json"), 0o600)
}

// Off turns sync off. The file on the server stays.
func Off() error {
	mu.Lock()
	defer mu.Unlock()
	os.Remove(path("sync-state.json"))
	if err := os.Remove(path("sync.json")); err != nil && !os.IsNotExist(err) {
		return err
	}
	return nil
}

// View is sync as the Settings page shows it: never the secrets.
type View struct {
	On            bool      `json:"on"`
	URL           string    `json:"url,omitempty"`
	User          string    `json:"user,omitempty"`
	PasswordSet   bool      `json:"passwordSet,omitempty"`
	PassphraseSet bool      `json:"passphraseSet,omitempty"`
	Keys          bool      `json:"keys"`
	Agents        bool      `json:"agents"`
	Last          time.Time `json:"last,omitzero"`
	Error         string    `json:"error,omitempty"`
	Notice        *Notice   `json:"notice,omitempty"`
}

// Status is sync's setup and how the last sync went.
func Status() View {
	c, ok := Load()
	if !ok {
		return View{Keys: true, Agents: true}
	}
	st := loadState()
	v := View{On: true, URL: c.URL, User: c.User, PasswordSet: c.Password != "", PassphraseSet: c.Passphrase != "",
		Keys: c.Keys, Agents: c.Agents, Error: st.Error, Notice: st.Notice}
	if st.Key == stateKey(c) {
		v.Last = st.Last
	}
	return v
}

// Dismiss clears the notice.
func Dismiss() {
	mu.Lock()
	defer mu.Unlock()
	st := loadState()
	st.Notice = nil
	saveState(st)
}

func loadState() state {
	var st state
	if b, err := os.ReadFile(path("sync-state.json")); err == nil {
		json.Unmarshal(b, &st)
	}
	return st
}

func saveState(st state) {
	b, _ := json.MarshalIndent(st, "", "  ")
	edit.WriteAtomic(path("sync-state.json"), b)
}

func stateKey(c Config) string { return sum([]byte(c.URL + "\x00" + c.User + "\x00" + c.Passphrase)) }

func sum(b []byte) string {
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

var mu sync.Mutex

// Now syncs once; nothing when sync is off.
func Now(ctx context.Context) error {
	mu.Lock()
	defer mu.Unlock()
	c, ok := Load()
	if !ok {
		return nil
	}
	st := loadState()
	if st.Key != stateKey(c) { // another folder or passphrase: start afresh
		st = state{Key: stateKey(c)}
	}
	err := syncOnce(ctx, c, &st)
	if errors.Is(err, errChanged) { // another computer got in between: again, over its version
		err = syncOnce(ctx, c, &st)
	}
	st.Error = ""
	if err != nil {
		st.Error = err.Error()
	} else {
		st.Last = time.Now()
	}
	saveState(st)
	return err
}

// Run syncs a little after it starts and every Every after that, until
// ctx ends. A failure is logged once, not on every try.
func Run(ctx context.Context) {
	t := time.NewTimer(20 * time.Second)
	defer t.Stop()
	last := ""
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
		c, cancel := context.WithTimeout(ctx, 2*time.Minute)
		err := Now(c)
		cancel()
		if msg := fmt.Sprint(err); err != nil && msg != last {
			log.Printf("syncing through WebDAV: %s", msg)
			last = msg
		} else if err == nil {
			last = ""
		}
		t.Reset(Every)
	}
}

func collect(c Config) (backup.Bundle, error) {
	b, err := backup.Collect(c.Keys, "magpie")
	if b.Settings == nil {
		s := settings.Load()
		b.Settings = &s
	}
	if !c.Agents {
		b.Agents = nil
	}
	return b, err
}

// hashes is each part of b, hashed: what is compared to tell a change.
func hashes(b backup.Bundle) map[string]string {
	h := func(v any) string { j, _ := json.Marshal(v); return sum(j) }
	var s settings.Settings
	if b.Settings != nil {
		s = *b.Settings
	}
	s.Window, s.Proxy, s.Dock = nil, "", false // this computer's own: never synced
	return map[string]string{
		"providers": h([]any{b.Providers, b.Icons, b.Groups}),
		"settings":  h(s),
		"profiles":  h(orEmpty(b.Profiles)),
		"agents":    h(orEmpty(b.Agents)),
	}
}

func orEmpty[V any](m map[string]V) map[string]V {
	if m == nil {
		return map[string]V{}
	}
	return m
}

// take puts from's part in to.
func take(to *backup.Bundle, from backup.Bundle, part string) {
	switch part {
	case "providers":
		ps := from.Providers
		if !from.Keys && to.Keys { // sent without keys: keep the ones the server has
			keys := map[string]provider.Provider{}
			for _, p := range to.Providers {
				keys[p.ID] = p
			}
			ps = slices.Clone(ps)
			for i, p := range ps {
				if k, ok := keys[p.ID]; ok && p.Key == "" && len(p.Keys) == 0 {
					ps[i].Key, ps[i].KeyName, ps[i].Keys, ps[i].KeyProtocol = k.Key, k.KeyName, k.Keys, k.KeyProtocol
				}
			}
		}
		to.Providers, to.Icons, to.Groups = ps, from.Icons, from.Groups
		to.Keys = to.Keys || from.Keys
	case "settings":
		to.Settings = from.Settings
	case "profiles":
		to.Profiles = from.Profiles
	case "agents":
		to.Agents = from.Agents
	}
}

// changed is when a part was last changed here, as its files say.
func changed(part string) time.Time {
	mtime := func(p string) time.Time {
		if fi, err := os.Stat(p); err == nil {
			return fi.ModTime()
		}
		return time.Time{}
	}
	switch part {
	case "providers":
		return mtime(provider.Path())
	case "settings":
		return mtime(settings.Path())
	case "profiles":
		return mtime(profile.Path())
	}
	var t time.Time
	for _, a := range agent.Detected() {
		if m := mtime(a.Path); m.After(t) {
			t = m
		}
	}
	return t
}

// bring puts the server's part in here: providers and profiles mirrored,
// so what went elsewhere goes here too.
func bring(b backup.Bundle, part string) error {
	switch part {
	case "providers":
		for name, data := range b.Icons {
			if f := provider.IconFile(name); f != "" && len(data) <= provider.MaxIcon {
				if _, err := os.Stat(f); err != nil {
					os.MkdirAll(filepath.Dir(f), 0o755)
					edit.WriteAtomic(f, data)
				}
			}
		}
		return provider.Mirror(b.Providers, b.Groups)
	case "settings":
		_, err := backup.Restore(b, backup.Parts{Settings: true})
		return err
	case "profiles":
		here, err := profile.Load()
		if err != nil {
			return err
		}
		for name := range here {
			if _, ok := b.Profiles[name]; !ok {
				if err := profile.Delete(name); err != nil {
					return err
				}
			}
		}
		_, err = backup.Restore(b, backup.Parts{Profiles: true})
		return err
	case "agents":
		_, err := backup.Restore(b, backup.Parts{Agents: true})
		return err
	}
	return nil
}

func syncOnce(ctx context.Context, c Config, st *state) error {
	d, err := newDAV(c)
	if err != nil {
		return err
	}
	data, etag, err := d.get(ctx)
	if err != nil {
		return err
	}
	local, err := collect(c)
	if err != nil {
		return err
	}
	L := hashes(local)
	push := func(b backup.Bundle, etag string) error {
		b.Created, b.App = time.Now().UTC(), "magpie"
		sealed, err := backup.Seal(b, c.Passphrase)
		if err != nil {
			return err
		}
		if err := d.put(ctx, sealed, etag); err != nil {
			return err
		}
		st.Sum, st.Remote = sum(sealed), hashes(b)
		return nil
	}
	if data == nil { // nothing there yet: this computer's setup is the first
		if err := push(local, ""); err != nil {
			return err
		}
		st.Local = L
		return nil
	}
	if sum(data) == st.Sum && maps.Equal(L, st.Local) {
		return nil // nothing changed on either side
	}
	remote, err := backup.Open(data, c.Passphrase)
	if errors.Is(err, backup.ErrPassphrase) {
		return errors.New("the passphrase doesn't open the file on the server: it was sealed with another one; use the passphrase set on your other computers")
	}
	if err != nil {
		return err
	}
	R := hashes(remote)
	first := st.Local == nil
	merged := remote
	var bringIn, here, there []string
	for _, p := range Parts {
		if L[p] == R[p] || (p == "agents" && !c.Agents) {
			continue
		}
		lc := !first && L[p] != st.Local[p]
		rc := first || R[p] != st.Remote[p]
		switch {
		case lc && !rc:
			take(&merged, local, p)
		case rc && !lc:
			bringIn = append(bringIn, p)
			if first {
				here = append(here, p)
			}
		case lc && rc: // both: the newer stays
			if changed(p).After(remote.Created) {
				take(&merged, local, p)
				there = append(there, p)
			} else {
				bringIn = append(bringIn, p)
				here = append(here, p)
			}
		}
	}
	var saved string
	if len(here)+len(there) > 0 {
		dir := path("sync")
		os.MkdirAll(dir, 0o700)
		stamp := time.Now().Format("2006-01-02-150405")
		if len(here) > 0 {
			if b, err := backup.Seal(local, c.Passphrase); err == nil {
				edit.WriteAtomic(filepath.Join(dir, stamp+"-this-computer"+backup.Ext), b)
			}
		}
		if len(there) > 0 {
			edit.WriteAtomic(filepath.Join(dir, stamp+"-server"+backup.Ext), data)
		}
		saved = dir
	}
	for _, p := range bringIn {
		if err := bring(remote, p); err != nil {
			return fmt.Errorf("bringing in the %s: %w", p, err)
		}
	}
	// the server's file is in; what is pushed next counts as changed here
	// until the push is done
	now := L
	if len(bringIn) > 0 {
		if local, err = collect(c); err != nil {
			return err
		}
		now = hashes(local)
	}
	M, pending := hashes(merged), map[string]string{}
	for _, p := range Parts {
		pending[p] = now[p]
		if M[p] != R[p] && st.Local != nil {
			pending[p] = st.Local[p]
		}
	}
	st.Local, st.Sum, st.Remote = pending, sum(data), R
	if len(here)+len(there) > 0 {
		st.Notice = &Notice{At: time.Now(), Here: here, There: there, Saved: saved}
	}
	if !maps.Equal(M, R) {
		if err := push(merged, etag); err != nil {
			return err
		}
	}
	st.Local = now
	return nil
}
