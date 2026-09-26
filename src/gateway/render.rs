// A whole reply, written out as the API the agent called in: what was said,
// what was thought, and what it asked to run — with the names that API
// gives those things.

use serde_json::{Value, json};

use super::ir::{self, Kind, Reply, args_of, args_string};

// chat is a Chat Completions reply. Its message carries the words, the
// reasoning some vendors keep beside them, and the calls to run.
pub fn chat(reply: &Reply, model: &str) -> Value {
    let mut content: Option<String> = None;
    let mut thought = String::new();
    let mut calls = Vec::new();
    for part in &reply.parts {
        match part.kind {
            Kind::Text => content = Some(content.unwrap_or_default() + &part.text),
            Kind::Thinking => thought.push_str(&part.text),
            Kind::ToolCall => calls.push(json!({
                "id": call_id(&part.id),
                "type": "function",
                "function": { "name": part.name, "arguments": args_string(part) },
            })),
            _ => {}
        }
    }
    let mut message = json!({ "role": "assistant", "content": content });
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    if !thought.is_empty() {
        message["reasoning_content"] = json!(thought);
    }
    json!({
        "id": chat_id(&reply.id),
        "object": "chat.completion",
        "created": ir::now(),
        "model": model_of(reply, model),
        "choices": [{ "index": 0, "message": message, "finish_reason": ir::stop_to_chat(&reply.stop) }],
        "usage": reply.usage.chat(),
    })
}

// responses is a Responses reply: one item per thing the model did, in the
// order it did them.
pub fn responses(reply: &Reply, model: &str) -> Value {
    let mut output = Vec::new();
    for part in &reply.parts {
        let item = match part.kind {
            Kind::Text => json!({
                "id": format!("msg_{}", ir::new_id()),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": part.text, "annotations": [] }],
            }),
            Kind::Thinking => json!({
                "id": format!("rs_{}", ir::new_id()),
                "type": "reasoning",
                "status": "completed",
                "summary": [{ "type": "summary_text", "text": part.text }],
            }),
            Kind::ToolCall => json!({
                "id": format!("fc_{}", ir::new_id()),
                "type": "function_call",
                "status": "completed",
                "call_id": call_id(&part.id),
                "name": part.name,
                "arguments": args_string(part),
            }),
            _ => continue,
        };
        output.push(item);
    }
    let mut id = reply.id.clone();
    if id.is_empty() {
        id = ir::new_id();
    }
    if !id.starts_with("resp_") {
        id = format!("resp_{id}");
    }
    let incomplete = matches!(reply.stop.as_str(), "length" | "filter").then(|| {
        json!({ "reason": if reply.stop == "filter" { "content_filter" } else { "max_output_tokens" } })
    });
    json!({
        "id": id,
        "object": "response",
        "created_at": ir::now(),
        "status": if incomplete.is_some() { "incomplete" } else { "completed" },
        "model": model_of(reply, model),
        "output": output,
        "usage": reply.usage.responses(),
        "incomplete_details": incomplete,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "error": null,
    })
}

// anthropic is a Messages reply: its content blocks, and why it stopped.
pub fn anthropic(reply: &Reply, model: &str) -> Value {
    let mut content = Vec::new();
    for part in &reply.parts {
        // an image the model was sent, or a result it was given, is nothing
        // a reply can send back
        let block = match part.kind {
            Kind::Text => json!({ "type": "text", "text": part.text }),
            Kind::Thinking => json!({
                "type": "thinking",
                "thinking": part.text,
                "signature": part.signature,
            }),
            Kind::ToolCall => json!({
                "type": "tool_use",
                "id": tool_id(&part.id),
                "name": part.name,
                "input": args_of(part),
            }),
            _ => continue,
        };
        content.push(block);
    }
    let id = if reply.id.is_empty() {
        format!("msg_{}", ir::new_id())
    } else {
        reply.id.clone()
    };
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model_of(reply, model),
        "content": content,
        "stop_reason": ir::stop_to_anthropic(&reply.stop),
        "stop_sequence": null,
        "usage": reply.usage.anthropic(),
    })
}

// model_of is what answered: a reply that names its model has said so, and
// the caller's guess is only a guess.
fn model_of(reply: &Reply, asked: &str) -> String {
    if reply.model.is_empty() {
        asked.to_owned()
    } else {
        reply.model.clone()
    }
}

fn call_id(id: &str) -> String {
    if id.is_empty() {
        format!("call_{}", ir::new_id())
    } else {
        id.to_owned()
    }
}

fn tool_id(id: &str) -> String {
    if id.is_empty() {
        format!("toolu_{}", ir::new_id())
    } else {
        id.to_owned()
    }
}

