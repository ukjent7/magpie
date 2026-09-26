// A call as it arrives in each API, read into the shape the subscription
// backends speak: the messages and their parts, the tools offered, and how
// the caller asked that they be used.

use serde_json::Value;

use super::ir::{Kind, Message, Part, Request, Tool, args_object, text_of};
use crate::grouprule::{effort_of, effort_of_budget, string_or_text};

// chat is an OpenAI Chat Completions call.
pub fn chat(body: &Value) -> Request {
    let mut r = Request {
        model: word(body, "model"),
        max_tokens: whole(body, "max_completion_tokens"),
        temp: real(body, "temperature"),
        top_p: real(body, "top_p"),
        stream: said(body, "stream"),
        effort: effort_of(&word(body, "reasoning_effort")),
        parallel: told(body, "parallel_tool_calls"),
        ..Request::default()
    };
    if r.max_tokens == 0 {
        r.max_tokens = whole(body, "max_tokens");
    }
    // asking for an effort is asking the model to think
    r.thinking = !r.effort.is_empty();
    r.stop = stops(body.get("stop"));
    let mut system: Vec<String> = Vec::new();
    for message in rows(body, "messages") {
        match word(message, "role").as_str() {
            "system" | "developer" => system.push(string_or_text(message.get("content"))),
            "user" => r.messages.push(Message {
                role: "user".to_owned(),
                parts: chat_parts(message.get("content")),
            }),
            "assistant" => {
                let mut parts = Vec::new();
                let thought = word(message, "reasoning_content");
                if !thought.is_empty() {
                    parts.push(Part {
                        kind: Kind::Thinking,
                        text: thought,
                        ..Part::default()
                    });
                }
                parts.extend(chat_parts(message.get("content")));
                for call in rows(message, "tool_calls") {
                    let function = call.get("function").cloned().unwrap_or(Value::Null);
                    parts.push(Part {
                        kind: Kind::ToolCall,
                        id: word(call, "id"),
                        name: word(&function, "name"),
                        args: Some(args_object(&word(&function, "arguments"))),
                        ..Part::default()
                    });
                }
                r.messages.push(Message {
                    role: "assistant".to_owned(),
                    parts,
                });
            }
            "tool" => r.messages.push(Message {
                role: "user".to_owned(),
                parts: vec![Part {
                    kind: Kind::ToolResult,
                    call_id: word(message, "tool_call_id"),
                    text: string_or_text(message.get("content")),
                    ..Part::default()
                }],
            }),
            _ => {}
        }
    }
    r.system = system.join("\n\n");
    for tool in rows(body, "tools") {
        let kind = word(tool, "type");
        if !kind.is_empty() && kind != "function" {
            continue;
        }
        let function = tool.get("function").cloned().unwrap_or(Value::Null);
        r.tools.push(Tool {
            name: word(&function, "name"),
            description: word(&function, "description"),
            schema: function.get("parameters").cloned().unwrap_or(Value::Null),
        });
    }
    r.tool_choice = choice(body.get("tool_choice"), &["function", "name"]);
    r
}

