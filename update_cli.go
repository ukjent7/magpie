package main

import (
	"context"
	"fmt"
	"time"

	"github.com/yetone/magpie/internal/update"
)

// updateCmd is `magpie update [check]`: the app replaces its bundle, the
// terminal build its binary.
func updateCmd(args []string) error {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	rel, err := update.Latest(ctx)
	if err != nil {
		return err
	}
	if !update.Newer(rel.Version, version) {
		if update.Released(version) {
			fmt.Println(green.Render("✓"), "magpie", version, muted.Render("is the latest"))
		} else {
			fmt.Println("magpie", version, muted.Render("was built from source; the latest release is "+rel.Version))
		}
		return nil
	}
	fmt.Println("magpie", bold.Render(rel.Version), "is out", muted.Render("(you have "+version+") · "+rel.URL))
	if len(args) > 1 && args[1] == "check" {
		return nil
	}
	if !update.Released(version) {
		return fmt.Errorf("this magpie was built from source; update it the way you built it, or get the release from %s", update.Site)
	}
	if app := update.Bundle(); app != "" {
		if update.Stuck(app) != "" {
			return fmt.Errorf("%s can't be replaced where it is; move magpie to Applications, or download the new version from %s", tilde(app), update.Site)
		}
		fmt.Println(muted.Render("  downloading " + update.AppAsset() + " …"))
		staged, err := update.Stage(ctx, rel, app)
		if err != nil {
			return err
		}
		err = update.Install(staged, app)
		if update.NeedsAdmin(err) {
			err = update.InstallAsAdmin(staged, app)
		}
		if err != nil {
			return err
		}
		fmt.Println(green.Render("✓"), "updated", tilde(app), "to", rel.Version, muted.Render("· quit and reopen magpie to use it"))
		return nil
	}
	if exe, err := update.Executable(); err == nil && update.Homebrew(exe) {
		return fmt.Errorf("this magpie was installed with Homebrew; update it with: brew upgrade magpie")
	}
	fmt.Println(muted.Render("  downloading " + update.BinaryAsset() + " …"))
	if err := update.ReplaceBinary(ctx, rel); err != nil {
		return err
	}
	fmt.Println(green.Render("✓"), "updated to", rel.Version)
	return nil
}
