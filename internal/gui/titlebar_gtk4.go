//go:build linux && cgo && !gtk3

package gui

/*
#cgo pkg-config: gtk4
#include <gtk/gtk.h>

static void plain_titlebar(void *w) {
	GtkWidget *bar = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 0);
	gtk_widget_set_visible(bar, FALSE);
	gtk_window_set_titlebar(GTK_WINDOW(w), bar);
}
*/
import "C"

import "github.com/wailsapp/wails/v3/pkg/application"

// plainTitlebar takes the system's title bar off a window whose page has a
// header of its own. GNOME draws one over it, the name and a close button
// stacked on the tabs; frameless, the window would lose its shadow, rounded
// corners and the edges it is resized by. A hidden widget as its titlebar
// keeps all those and shows no bar: the page's header drags it, and has the
// close button.
func plainTitlebar(w *application.WebviewWindow) {
	application.InvokeSync(func() {
		if p := w.NativeWindow(); p != nil {
			C.plain_titlebar(p)
		}
	})
}
