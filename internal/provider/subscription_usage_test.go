package provider

import (
	"context"
	"testing"
	"time"
)

func TestCodexQuotaUsesProviderWindowDuration(t *testing.T) {
	for _, tt := range []struct {
		seconds int64
		want    string
	}{{5 * 60 * 60, "5 hours"}, {7 * 24 * 60 * 60, "7 days"}, {0, "Allowance"}} {
		if got := quotaDurationName(tt.seconds); got != tt.want {
			t.Errorf("quotaDurationName(%d) = %q, want %q", tt.seconds, got, tt.want)
		}
	}

	w := codexWindow{UsedPercent: 16, LimitWindowSecs: 7 * 24 * 60 * 60}
	if got := w.window(); got.Name != "7 days" || got.Used != 16 {
		t.Fatalf("Codex window = %+v", got)
	}
}

func TestCompactQuotaNumber(t *testing.T) {
	for _, tt := range []struct {
		in   float64
		want string
	}{{7, "7"}, {7.5, "7.5"}, {7.25, "7.25"}} {
		if got := compactNumber(tt.in); got != tt.want {
			t.Errorf("compactNumber(%v) = %q, want %q", tt.in, got, tt.want)
		}
	}
}

// A stale copy comes back at once while it refreshes, and accounts removed
// from magpie in the meantime are left out of it.
func TestSubscriptionUsageServesStale(t *testing.T) {
	signIn(t)
	if err := Delete("codex"); err != nil {
		t.Fatal(err)
	}
	c := &subscriptionUsageCache
	c.Lock()
	c.at, c.pending = time.Now().Add(-time.Hour), nil
	c.data = []SubscriptionQuota{{Provider: "claude", Name: "Claude Code"}, {Provider: "codex", Name: "Codex"}}
	c.Unlock()
	t.Cleanup(func() {
		for {
			c.Lock()
			p := c.pending
			if p == nil {
				c.data, c.at = nil, time.Time{}
				c.Unlock()
				return
			}
			c.Unlock()
			<-p
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	start := time.Now()
	got := SubscriptionUsage(ctx)
	if time.Since(start) > 40*time.Millisecond {
		t.Fatalf("waited %v for a stale copy", time.Since(start))
	}
	if len(got) != 1 || got[0].Provider != "claude" {
		t.Fatalf("got %+v, want only claude", got)
	}
	c.Lock()
	refreshing := c.pending != nil || time.Since(c.at) < time.Minute
	c.Unlock()
	if !refreshing {
		t.Fatal("a stale copy did not start a refresh")
	}
}

func TestChosenWindows(t *testing.T) {
	ws := []QuotaWindow{{Name: "Gemini 3.8 Flash (High)", Model: "gemini-3.8-flash-high"}, {Name: "Gemini 3 Flash", Model: "gemini-3-flash"}, {Name: "Weekly"}}
	got := chosenWindows(ws, map[string]bool{"gemini-3.8-flash-high": true})
	if len(got) != 2 || got[0].Model != "gemini-3.8-flash-high" || got[1].Name != "Weekly" {
		t.Fatalf("got %+v", got)
	}
	// ids the quota names that none of the enabled ones match: keep them all
	if got := chosenWindows(ws[:2], map[string]bool{"gemini-pro-agent": true}); len(got) != 2 {
		t.Fatalf("kept %d of 2", len(got))
	}
}
