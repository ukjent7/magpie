//go:build !windows

package proc

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// UserPath gives the desktop app the PATH a terminal of the user's has. One
// opened from the Finder, the Dock or a login item gets launchd's
// /usr/bin:/bin:/usr/sbin:/sbin and nothing a shell profile adds, so a claude
// or codex installed with a custom npm prefix (~/.npm-global/bin), nvm, bun,
// volta or asdf wasn't found: the agent wasn't seen and its subscription
// couldn't be used. The folders those tools use are added at once, when they
// exist; the login shell's own PATH is asked for in the background and
// added after, since a slow profile mustn't hold the window up.
func UserPath() {
	home, _ := os.UserHomeDir()
	var known []string
	for _, d := range []string{".local/bin", ".npm-global/bin", ".npm/bin", ".bun/bin", ".volta/bin", ".asdf/shims", ".local/share/mise/shims", ".cargo/bin", ".deno/bin", "Library/pnpm"} {
		known = append(known, filepath.Join(home, d))
	}
	if nvm, _ := filepath.Glob(filepath.Join(home, ".nvm/versions/node/*/bin")); len(nvm) > 0 {
		known = append(known, nvm[len(nvm)-1])
	}
	known = append(known, "/opt/homebrew/bin", "/usr/local/bin")
	var have []string
	for _, d := range known {
		if st, err := os.Stat(d); err == nil && st.IsDir() {
			have = append(have, d)
		}
	}
	addPath(have)
	go func() {
		if p := shellPath(); p != "" {
			addPath(filepath.SplitList(p))
		}
	}()
}

// shellPath asks the user's login shell for its PATH; "" when it can't say
// within a few seconds.
func shellPath() string {
	sh := os.Getenv("SHELL")
	if sh == "" || !filepath.IsAbs(sh) {
		sh = "/bin/zsh"
		if _, err := os.Stat(sh); err != nil {
			sh = "/bin/sh"
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	// interactive too, since many put PATH in .zshrc/.bashrc; the marker
	// tells PATH apart from whatever the profile prints
	const mark = "__magpie_path__"
	cmd := CommandContext(ctx, sh, "-ilc", "printf '"+mark+"%s"+mark+"' \"$PATH\"")
	cmd.Stdin = nil
	out, _ := cmd.Output()
	s := string(out)
	i := strings.Index(s, mark)
	if i < 0 {
		return ""
	}
	s = s[i+len(mark):]
	j := strings.Index(s, mark)
	if j < 0 {
		return ""
	}
	return s[:j]
}

// addPath adds the folders PATH lacks, after the ones it has.
func addPath(dirs []string) {
	cur := filepath.SplitList(os.Getenv("PATH"))
	seen := map[string]bool{}
	for _, d := range cur {
		seen[d] = true
	}
	for _, d := range dirs {
		if d != "" && filepath.IsAbs(d) && !seen[d] {
			seen[d] = true
			cur = append(cur, d)
		}
	}
	os.Setenv("PATH", strings.Join(cur, string(os.PathListSeparator)))
}
