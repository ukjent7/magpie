//go:build !nogui

package main

import (
	"github.com/yetone/magpie/internal/gui"
	"github.com/yetone/magpie/internal/proc"
)

const hasGUI = true

// the desktop app may have been opened from the Finder, with none of the
// PATH a terminal has
func runGUI(showMain bool, link string) error {
	proc.UserPath()
	return gui.Run(version, showMain, link)
}
