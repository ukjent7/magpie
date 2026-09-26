//go:build !darwin

package gui

import "github.com/wailsapp/wails/v3/pkg/application"

// The Dock is the Mac's; elsewhere magpie stays in the tray.
func dockPolicy(bool) application.ActivationPolicy { return application.ActivationPolicyAccessory }
func setDock(bool)                                 {}

// glidePanel steps the shown panel to height a frame at a time, keeping it
// by the tray icon as it goes.
func (h *host) glidePanel(height int, g Glide) bool {
	_, from := h.panel.Size()
	gen := h.glides.Add(1)
	go stepGlide(g, from, height, func() bool { return h.glides.Load() == gen }, func(v int) {
		h.panel.SetSize(panelWidth, v)
		_ = h.tray.PositionWindow(h.panel, 6)
	})
	return true
}

// TintPanel: the page paints the panel's tint itself here.
func (h *host) TintPanel(c [4]uint8, ms int) bool { return false }
