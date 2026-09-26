package main

import (
	"fmt"
	"maps"
	"slices"
	"strings"

	"github.com/yetone/magpie/internal/provider"
	"github.com/yetone/magpie/internal/settings"
)

// A model's name and reasoning levels, from the terminal: magpie model.

const modelUsage = `usage:
  magpie model name <provider/model>             the name the model goes by
  magpie model name <provider/model> <name>      name it so everywhere: in magpie, in the gateway's
                                                 model list, and in the lists magpie writes into the agents
  magpie model name <provider/model> --reset     give it back its own name
  magpie model efforts <provider/model>          the reasoning levels it offers, and those it has
  magpie model efforts <provider/model> <l>,<l>  offer only these of them, e.g. low,medium,high
  magpie model efforts <provider/model> --reset  offer every level it has again
  magpie model names                             the models you named or narrowed

  Only what agents are shown changes: they still pick the model, and requests still reach it,
  as <provider/model>. The same model from another provider keeps its own name and levels.

  e.g. magpie model name claude/claude-opus-5-5 "Opus 5.5"
       magpie model efforts openai/gpt-6 low,medium,high`

func modelCmd(args []string) error {
	if len(args) == 0 {
		return fmt.Errorf("%s", modelUsage)
	}
	switch args[0] {
	case "names", "ls", "list":
		return modelNames()
	case "name", "rename":
		return modelName(args[1:])
	case "efforts", "effort", "levels":
		return modelEfforts(args[1:])
	case "help", "-h", "--help":
		fmt.Println(modelUsage)
		return nil
	}
	return fmt.Errorf("unknown command %q\n\n%s", args[0], modelUsage)
}

// modelRef is the provider and model a typed provider/model names.
func modelRef(s string) (*provider.Provider, string, error) {
	ref := strings.TrimPrefix(strings.TrimSpace(s), "magpie/")
	pid, model, ok := strings.Cut(ref, "/")
	if !ok || model == "" {
		return nil, "", fmt.Errorf("name a model as provider/model, not %q (magpie models lists them)", s)
	}
	p, err := provider.Find(pid)
	if err != nil {
		return nil, "", err
	}
	return p, model, nil
}

func isReset(args []string) bool {
	return len(args) == 1 && slices.Contains([]string{"--reset", "-r", "--default", "default", "reset"}, args[0])
}

func modelName(args []string) error {
	if len(args) == 0 {
		return fmt.Errorf("%s", modelUsage)
	}
	p, model, err := modelRef(args[0])
	if err != nil {
		return err
	}
	own := model
	for _, m := range p.Available() {
		if m.ID == model && m.Name != "" {
			own = m.Name
			break
		}
	}
	id := p.ID + "/" + model
	rest := args[1:]
	if len(rest) == 0 {
		if n, ok := provider.ModelName(p.ID, model); ok {
			fmt.Println(bold.Render(n), muted.Render("· "+id+" · its own name is "+own))
		} else {
			fmt.Println(bold.Render(own), muted.Render("· "+id+" · its own name"))
		}
		return nil
	}
	name := strings.Join(rest, " ")
	if isReset(rest) {
		name = ""
	}
	if err := provider.SetModelName(id, name); err != nil {
		return err
	}
	if name == "" {
		fmt.Println(green.Render("✓"), id, muted.Render("is called"), bold.Render(own), muted.Render("again"))
	} else {
		fmt.Println(green.Render("✓"), id, muted.Render("is called"), bold.Render(strings.Join(strings.Fields(name), " ")))
	}
	return nil
}

func modelEfforts(args []string) error {
	if len(args) == 0 {
		return fmt.Errorf("%s", modelUsage)
	}
	p, model, err := modelRef(args[0])
	if err != nil {
		return err
	}
	id := p.ID + "/" + model
	all := p.Efforts(model)
	rest := args[1:]
	if len(rest) == 0 {
		if len(all) == 0 {
			fmt.Println(muted.Render(id + " has no reasoning levels"))
			return nil
		}
		kept, narrowed := p.ModelEfforts()[model]
		for _, e := range all {
			if !narrowed || slices.Contains(kept, e) {
				fmt.Print(bold.Render(e), " ")
			} else {
				fmt.Print(faint.Render(e), " ")
			}
		}
		if narrowed {
			fmt.Println(muted.Render("· only the bold ones are offered · --reset offers them all"))
		} else {
			fmt.Println(muted.Render("· all offered"))
		}
		return nil
	}
	var levels []string
	if !isReset(rest) {
		for _, a := range rest {
			for _, l := range strings.FieldsFunc(a, func(r rune) bool { return r == ',' || r == ' ' || r == '/' }) {
				levels = append(levels, l)
			}
		}
	}
	if err := provider.SetModelEfforts(id, levels); err != nil {
		return err
	}
	if kept, ok := p.ModelEfforts()[model]; ok {
		fmt.Println(green.Render("✓"), id, muted.Render("offers"), bold.Render(strings.Join(kept, ", ")))
	} else {
		fmt.Println(green.Render("✓"), id, muted.Render("offers every level it has"), faint.Render(strings.Join(all, ", ")))
	}
	return nil
}

func modelNames() error {
	s := settings.Load()
	keys := slices.Sorted(maps.Keys(s.ModelNames))
	for k := range s.ModelEfforts {
		if !slices.Contains(keys, k) {
			keys = append(keys, k)
		}
	}
	slices.Sort(keys)
	if len(keys) == 0 {
		fmt.Println(muted.Render("no model is named or narrowed yet · magpie model name <provider/model> <name>"))
		return nil
	}
	w := 0
	for _, k := range keys {
		w = max(w, len(k))
	}
	for _, k := range keys {
		line := "  " + pad(k, w)
		if n := s.ModelNames[k]; n != "" {
			line += "  " + n
		}
		if es := s.ModelEfforts[k]; len(es) > 0 {
			line += "  " + muted.Render(strings.Join(es, "/"))
		}
		fmt.Println(line)
	}
	return nil
}
