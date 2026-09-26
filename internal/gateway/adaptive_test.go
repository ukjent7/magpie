package gateway

import (
	"encoding/json"
	"testing"
)

func TestAdaptiveThinking(t *testing.T) {
	for model, want := range map[string]bool{
		"claude-opus-5-5": true, "claude-opus-5": true, "claude-sonnet-4-6": true,
		"claude-opus-4-7": true, "anthropic.claude-sonnet-4.6-v1": true, "claude-sonnet-5": true,
		"claude-sonnet-4-5-20250929": false, "claude-sonnet-4-20250514": false, "claude-opus-4-1": false,
		"claude-3-7-sonnet-20250219": false, "deepseek-v4": false, "claude-haiku-4-5": false,
	} {
		if got := adaptiveOnly(model); got != want {
			t.Errorf("adaptiveOnly(%q) = %v", model, got)
		}
	}
	var out map[string]any
	json.Unmarshal(buildAnthropic(&Request{Thinking: true, Effort: "xhigh"}, "claude-opus-5-5"), &out)
	if th, _ := json.Marshal(out["thinking"]); string(th) != `{"type":"adaptive"}` {
		t.Errorf("thinking = %s", th)
	}
	if oc, _ := json.Marshal(out["output_config"]); string(oc) != `{"effort":"max"}` {
		t.Errorf("output_config = %s", oc)
	}
	json.Unmarshal(buildAnthropic(&Request{Thinking: true, Effort: "low"}, "claude-sonnet-4-5"), &out)
	if th, _ := json.Marshal(out["thinking"]); string(th) != `{"budget_tokens":4096,"type":"enabled"}` {
		t.Errorf("old model thinking = %s", th)
	}
}