// responses is an OpenAI Responses call.
pub fn responses(body: &Value) -> Request {
    let mut r = Request {
        model: word(body, "model"),
        system: word(body, "instructions"),
        max_tokens: whole(body, "max_output_tokens"),
        temp: real(body, "temperature"),
        top_p: real(body, "top_p"),
        stream: said(body, "stream"),
        parallel: told(body, "parallel_tool_calls"),
        ..Request::default()
    };
    if let Some(reasoning) = body.get("reasoning") {
        r.effort = effort_of(&word(reasoning, "effort"));
        r.thinking = true;
    }
    match body.get("input") {
        Some(Value::String(text)) => r.messages.push(Message {
            role: "user".to_owned(),
            parts: vec![text_part(text)],
        }),
        Some(Value::Array(items)) => {
            for item in items {
                let kind = word(item, "type");
                let role = word(item, "role");
                // a message may be sent with its type left out
                if kind == "message" || (kind.is_empty() && !role.is_empty()) {
                    let parts = responses_parts(item.get("content"));
                    if matches!(role.as_str(), "system" | "developer") {
                        let told = text_of(&parts);
                        if !told.is_empty() {
                            if !r.system.is_empty() {
                                r.system.push_str("\n\n");
                            }
                            r.system.push_str(&told);
                        }
                        continue;
                    }
                    r.messages.push(Message {
                        role: if role == "assistant" {
                            "assistant".to_owned()
                        } else {
                            "user".to_owned()
                        },
                        parts,
                    });
                    continue;
                }
                match kind.as_str() {
                    "function_call" => r.messages.push(Message {
                        role: "assistant".to_owned(),
                        parts: vec![Part {
                            kind: Kind::ToolCall,
                            id: word(item, "call_id"),
                            name: word(item, "name"),
                            args: Some(args_object(&word(item, "arguments"))),
                            ..Part::default()
                        }],
                    }),
                    "function_call_output" => r.messages.push(Message {
                        role: "user".to_owned(),
                        parts: vec![Part {
                            kind: Kind::ToolResult,
                            call_id: word(item, "call_id"),
                            text: string_or_text(item.get("output")),
                            ..Part::default()
                        }],
                    }),
                    "reasoning" => {
                        let thought: String = rows(item, "summary")
                            .iter()
                            .map(|part| word(part, "text"))
                            .collect();
                        if !thought.is_empty() {
                            r.messages.push(Message {
                                role: "assistant".to_owned(),
                                parts: vec![Part {
                                    kind: Kind::Thinking,
                                    text: thought,
                                    ..Part::default()
                                }],
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    r.messages = merge_turns(std::mem::take(&mut r.messages));
    for tool in rows(body, "tools") {
        if word(tool, "type") != "function" {
            continue;
        }
        r.tools.push(Tool {
            name: word(tool, "name"),
            description: word(tool, "description"),
            schema: tool.get("parameters").cloned().unwrap_or(Value::Null),
        });
    }
    r.tool_choice = choice(body.get("tool_choice"), &["name"]);
    r
}

// anthropic is an Anthropic Messages call.
pub fn anthropic(body: &Value) -> Request {
    let mut r = Request {
        model: word(body, "model"),
        system: string_or_text(body.get("system")),
        max_tokens: whole(body, "max_tokens"),
        temp: real(body, "temperature"),
        top_p: real(body, "top_p"),
        stop: rows(body, "stop_sequences")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        stream: said(body, "stream"),
        ..Request::default()
    };
    for message in rows(body, "messages") {
        let mut parts = Vec::new();
        match message.get("content") {
            Some(Value::String(text)) => {
                if !text.is_empty() {
                    parts.push(text_part(text));
                }
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match word(block, "type").as_str() {
                        "text" => parts.push(text_part(word(block, "text"))),
                        "image" => {
                            if let Some(source) = block.get("source") {
                                parts.push(Part {
                                    kind: Kind::Image,
                                    media_type: word(source, "media_type"),
                                    data: word(source, "data"),
                                    url: word(source, "url"),
                                    ..Part::default()
                                });
                            }
                        }
                        "tool_use" => parts.push(Part {
                            kind: Kind::ToolCall,
                            id: word(block, "id"),
                            name: word(block, "name"),
                            args: block.get("input").cloned(),
                            ..Part::default()
                        }),
                        "tool_result" => parts.push(Part {
                            kind: Kind::ToolResult,
                            call_id: word(block, "tool_use_id"),
                            text: string_or_text(block.get("content")),
                            is_error: said(block, "is_error"),
                            ..Part::default()
                        }),
                        "thinking" => parts.push(Part {
                            kind: Kind::Thinking,
                            text: word(block, "thinking"),
                            signature: word(block, "signature"),
                            ..Part::default()
                        }),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        r.messages.push(Message {
            role: word(message, "role"),
            parts,
        });
    }
    for tool in rows(body, "tools") {
        let kind = word(tool, "type");
        let schema = tool.get("input_schema");
        if !kind.is_empty() && kind != "custom" && schema.is_none() {
            continue; // server-side tools (web search…) mean nothing elsewhere
        }
        r.tools.push(Tool {
            name: word(tool, "name"),
            description: word(tool, "description"),
            schema: schema.cloned().unwrap_or(Value::Null),
        });
    }
    if let Some(choice) = body.get("tool_choice") {
        r.tool_choice = match word(choice, "type").as_str() {
            "auto" | "none" => word(choice, "type"),
            "any" => "required".to_owned(),
            "tool" => format!("name:{}", word(choice, "name")),
            _ => String::new(),
        };
        if said(choice, "disable_parallel_tool_use") {
            r.parallel = Some(false);
        }
    }
    // output_config's effort says how hard the model thinks only when it was
    // asked to think: Claude Code's title requests carry effort but no
    // thinking, and reasoning_effort would turn it on upstream
    if let Some(thinking) = body.get("thinking")
        && matches!(word(thinking, "type").as_str(), "enabled" | "adaptive")
    {
        r.thinking = true;
        r.effort = effort_of_budget(whole(thinking, "budget_tokens"));
        if let Some(output) = body.get("output_config") {
            let asked = effort_of(&word(output, "effort"));
            if !asked.is_empty() {
                r.effort = asked;
            }
        }
    }
    r
}

// chat_parts is a Chat message's content: a string, or an array of parts.
fn chat_parts(content: Option<&Value>) -> Vec<Part> {
    let items = match words(content) {
        Some(given) => return given,
        None => items_of(content),
    };
    let mut parts = Vec::new();
    for item in items {
        match word(item, "type").as_str() {
            "text" => parts.push(text_part(word(item, "text"))),
            "image_url" => parts.push(image_part(
                item.get("image_url")
                    .map_or(String::new(), |url| word(url, "url")),
            )),
            _ => {}
        }
    }
    parts
}

// responses_parts is a Responses message's content, whose text says whose
// turn it is in.
fn responses_parts(content: Option<&Value>) -> Vec<Part> {
    let items = match words(content) {
        Some(given) => return given,
        None => items_of(content),
    };
    let mut parts = Vec::new();
    for item in items {
        match word(item, "type").as_str() {
            "input_text" | "output_text" | "text" => parts.push(text_part(word(item, "text"))),
            "input_image" => {
                let url = word(item, "image_url");
                if !url.is_empty() {
                    parts.push(image_part(url));
                }
            }
            _ => {}
        }
    }
    parts
}

// words is content that is only words: the parts of a message that said
// nothing else.
fn words(content: Option<&Value>) -> Option<Vec<Part>> {
    let text = content?.as_str()?;
    Some(if text.is_empty() {
        Vec::new()
    } else {
        vec![text_part(text)]
    })
}

// items_of is content that is an array of blocks, and nothing else.
fn items_of(content: Option<&Value>) -> &[Value] {
    match content {
        Some(Value::Array(items)) => items,
        _ => &[],
    }
}

// image_part reads a data: URL into an inline image, or keeps the URL.
fn image_part(url: String) -> Part {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((meta, data)) = rest.split_once(',')
    {
        return Part {
            kind: Kind::Image,
            media_type: meta.trim_end_matches(";base64").to_owned(),
            data: data.to_owned(),
            ..Part::default()
        };
    }
    Part {
        kind: Kind::Image,
        url,
        ..Part::default()
    }
}

// merge_turns joins messages of one role sent as several: the Responses API
// splits an assistant turn into one item per part.
pub fn merge_turns(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for message in messages {
        if let Some(last) = out.last_mut().filter(|last| last.role == message.role) {
            last.parts.extend(message.parts);
            continue;
        }
        out.push(message);
    }
    out
}

// choice is how a caller asked its tools be used: a word of its own, or the
// one tool it named — which Chat keeps under its function and Responses at
// the top, so name_path says where to look.
fn choice(value: Option<&Value>, name_path: &[&str]) -> String {
    match value {
        Some(Value::String(one)) => one.clone(),
        Some(value) => name_path
            .iter()
            .fold(Some(value), |at, key| at.and_then(|at| at.get(*key)))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map_or(String::new(), |name| format!("name:{name}")),
        None => String::new(),
    }
}

// stops is a stop sequence, or the list of them, as the client sent it.
fn stops(stop: Option<&Value>) -> Vec<String> {
    match stop {
        Some(Value::String(one)) if !one.is_empty() => vec![one.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn text_part(text: impl Into<String>) -> Part {
    Part {
        kind: Kind::Text,
        text: text.into(),
        ..Part::default()
    }
}

fn word(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn whole(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or_default()
}

fn real(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn told(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn said(value: &Value, key: &str) -> bool {
    told(value, key).unwrap_or_default()
}

// rows is the array a key holds, or none of one when it holds anything else,
// which is what a client that sends its parts as a bare string does.
fn rows<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn said(r: &Request) -> Vec<String> {
        r.messages
            .iter()
            .map(|message| text_of(&message.parts))
            .collect()
    }

    #[test]
    fn a_chat_call_is_read_whole() {
        let r = chat(&json!({
            "model": "gpt-5",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [{"type": "text", "text": "hi"},
                                              {"type": "image_url", "image_url": {"url": "data:image/png;base64,YY"}}]},
                {"role": "assistant", "content": "hello", "reasoning_content": "think",
                 "tool_calls": [{"id": "c1", "function": {"name": "read", "arguments": "{\"a\":1}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "done"}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}},
                      {"type": "mcp", "function": {"name": "ignored"}}],
            "tool_choice": {"type": "function", "function": {"name": "read"}},
            "max_completion_tokens": 900,
            "temperature": 0.5,
            "stop": ["a", "b"],
            "stream": true,
            "reasoning_effort": "minimal",
            "parallel_tool_calls": false
        }));
        assert_eq!(r.system, "be brief");
        assert_eq!(said(&r), ["hi", "hello", ""]);
        let user = &r.messages[0].parts;
        assert_eq!(user.len(), 2);
        assert_eq!(user[1].media_type, "image/png");
        assert_eq!(user[1].data, "YY");
        let assistant = &r.messages[1].parts;
        assert_eq!(assistant.len(), 3);
        assert_eq!(assistant[0].kind, Kind::Thinking);
        assert_eq!(assistant[2].name, "read");
        assert_eq!(assistant[2].args, Some(json!({"a": 1})));
        assert_eq!(r.messages[2].parts[0].kind, Kind::ToolResult);
        assert_eq!(r.messages[2].parts[0].call_id, "c1");
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].schema, json!({"type": "object"}));
        assert_eq!(r.tool_choice, "name:read");
        assert_eq!(r.max_tokens, 900);
        assert_eq!(r.temp, Some(0.5));
        assert_eq!(r.stop, ["a", "b"]);
        assert!(r.stream && r.thinking);
        assert_eq!(r.effort, "low");
        assert_eq!(r.parallel, Some(false));
    }

    #[test]
    fn the_newer_way_to_cap_an_answer_wins() {
        assert_eq!(chat(&json!({"max_tokens": 12})).max_tokens, 12);
        assert_eq!(
            chat(&json!({"max_tokens": 12, "max_completion_tokens": 34})).max_tokens,
            34
        );
        assert_eq!(chat(&json!({"temperature": 0})).temp, Some(0.0));
    }

    #[test]
    fn a_responses_turn_split_into_items_is_one_message() {
        let r = responses(&json!({
            "model": "gpt-5",
            "instructions": "be terse",
            "input": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thought"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "said"}]},
                {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": [{"type": "text", "text": "ok"}]},
                {"type": "message", "role": "system", "content": "and more"}
            ],
            "tools": [{"type": "function", "name": "read"}, {"type": "web_search"}],
            "tool_choice": {"type": "function", "name": "read"},
            "reasoning": {"effort": "xhigh"}
        }));
        assert_eq!(r.system, "be terse\n\nand more");
        assert_eq!(said(&r), ["said", ""], "a result is no words");
        assert_eq!(r.messages[0].parts.len(), 3, "thought, words and a call");
        assert_eq!(r.messages[0].parts[0].kind, Kind::Thinking);
        assert_eq!(r.messages[1].parts[0].kind, Kind::ToolResult);
        assert_eq!(r.messages[1].parts[0].text, "ok");
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tool_choice, "name:read");
        assert_eq!(r.effort, "xhigh");
        assert!(r.thinking);
    }

    #[test]
    fn a_plain_string_input_is_one_user_turn() {
        assert_eq!(
            said(&responses(&json!({"input": "just this"}))),
            ["just this"]
        );
    }

    #[test]
    fn an_anthropic_call_keeps_what_its_blocks_hold() {
        let r = anthropic(&json!({
            "model": "claude",
            "system": [{"type": "text", "text": "be "}, {"type": "text", "text": "kind"}],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "WA"}},
                    {"type": "tool_result", "tool_use_id": "c1", "content": "was here", "is_error": true}
                ]
            }],
            "tools": [{"name": "read", "input_schema": {"type": "object"}},
                      {"type": "web_search_20250305", "name": "web"}],
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "max_tokens": 1000,
            "thinking": {"type": "enabled", "budget_tokens": 5000},
            "output_config": {"effort": "high"}
        }));
        assert_eq!(r.system, "be kind");
        let parts = &r.messages[0].parts;
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].data, "WA");
        assert!(parts[2].is_error);
        assert_eq!(parts[2].call_id, "c1");
        assert_eq!(r.tools.len(), 1, "a server-side tool means nothing here");
        assert_eq!(r.tool_choice, "required");
        assert_eq!(r.parallel, Some(false));
        assert_eq!(r.effort, "high");
        assert!(r.thinking);
        assert_eq!(r.max_tokens, 1000);
    }

    #[test]
    fn effort_without_thinking_is_nothing_asked() {
        let r = anthropic(&json!({"output_config": {"effort": "high"}, "max_tokens": 8}));
        assert_eq!(r.effort, "");
        assert!(!r.thinking);
        let r = anthropic(
            &json!({"thinking": {"type": "enabled", "budget_tokens": 12001}, "max_tokens": 8}),
        );
        assert_eq!(r.effort, "high");
    }

    #[test]
    fn a_data_url_without_a_comma_stays_a_url() {
        let part = image_part("data:nonsense".to_owned());
        assert_eq!(part.url, "data:nonsense");
        assert!(part.data.is_empty());
        assert_eq!(
            image_part("https://x/y.png".to_owned()).url,
            "https://x/y.png"
        );
    }

    #[test]
    fn a_tool_choice_is_a_word_or_the_tool_it_names() {
        assert_eq!(choice(Some(&json!("auto")), &["function", "name"]), "auto");
        assert_eq!(
            choice(Some(&json!({"type": "none"})), &["function", "name"]),
            ""
        );
        assert_eq!(
            choice(
                Some(&json!({"type": "function", "function": {"name": "x"}})),
                &["function", "name"]
            ),
            "name:x"
        );
        assert_eq!(
            choice(Some(&json!({"type": "function", "name": "y"})), &["name"]),
            "name:y"
        );
        assert_eq!(choice(Some(&json!(null)), &["function", "name"]), "");
        assert_eq!(choice(None, &["function", "name"]), "");
    }
}
