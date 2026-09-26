package main

import (
	"testing"

	"github.com/yetone/magpie/internal/catalog"
	"github.com/yetone/magpie/internal/provider"
	"github.com/yetone/magpie/internal/settings"
)

// magpie model name and --reset keep and drop the user's name for a
// provider's model, a name of several words included.
func TestModelNameCmd(t *testing.T) {
	groupsHome(t)
	catalog.Changed = nil // no agent's files are rewritten here
	if err := modelCmd([]string{"name", "a/m", "My", "Model"}); err != nil {
		t.Fatal(err)
	}
	if n, ok := provider.ModelName("a", "m"); !ok || n != "My Model" {
		t.Fatal(n, ok)
	}
	if err := modelCmd([]string{"name", "a/m"}); err != nil {
		t.Fatal(err)
	}
	if err := modelCmd([]string{"names"}); err != nil {
		t.Fatal(err)
	}
	if err := modelCmd([]string{"name", "a/m", "--reset"}); err != nil {
		t.Fatal(err)
	}
	if _, ok := provider.ModelName("a", "m"); ok {
		t.Fatal("still named")
	}
	if err := modelCmd([]string{"name", "m", "x"}); err == nil {
		t.Fatal("named a model without its provider")
	}
	// a model without reasoning levels has none to keep
	if err := modelCmd([]string{"efforts", "a/m", "low"}); err == nil {
		t.Fatal("kept a level of a model without any")
	}
	if err := modelCmd([]string{"efforts", "a/m", "--reset"}); err != nil {
		t.Fatal(err)
	}
	if es := settings.Load().ModelEfforts; len(es) != 0 {
		t.Fatal(es)
	}
}
