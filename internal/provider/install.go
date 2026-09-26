package provider

// Some subscriptions are used through the vendor's own CLI — Cursor's,
// Grok's, Devin's — and signed in with it too. One that isn't installed is
// installed with the vendor's own installer when its sign-in starts, as the
// vendor's page tells a person to, rather than leaving the sign-in to fail
// or, as Devin's did, to finish with nothing to show for it.

import (
	"context"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"time"

	"github.com/yetone/magpie/internal/netproxy"
	"github.com/yetone/magpie/internal/proc"
)

// agentCLI is a CLI a subscription runs through: how to find it, and the
// vendor's install one-liners for a POSIX shell and for PowerShell.
type agentCLI struct {
	Name string
	find func() string
	sh   string
	ps   string
}

func cliFor(agent string) (agentCLI, bool) {
	switch agent {
	case "devin":
		return agentCLI{"Devin CLI", func() string { return DevinExecutable() },
			"curl -fsSL https://cli.devin.ai/install.sh | bash",
			"irm https://static.devin.ai/cli/setup.ps1 | iex"}, true
	case "cursor":
		return agentCLI{"Cursor CLI", func() string { return CursorExecutable() },
			"curl https://cursor.com/install -fsS | bash",
			"irm 'https://cursor.com/install?win32=true' | iex"}, true
	case "grok":
		return agentCLI{"Grok Build", func() string { return GrokExecutable() },
			"curl -fsSL https://x.ai/cli/install.sh | bash",
			"irm https://x.ai/cli/install.ps1 | iex"}, true
	}
	return agentCLI{}, false
}

// missingCLI is the CLI an agent's sign-in needs and this machine lacks.
func missingCLI(agent string) (agentCLI, bool) {
	c, ok := cliFor(agent)
	if !ok || c.find() != "" {
		return agentCLI{}, false
	}
	return c, true
}

// installTimeout bounds an installer: they download a few hundred MB at most.
var installTimeout = 10 * time.Minute

// runInstaller runs a vendor's installer; a var so tests can fake it.
var runInstaller = func(ctx context.Context, c agentCLI) ([]byte, error) {
	var cmd *exec.Cmd
	if runtime.GOOS == "windows" {
		cmd = proc.CommandContext(ctx, "powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", c.ps)
	} else {
		cmd = proc.CommandContext(ctx, "bash", "-c", c.sh)
	}
	cmd.Dir, _ = os.UserHomeDir()
	cmd.Env = netproxy.Env(nil)
	// no stdin: an installer that would ask something takes its default
	return cmd.CombinedOutput()
}

// installCLI installs c with its vendor's installer and makes sure magpie
// finds it afterwards.
func installCLI(ctx context.Context, c agentCLI) error {
	ctx, cancel := context.WithTimeout(ctx, installTimeout)
	defer cancel()
	out, err := runInstaller(ctx, c)
	// an installer adds its directory to the user's PATH, which a running
	// program never sees
	refreshPath()
	manual := c.sh
	if runtime.GOOS == "windows" {
		manual = c.ps
	}
	if ctx.Err() == context.Canceled {
		return ctx.Err()
	}
	// what counts is the CLI being there: an installer may end in a step
	// that wants a terminal — Devin's runs `devin setup`, whose sign-in
	// gives up without one — and fail after it installed everything
	if c.find() != "" {
		return nil
	}
	if err != nil {
		msg := lastLine(out)
		if msg == "" {
			msg = err.Error()
		}
		return errorf("installing %s failed (%s); install it yourself with `%s`, then sign in again", c.Name, msg, manual)
	}
	return errorf("the %s installer finished, but magpie can't find it; install it yourself with `%s`, then sign in again", c.Name, manual)
}

// lastLine is the last line of a command's output with something on it,
// without its colours.
func lastLine(out []byte) string {
	lines := strings.Split(ansi.ReplaceAllString(string(out), ""), "\n")
	for i := len(lines) - 1; i >= 0; i-- {
		if l := strings.TrimSpace(lines[i]); l != "" {
			return l
		}
	}
	return ""
}
