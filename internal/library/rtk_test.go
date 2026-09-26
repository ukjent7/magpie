package library

import (
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

// fakeRTK is an rtk on PATH that acts as rtk 0.50's installer does for
// Claude Code, OpenCode and Cursor: installing OpenCode or Cursor gives it to
// Claude Code as well, and uninstalling Claude Code or OpenCode takes all
// three away.
const fakeRTK = `#!/bin/sh
c="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
claude() { [ -d "$c" ] && echo '{"hooks":{"PreToolUse":[{"hooks":[{"command":"rtk hook claude"}]}]}}' > "$c/settings.json"; }
case "$1" in
--version) echo "rtk 9.9.9"; exit 0 ;;
gain) echo '{"summary":{"total_commands":3,"total_input":100,"total_saved":60,"avg_savings_pct":60}}'; exit 0 ;;
esac
shift 2
case "$*" in
"--auto-patch") claude ;;
"--opencode --auto-patch") claude; /bin/mkdir -p "$XDG_CONFIG_HOME/opencode/plugins"; echo x > "$XDG_CONFIG_HOME/opencode/plugins/rtk.ts" ;;
"--agent cursor --auto-patch") claude; echo '{"hooks":{"preToolUse":[{"command":"rtk hook cursor"}]}}' > "$HOME/.cursor/hooks.json" ;;
"--agent cursor --uninstall") echo '{}' > "$HOME/.cursor/hooks.json" ;;
"--uninstall"|"--opencode --uninstall")
	echo '{}' > "$c/settings.json"; /bin/rm -f "$XDG_CONFIG_HOME/opencode/plugins/rtk.ts"; echo '{}' > "$HOME/.cursor/hooks.json" ;;
*) echo "unknown: $*" >&2; exit 2 ;;
esac
`

func TestRTK(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("the fake rtk is a shell script")
	}
	h := sandbox(t)
	bin := filepath.Join(h, "bin")
	write(t, filepath.Join(bin, "rtk"), fakeRTK)
	if err := os.Chmod(filepath.Join(bin, "rtk"), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin)
	state := func() map[string]bool {
		m := map[string]bool{}
		for _, a := range ReadRTK().Agents {
			m[a.ID] = a.On
		}
		return m
	}
	want := func(claude, opencode, cursor bool) {
		t.Helper()
		s := state()
		if s["claude"] != claude || s["opencode"] != opencode || s["cursor"] != cursor {
			t.Fatalf("claude, opencode, cursor = %v, %v, %v; want %v, %v, %v", s["claude"], s["opencode"], s["cursor"], claude, opencode, cursor)
		}
	}
	set := func(id string, on bool) {
		t.Helper()
		if _, err := SetRTK(id, on); err != nil {
			t.Fatal(err)
		}
	}

	v := ReadRTK()
	if v.Version != "9.9.9" || v.Gain == nil || v.Gain.Saved != 60 {
		t.Fatalf("version %q, gain %+v", v.Version, v.Gain)
	}
	want(false, false, false)
	// OpenCode's and Cursor's installers leave Claude Code as it was
	set("opencode", true)
	set("cursor", true)
	want(false, true, true)
	set("claude", true)
	want(true, true, true)
	// Claude Code's uninstaller takes the other two with it: they're put back
	set("claude", false)
	want(false, true, true)
	set("claude", true)
	set("opencode", false)
	want(true, false, true)
	set("cursor", false)
	want(true, false, false)

	if _, err := SetRTK("goose", true); err == nil {
		t.Fatal("goose has no rtk hook, yet it was switched on")
	}
	t.Setenv("PATH", "")
	if v := ReadRTK(); v.Path != "" {
		t.Fatalf("rtk found at %s with nothing on PATH", v.Path)
	}
	if _, err := SetRTK("claude", false); err == nil {
		t.Fatal("switched without rtk installed")
	}
}
