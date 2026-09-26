package provider

// Signing Codex in to the next of its accounts when the one it is on has
// used its allowance up. Codex's requests on its own models go through
// magpie while more of its accounts are on there, and move on to the next
// account when one is out — but the Codex app, once it knows the account it
// is signed in to is out, may not send at all, so no request reaches magpie
// to move on. So whichever magpie runs the gateway signs Codex in to the
// next account that is on and has room, as switching it on the Accounts
// page would: a Codex started after that is on it from the first turn.

import (
	"context"
	"log"
	"time"

	"github.com/yetone/magpie/internal/catalog"
)

// codexSwitchEvery is how often the account Codex is on is looked at.
const codexSwitchEvery = 5 * time.Minute

// usedUp reports whether an account's allowance is used up for now: a
// window that stops the account, for every model, at 100%.
func usedUp(q SubscriptionQuota) bool {
	for _, w := range q.Windows {
		if !w.Aside && w.Model == "" && w.Used >= 100 {
			return true
		}
	}
	return false
}

// NextCodexLogin is the account Codex should be signed in to instead of
// the one it is on, when that one has used its allowance up: the first of
// its other accounts that are on in magpie, in the order they were saved,
// whose allowance is known and has room. ok is false when Codex should stay.
func NextCodexLogin(ctx context.Context) (from, to string, ok bool) {
	var spares []Login
	for _, l := range Logins("codex") {
		switch {
		case l.Active:
			from = l.User
		case l.On && l.Lapsed == "":
			spares = append(spares, l)
		}
	}
	if from == "" || len(spares) == 0 {
		return "", "", false
	}
	u := LoginUsage(ctx, "codex")
	if q, known := u[from]; !known || q.Error != "" || !usedUp(q) {
		return "", "", false
	}
	for _, l := range spares {
		if q, known := u[l.User]; known && q.Error == "" && !usedUp(q) {
			return from, l.User, true
		}
	}
	return "", "", false
}

// SwitchCodexWhenUsedUp signs Codex in to the next of its accounts when the
// one it is on has used its allowance up (NextCodexLogin), and answers the
// account it signed it in to, "" when it stayed.
func SwitchCodexWhenUsedUp(ctx context.Context) (string, error) {
	from, to, ok := NextCodexLogin(ctx)
	if !ok {
		return "", nil
	}
	if err := SwitchLogin("codex", to); err != nil {
		return "", err
	}
	log.Printf("codex: %s has used its allowance up; signed Codex in to %s", from, to)
	// the models Codex is offered are the new account's plan's
	catalog.Touched()
	return to, nil
}

// KeepCodexOnAnAccountWithRoom runs SwitchCodexWhenUsedUp a minute after it
// starts and every codexSwitchEvery after that, until ctx ends.
func KeepCodexOnAnAccountWithRoom(ctx context.Context) {
	t := time.NewTimer(time.Minute)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
		c, cancel := context.WithTimeout(ctx, time.Minute)
		if _, err := SwitchCodexWhenUsedUp(c); err != nil {
			log.Printf("codex: switching to an account with room: %v", err)
		}
		cancel()
		t.Reset(codexSwitchEvery)
	}
}
