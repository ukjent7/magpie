package gui

import (
	"net/http"
	"strconv"
	"time"

	"github.com/yetone/magpie/internal/gateway"
)

// traceJSON is the gateway's routing trace since the page last asked.
type traceJSON struct {
	gateway.TraceState
	Mine bool      `json:"mine"` // this magpie serves the gateway: another's trace isn't here
	Now  time.Time `json:"now"`
}

// traceRoutes serves the routing trace for the Gateway view to play: it
// waits up to 25 s for something to change after the seq it is given, so
// the page hears of a request as it happens.
func traceRoutes(mux *http.ServeMux) {
	mux.HandleFunc("GET /api/gateway/trace", func(rw http.ResponseWriter, r *http.Request) {
		gw := served.Load()
		out := traceJSON{TraceState: gateway.TraceState{Routes: []gateway.Route{}}, Mine: gw != nil}
		if gw != nil {
			after, _ := strconv.ParseInt(r.URL.Query().Get("after"), 10, 64)
			var wait time.Duration
			if r.URL.Query().Get("wait") != "" {
				wait = 25 * time.Second
			}
			out.TraceState = gw.Trace(r.Context(), after, wait)
		}
		out.Now = time.Now()
		writeJSON(rw, out)
	})
}
