//go:build !windows

package provider

// refreshPath has nothing to do: magpie looks for the CLIs where their
// installers put them.
func refreshPath() {}