fn chat_id(id: &str) -> String {
    let id = if id.is_empty() {
        ir::new_id()
    } else {
        id.to_owned()
    };
    if id.starts_with("chatcmpl-") {
        id
    } else {
        format!("chatcmpl-{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ir::{Part, Usage};

    fn part(kind: Kind, text: &str) -> Part {
        Part {
            kind,
            text: text.to_owned(),
            ..Part::default()
        }
    }

    #[test]
    fn a_chat_reply_says_what_it_said() {
        let reply = Reply {
            id: "abc".to_owned(),
            parts: vec![
                part(Kind::Text, "hel"),
                part(Kind::Text, "lo"),
                part(Kind::Thinking, "because"),
            ],
            stop: "stop".to_owned(),
            usage: Usage {
                input: 5,
                output: 2,
                cache_read: 3,
                ..Usage::default()
            },
            ..Reply::default()
        };
        let got = chat(&reply, "asked");
        assert_eq!(got["id"], json!("chatcmpl-abc"));
        assert_eq!(got["model"], json!("asked"));
        assert_eq!(got["choices"][0]["message"]["content"], json!("hello"));
        assert_eq!(
            got["choices"][0]["message"]["reasoning_content"],
            json!("because")
        );
        assert_eq!(got["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(got["usage"]["prompt_tokens"], json!(8));
        assert_eq!(got["usage"]["total_tokens"], json!(10));
        assert_eq!(got["usage"]["prompt_tokens_details"]["cached_tokens"], json!(3));
    }

    #[test]
    fn a_chat_reply_that_calls_names_the_call() {
        let reply = Reply {
            parts: vec![Part {
                kind: Kind::ToolCall,
                id: "c1".to_owned(),
                name: "read".to_owned(),
                args: Some(json!({"a": 1})),
                ..Part::default()
            }],
            stop: "tool".to_owned(),
            ..Reply::default()
        };
        let got = chat(&reply, "m");
        assert!(got["choices"][0]["message"]["content"].is_null());
        assert_eq!(got["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(
            got["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"a":1}"#)
        );
        assert!(got["id"].as_str().unwrap().starts_with("chatcmpl-"));
    }

    #[test]
    fn a_responses_reply_is_one_item_per_thing_done() {
        let reply = Reply {
            id: "resp_x".to_owned(),
            model: "answered".to_owned(),
            parts: vec![
                part(Kind::Thinking, "hmm"),
                part(Kind::Text, "yes"),
                Part {
                    kind: Kind::ToolCall,
                    name: "run".to_owned(),
                    args: Some(json!({})),
                    ..Part::default()
                },
            ],
            stop: "length".to_owned(),
            ..Reply::default()
        };
        let got = responses(&reply, "asked");
        assert_eq!(got["id"], json!("resp_x"));
        assert_eq!(got["model"], json!("answered"));
        assert_eq!(got["status"], json!("incomplete"));
        assert_eq!(got["incomplete_details"]["reason"], json!("max_output_tokens"));
        let output = got["output"].as_array().unwrap();
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["type"], json!("reasoning"));
        assert_eq!(output[1]["content"][0]["type"], json!("output_text"));
        assert_eq!(output[2]["type"], json!("function_call"));
        assert!(output[2]["call_id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(output[2]["arguments"], json!("{}"));
    }

    #[test]
    fn a_refusal_is_incomplete_too() {
        let reply = Reply {
            stop: "filter".to_owned(),
            ..Reply::default()
        };
        let got = responses(&reply, "m");
        assert_eq!(got["status"], json!("incomplete"));
        assert_eq!(got["incomplete_details"]["reason"], json!("content_filter"));
        let stopped = responses(&Reply::default(), "m");
        assert_eq!(stopped["status"], json!("completed"));
        assert!(stopped["incomplete_details"].is_null());
    }

    #[test]
    fn a_messages_reply_keeps_its_blocks_apart() {
        let reply = Reply {
            id: "msg_1".to_owned(),
            parts: vec![
                part(Kind::Text, "look"),
                Part {
                    kind: Kind::Thinking,
                    text: "why".to_owned(),
                    signature: "sig".to_owned(),
                    ..Part::default()
                },
                Part {
                    kind: Kind::ToolCall,
                    id: "t1".to_owned(),
                    name: "read".to_owned(),
                    args: Some(json!({"path": "/x"})),
                    ..Part::default()
                },
            ],
            stop: "tool".to_owned(),
            ..Reply::default()
        };
        let got = anthropic(&reply, "claude");
        assert_eq!(got["id"], json!("msg_1"));
        assert_eq!(got["stop_reason"], json!("tool_use"));
        assert!(got["stop_sequence"].is_null());
        let content = got["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], json!("text"));
        assert_eq!(content[1]["signature"], json!("sig"));
        assert_eq!(content[2]["input"], json!({"path": "/x"}));
        assert_eq!(got["usage"]["input_tokens"], json!(0));
    }

    #[test]
    fn an_end_of_its_own_words() {
        assert_eq!(ir::stop_to_anthropic("stop"), "end_turn");
        assert_eq!(ir::stop_to_anthropic("length"), "max_tokens");
        assert_eq!(ir::stop_from_anthropic("model_context_window_exceeded"), "length");
        assert_eq!(ir::stop_from_chat("function_call"), "tool");
    }
}
