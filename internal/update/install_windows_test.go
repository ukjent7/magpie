package update

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

// What the last update moved aside can still be running (a `magpie serve`
// started before it), and then can't be removed or replaced: the running
// exe goes beside it and the new version still goes in.
func TestInstallBinaryBesideARunningOld(t *testing.T) {
	ping := filepath.Join(os.Getenv("SystemRoot"), "System32", "PING.EXE")
	b, err := os.ReadFile(ping)
	if err != nil {
		t.Skip("no PING.EXE:", err)
	}
	dir := t.TempDir()
	exe := filepath.Join(dir, "magpie.exe")
	for _, f := range []string{exe, exe + ".old"} {
		if err := os.WriteFile(f, b, 0o755); err != nil {
			t.Fatal(err)
		}
		cmd := exec.Command(f, "-n", "30", "127.0.0.1")
		if err := cmd.Start(); err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { cmd.Process.Kill(); cmd.Wait() })
	}
	if os.Remove(exe+".old") == nil {
		t.Fatal("a running .old could be removed; the test proves nothing")
	}
	staged := exe + ".new"
	os.WriteFile(staged, []byte("new version"), 0o755)
	if err := InstallBinary(staged, exe); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(exe); string(got) != "new version" {
		t.Fatalf("exe is not the new version: %d bytes", len(got))
	}
	if _, err := os.Stat(exe + ".old-2"); err != nil {
		t.Fatal("the running exe was not moved beside the old:", err)
	}
}
