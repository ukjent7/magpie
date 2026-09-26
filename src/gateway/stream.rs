// A reply as it is streamed: the events a model's answer is made of,
// written out in the protocol the agent is reading. Each encoder keeps what
// it has already announced, so the frames it adds say only what is new.

use serde_json::{Value, json};

use super::ApiProtocol;
use super::ir::{self, Collector, Event, EventKind, Kind};

// encoder writes one API's stream.
pub enum Encoder {
    Chat(Chat),
    Responses(Responses),
    Anthropic(Anthropic),
}

impl Encoder {
    pub fn new(api: ApiProtocol, model: &str) -> Encoder {
        let model = model.to_owned();
        match api {
            ApiProtocol::Chat => Encoder::Chat(Chat {
                model,
                id: String::new(),
                created: 0,
                started: false,
                tool: -1,
                col: Collector::default(),
            }),
            ApiProtocol::Responses => Encoder::Responses(Responses {
                model,
                id: String::new(),
                created: 0,
                seq: 0,
                started: false,
                item: -1,
                open: None,
                item_id: String::new(),
                call_id: String::new(),
                call_name: String::new(),
                text: String::new(),
                output: Vec::new(),
                col: Collector::default(),
            }),
            ApiProtocol::Anthropic => Encoder::Anthropic(Anthropic {
                model,
                index: 0,
                open: None,
                args: false,
                started: false,
                col: Collector::default(),
            }),
        }
    }

    pub fn event(&mut self, ev: Event) -> Vec<u8> {
        match self {
            Self::Chat(e) => e.event(ev),
            Self::Responses(e) => e.event(ev),
            Self::Anthropic(e) => e.event(ev),
        }
    }

    pub fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::Chat(e) => e.finish(),
            Self::Responses(e) => e.finish(),
            Self::Anthropic(e) => e.finish(),
        }
    }
}

// frame is one event of the stream, in the form every client reads.
fn frame(name: &str, data: Value) -> Vec<u8> {
    said(name, &data.to_string())
}

// said is one event whose data is already written.
fn said(name: &str, data: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if !name.is_empty() {
        out.extend_from_slice(format!("event: {name}\n").as_bytes());
    }
    out.extend_from_slice(format!("data: {data}\n\n").as_bytes());
    out
}

// ---- Chat Completions --------------------------------------------------------

struct Chat {
    model: String,
    id: String,
    created: i64,
    started: bool,
    // where the open tool call sits, -1 for none
    tool: i64,
    col: Collector,
}

impl Chat {
    fn event(&mut self, ev: Event) -> Vec<u8> {
        let mut out = if self.started || ev.kind == EventKind::Start {
            Vec::new()
        } else {
            self.begin(&Event::default())
        };
        match ev.kind {
            EventKind::Start => out.extend(self.begin(&ev)),
            EventKind::Text if !ev.text.is_empty() => {
                out.extend(self.chunk(Some(json!({ "content": &ev.text })), None, None));
            }
            EventKind::Think if !ev.text.is_empty() => out.extend(self.chunk(
                Some(json!({ "reasoning_content": &ev.text })),
                None,
                None,
            )),
            EventKind::ToolStart => {
                self.tool += 1;
                let id = if ev.id.is_empty() {
                    format!("call_{}", ir::new_id())
                } else {
                    ev.id.clone()
                };
                out.extend(self.chunk(
                    Some(json!({ "tool_calls": [{
                        "index": self.tool,
                        "id": id,
                        "type": "function",
                        "function": { "name": &ev.name, "arguments": "" },
                    }] })),
                    None,
                    None,
                ));
            }
            EventKind::ToolArgs if self.tool >= 0 && !ev.text.is_empty() => out.extend(
                self.chunk(
                    Some(json!({ "tool_calls": [{
                        "index": self.tool,
                        "function": { "arguments": &ev.text },
                    }] })),
                    None,
                    None,
                ),
            ),
            EventKind::Error => out.extend(frame(
                "",
                json!({ "error": { "message": &ev.text, "type": "api_error" } }),
            )),
            _ => {}
        }
        // what was said is the reply, however it was framed
        self.col.add(ev);
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.started {
            out.extend(self.begin(&Event::default()));
        }
        let reply = self.col.finish();
        out.extend(self.chunk(None, Some(ir::stop_to_chat(&reply.stop)), None));
        out.extend(self.chunk(None, None, Some(reply.usage.chat())));
        out.extend(said("", "[DONE]"));
        out
    }

    // begin announces the reply, whose id Chat wants prefixed.
    fn begin(&mut self, ev: &Event) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        self.tool = -1;
        self.created = ir::now();
        self.id = if ev.msg_id.is_empty() {
            ir::new_id()
        } else {
            ev.msg_id.clone()
        };
        if !self.id.starts_with("chatcmpl-") {
            self.id = format!("chatcmpl-{}", self.id);
        }
        if !ev.model.is_empty() {
            self.model.clone_from(&ev.model);
        }
        self.chunk(Some(json!({ "role": "assistant", "content": "" })), None, None)
    }

    fn chunk(&self, delta: Option<Value>, finish: Option<&str>, usage: Option<Value>) -> Vec<u8> {
        let mut choices = Vec::new();
        if delta.is_some() || finish.is_some() {
            choices.push(json!({
                "index": 0,
                "delta": delta.unwrap_or_else(|| json!({})),
                "finish_reason": finish,
            }));
        }
        frame(
            "",
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": choices,
                "usage": usage,
            }),
        )
    }
}

// ---- Responses ---------------------------------------------------------------

struct Responses {
    model: String,
    id: String,
    created: i64,
    seq: i64,
    started: bool,
    // which output item is open, -1 before the first
    item: i64,
    open: Option<Kind>,
    item_id: String,
    // the call the open item was made for, whose arguments are arriving
    call_id: String,
    call_name: String,
    text: String,
    output: Vec<Value>,
    col: Collector,
}

impl Responses {
    fn event(&mut self, mut ev: Event) -> Vec<u8> {
        let mut out = if self.started || ev.kind == EventKind::Start {
            Vec::new()
        } else {
            self.begin(&Event::default())
        };
        match ev.kind {
            EventKind::Start => out.extend(self.begin(&ev)),
            EventKind::Text => {
                if ev.text.is_empty() {
                    return out;
                }
                if self.open != Some(Kind::Text) {
                    out.extend(self.open_item(
                        Kind::Text,
                        "msg_",
                        json!({ "type": "message", "role": "assistant", "content": [] }),
                    ));
                    out.extend(self.send(
                        "response.content_part.added",
                        json!({
                            "item_id": &self.item_id,
                            "output_index": self.item,
                            "content_index": 0,
                            "part": { "type": "output_text", "text": "", "annotations": [], "logprobs": [] },
                        }),
                    ));
                }
                self.text.push_str(&ev.text);
                out.extend(self.send(
                    "response.output_text.delta",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "content_index": 0,
                        "delta": &ev.text,
                        "logprobs": [],
                    }),
                ));
            }
            EventKind::Think => {
                if ev.text.is_empty() {
                    return out;
                }
                if self.open != Some(Kind::Thinking) {
                    out.extend(self.open_item(
                        Kind::Thinking,
                        "rs_",
                        json!({ "type": "reasoning", "summary": [] }),
                    ));
                    out.extend(self.send(
                        "response.reasoning_summary_part.added",
                        json!({
                            "item_id": &self.item_id,
                            "output_index": self.item,
                            "summary_index": 0,
                            "part": { "type": "summary_text", "text": "" },
                        }),
                    ));
                }
                self.text.push_str(&ev.text);
                out.extend(self.send(
                    "response.reasoning_summary_text.delta",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "summary_index": 0,
                        "delta": &ev.text,
                    }),
                ));
            }
            EventKind::ToolStart => {
                // the item, the reply and the result all name the one call
                if ev.id.is_empty() {
                    ev.id = format!("call_{}", ir::new_id());
                }
                let (id, called) = (ev.id.clone(), ev.name.clone());
                out.extend(self.open_item(
                    Kind::ToolCall,
                    "fc_",
                    json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": called,
                        "arguments": "",
                    }),
                ));
                self.call_id = id;
                self.call_name = called;
            }
            EventKind::ToolArgs if self.open == Some(Kind::ToolCall) && !ev.text.is_empty() => {
                self.text.push_str(&ev.text);
                out.extend(self.send(
                    "response.function_call_arguments.delta",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "delta": &ev.text,
                    }),
                ));
            }
            EventKind::Error => {
                out.extend(self.close_item());
                let failed = self.response(
                    "failed",
                    Some(json!({ "error": { "code": "server_error", "message": &ev.text } })),
                );
                out.extend(self.send("response.failed", json!({ "response": failed })));
            }
            _ => {}
        }
        self.col.add(ev);
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.started {
            out.extend(self.begin(&Event::default()));
        }
        out.extend(self.close_item());
        let reply = self.col.finish();
        let (status, typ, reason) = match reply.stop.as_str() {
            "length" => ("incomplete", "response.incomplete", Some("max_output_tokens")),
            "filter" => ("incomplete", "response.incomplete", Some("content_filter")),
            _ => ("completed", "response.completed", None),
        };
        let extra = json!({
            "usage": reply.usage.responses(),
            "incomplete_details": reason.map(|reason| json!({ "reason": reason })),
        });
        let response = self.response(status, Some(extra));
        out.extend(self.send(typ, json!({ "response": response })));
        out
    }

    fn begin(&mut self, ev: &Event) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        self.item = -1;
        self.created = ir::now();
        self.id = if ev.msg_id.is_empty() {
            ir::new_id()
        } else {
            ev.msg_id.clone()
        };
        if !self.id.starts_with("resp_") {
            self.id = format!("resp_{}", self.id);
        }
        if !ev.model.is_empty() {
            self.model.clone_from(&ev.model);
        }
        let mut out = Vec::new();
        let created = self.response("in_progress", None);
        out.extend(self.send("response.created", json!({ "response": created })));
        let in_progress = self.response("in_progress", None);
        out.extend(self.send("response.in_progress", json!({ "response": in_progress })));
        out
    }

    // open_item closes what went before, and announces this one.
    fn open_item(&mut self, kind: Kind, prefix: &str, mut item: Value) -> Vec<u8> {
        let mut out = self.close_item();
        self.item += 1;
        self.open = Some(kind);
        self.item_id = format!("{prefix}{}", ir::new_id());
        if let Some(object) = item.as_object_mut() {
            object.insert("id".to_owned(), json!(self.item_id));
            object.insert("status".to_owned(), json!("in_progress"));
        }
        out.extend(self.send(
            "response.output_item.added",
            json!({ "output_index": self.item, "item": item }),
        ));
        out
    }

    // close_item ends the open item so the client can read it whole.
    fn close_item(&mut self) -> Vec<u8> {
        let Some(open) = self.open else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let text = std::mem::take(&mut self.text);
        let item = match open {
            Kind::Text => {
                out.extend(self.send(
                    "response.output_text.done",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "content_index": 0,
                        "text": text,
                        "logprobs": [],
                    }),
                ));
                let part = json!({
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                    "logprobs": [],
                });
                out.extend(self.send(
                    "response.content_part.done",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "content_index": 0,
                        "part": part,
                    }),
                ));
                json!({
                    "id": &self.item_id,
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [part],
                })
            }
            Kind::Thinking => {
                out.extend(self.send(
                    "response.reasoning_summary_text.done",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "summary_index": 0,
                        "text": text,
                    }),
                ));
                let part = json!({ "type": "summary_text", "text": text });
                out.extend(self.send(
                    "response.reasoning_summary_part.done",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "summary_index": 0,
                        "part": part,
                    }),
                ));
                json!({
                    "id": &self.item_id,
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [part],
                })
            }
            _ => {
                let arguments = ir::args_object(&text).to_string();
                let item = json!({
                    "id": &self.item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": &self.call_id,
                    "name": &self.call_name,
                    "arguments": arguments,
                });
                out.extend(self.send(
                    "response.function_call_arguments.done",
                    json!({
                        "item_id": &self.item_id,
                        "output_index": self.item,
                        "call_id": &self.call_id,
                        "name": &self.call_name,
                        "arguments": arguments,
                    }),
                ));
                item
            }
        };
        out.extend(self.send(
            "response.output_item.done",
            json!({ "output_index": self.item, "item": item }),
        ));
        self.output.push(item);
        self.open = None;
        out
    }

    fn send(&mut self, typ: &str, mut fields: Value) -> Vec<u8> {
        if let Some(object) = fields.as_object_mut() {
            object.insert("type".to_owned(), json!(typ));
            object.insert("sequence_number".to_owned(), json!(self.seq));
        }
        self.seq += 1;
        frame(typ, fields)
    }

    // response is the reply as it stands, which every event about it carries.
    fn response(&self, status: &str, extra: Option<Value>) -> Value {
        let mut out = json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created,
            "status": status,
            "model": self.model,
            "output": self.output,
            "parallel_tool_calls": true,
            "tool_choice": "auto",
            "tools": [],
        });
        if let (Some(object), Some(more)) =
            (out.as_object_mut(), extra.as_ref().and_then(Value::as_object))
        {
            for (key, value) in more {
                object.insert(key.clone(), value.clone());
            }
        }
        out
    }
}

// ---- Messages ----------------------------------------------------------------

struct Anthropic {
    model: String,
    index: i64,
    open: Option<Kind>,
    // the open tool block has been given its arguments
    args: bool,
    started: bool,
    col: Collector,
}

impl Anthropic {
    fn event(&mut self, ev: Event) -> Vec<u8> {
        let mut out = if self.started || ev.kind == EventKind::Start {
            Vec::new()
        } else {
            self.begin(&Event::default())
        };
        match ev.kind {
            EventKind::Start => out.extend(self.begin(&ev)),
            EventKind::Text => {
                if ev.text.is_empty() {
                    return out;
                }
                out.extend(self.open_block(Kind::Text, json!({ "text": "" })));
                out.extend(self.delta(json!({ "type": "text_delta", "text": &ev.text })));
            }
            EventKind::Think => {
                if ev.text.is_empty() {
                    return out;
                }
                out.extend(self.open_block(Kind::Thinking, json!({ "thinking": "" })));
                out.extend(self.delta(json!({ "type": "thinking_delta", "thinking": &ev.text })));
            }
            EventKind::Sig if self.open == Some(Kind::Thinking) => {
                out.extend(self.delta(json!({ "type": "signature_delta", "signature": &ev.text })));
            }
            EventKind::ToolStart => {
                let id = if ev.id.is_empty() {
                    format!("toolu_{}", ir::new_id())
                } else {
                    ev.id.clone()
                };
                out.extend(self.open_block(
                    Kind::ToolCall,
                    json!({ "id": id, "name": &ev.name, "input": {} }),
                ));
            }
            EventKind::ToolArgs if self.open == Some(Kind::ToolCall) && !ev.text.is_empty() => {
                self.args = true;
                out.extend(self.delta(
                    json!({ "type": "input_json_delta", "partial_json": &ev.text }),
                ));
            }
            EventKind::Error => {
                out.extend(self.close());
                out.extend(frame(
                    "error",
                    json!({
                        "type": "error",
                        "error": { "type": "api_error", "message": &ev.text },
                    }),
                ));
            }
            _ => {}
        }
        self.col.add(ev);
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.started {
            out.extend(self.begin(&Event::default()));
        }
        out.extend(self.close());
        let reply = self.col.finish();
        out.extend(frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": ir::stop_to_anthropic(&reply.stop), "stop_sequence": null },
                "usage": reply.usage.anthropic(),
            }),
        ));
        out.extend(frame("message_stop", json!({ "type": "message_stop" })));
        out
    }

    fn begin(&mut self, ev: &Event) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let id = if ev.msg_id.is_empty() {
            format!("msg_{}", ir::new_id())
        } else {
            ev.msg_id.clone()
        };
        if !ev.model.is_empty() {
            self.model.clone_from(&ev.model);
        }
        frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": ev.usage.anthropic(),
                }
            }),
        )
    }

    // open_block starts a block of that kind, unless the words of one are
    // still arriving.
    fn open_block(&mut self, kind: Kind, mut block: Value) -> Vec<u8> {
        if self.open == Some(kind) && kind != Kind::ToolCall {
            return Vec::new();
        }
        let mut out = self.close();
        self.args = false;
        if let Some(object) = block.as_object_mut() {
            object.insert(
                "type".to_owned(),
                json!(match kind {
                    Kind::Text => "text",
                    Kind::Thinking => "thinking",
                    _ => "tool_use",
                }),
            );
        }
        out.extend(frame(
            "content_block_start",
            json!({ "type": "content_block_start", "index": self.index, "content_block": block }),
        ));
        self.open = Some(kind);
        out
    }

    // close ends the open block, giving a tool block its empty arguments in
    // words when none ever came.
    fn close(&mut self) -> Vec<u8> {
        let Some(open) = self.open else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if open == Kind::ToolCall && !self.args {
            out.extend(self.delta(json!({ "type": "input_json_delta", "partial_json": "{}" })));
        }
        out.extend(frame(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": self.index }),
        ));
        self.index += 1;
        self.open = None;
        out
    }

    fn delta(&mut self, delta: Value) -> Vec<u8> {
        frame(
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": self.index, "delta": delta }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::ApiProtocol;
    use super::*;
    use crate::gateway::ir::Usage;
    use serde_json::json;

    // events_of reads back the frames, skipping what is no JSON at all.
    fn events_of(bytes: &[u8]) -> Vec<(String, Value)> {
        let text = String::from_utf8_lossy(bytes);
        let mut out = Vec::new();
        let mut name = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                name = rest.to_owned();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if let Ok(data) = serde_json::from_str(rest) {
                    out.push((name.clone(), data));
                }
                name = String::new();
            }
        }
        out
    }

    // one is the frame that says what, which is where a stream's meaning is.
    fn one(got: &[(String, Value)], what: &str) -> Value {
        got.iter()
            .find(|(_, data)| data["type"] == what)
            .unwrap_or_else(|| panic!("no {what} in {:?}", got.iter().map(|f| &f.0).collect::<Vec<_>>()))
            .1
            .clone()
    }

    fn told(kind: EventKind, text: &str) -> Event {
        Event {
            kind,
            text: text.to_owned(),
            ..Event::default()
        }
    }

    #[test]
    fn a_chat_stream_opens_with_the_role_and_closes_done() {
        let mut e = Encoder::new(ApiProtocol::Chat, "gpt-5");
        let mut bytes = e.event(Event {
            kind: EventKind::Start,
            msg_id: "abc".to_owned(),
            ..Event::default()
        });
        bytes.extend(e.event(told(EventKind::Text, "hi")));
        bytes.extend(e.finish());
        let got = events_of(&bytes);
        assert_eq!(got[0].1["id"], json!("chatcmpl-abc"));
        assert_eq!(got[0].1["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(got[1].1["choices"][0]["delta"]["content"], json!("hi"));
        assert_eq!(got[2].1["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(got[2].1["choices"][0]["delta"], json!({}));
        assert!(got[3].1["usage"].is_object());
        assert!(got[3].1["choices"].as_array().unwrap().is_empty());
        let tail = String::from_utf8_lossy(&bytes[bytes.len() - 14..]);
        assert!(tail.contains("data: [DONE]"), "{tail}");
    }

    #[test]
    fn a_chat_call_keeps_its_place_in_the_stream() {
        let mut e = Encoder::new(ApiProtocol::Chat, "m");
        let mut bytes = e.event(Event {
            kind: EventKind::ToolStart,
            id: "c1".to_owned(),
            name: "read".to_owned(),
            ..Event::default()
        });
        bytes.extend(e.event(told(EventKind::ToolArgs, "{\"a\":")));
        bytes.extend(e.event(told(EventKind::ToolArgs, "1}")));
        bytes.extend(e.finish());
        let got = events_of(&bytes);
        // the reply was announced before anything could be said in it
        assert_eq!(got[0].1["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(
            got[1].1["choices"][0]["delta"]["tool_calls"][0]["id"],
            json!("c1")
        );
        assert_eq!(
            got[1].1["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(0)
        );
        assert_eq!(
            got[2].1["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":")
        );
        assert_eq!(
            got[4].1["choices"][0]["finish_reason"],
            json!("tool_calls"),
            "a call is why it stopped: {:?}",
            got[4]
        );
    }

    #[test]
    fn a_responses_stream_numbers_what_it_says() {
        let mut e = Encoder::new(ApiProtocol::Responses, "gpt-5");
        let mut bytes = e.event(Event {
            kind: EventKind::Start,
            msg_id: "resp_1".to_owned(),
            ..Event::default()
        });
        bytes.extend(e.event(told(EventKind::Text, "hello")));
        bytes.extend(e.finish());
        let got = events_of(&bytes);
        assert_eq!(got[0].0, "response.created");
        assert_eq!(got[1].0, "response.in_progress");
        assert_eq!(got[2].0, "response.output_item.added");
        assert_eq!(got[2].1["item"]["type"], json!("message"));
        assert_eq!(got[3].0, "response.content_part.added");
        assert_eq!(got[4].1["delta"], json!("hello"));
        let last = &got.last().unwrap().1;
        assert_eq!(last["id"], json!("resp_1"));
        assert_eq!(last["status"], json!("completed"));
        assert_eq!(last["output"][0]["content"][0]["text"], json!("hello"));
        let numbers: Vec<i64> = got
            .iter()
            .map(|(_, data)| data["sequence_number"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, (0..numbers.len() as i64).collect::<Vec<i64>>());
    }

    #[test]
    fn a_responses_call_is_closed_with_the_arguments_it_asked_for() {
        let mut e = Encoder::new(ApiProtocol::Responses, "m");
        let mut bytes = e.event(told(EventKind::Think, "why not"));
        bytes.extend(e.event(Event {
            kind: EventKind::ToolStart,
            name: "run".to_owned(),
            ..Event::default()
        }));
        bytes.extend(e.event(told(EventKind::ToolArgs, "{\"a\":")));
        bytes.extend(e.event(told(EventKind::ToolArgs, "1}")));
        bytes.extend(e.finish());
        let got = events_of(&bytes);
        assert_eq!(
            one(&got, "response.reasoning_summary_text.done")["text"],
            json!("why not")
        );
        assert_eq!(
            one(&got, "response.function_call_arguments.done")["arguments"],
            json!(r#"{"a":1}"#)
        );
        let items: Vec<Value> = got
            .iter()
            .filter(|(_, data)| data["type"] == "response.output_item.done")
            .map(|(_, data)| data["item"].clone())
            .collect();
        assert_eq!(items.len(), 2, "thought then called");
        assert_eq!(items[0]["type"], json!("reasoning"));
        assert!(
            items[1]["call_id"]
                .as_str()
                .unwrap()
                .starts_with("call_")
        );
        assert_eq!(items[1]["arguments"], json!(r#"{"a":1}"#));
        assert_eq!(one(&got, "response.completed")["output"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_messages_stream_wraps_every_block() {
        let mut e = Encoder::new(ApiProtocol::Anthropic, "claude");
        let mut bytes = e.event(told(EventKind::Text, "look"));
        bytes.extend(e.event(told(EventKind::Text, " again")));
        bytes.extend(e.event(Event {
            kind: EventKind::ToolStart,
            id: "t1".to_owned(),
            name: "read".to_owned(),
            ..Event::default()
        }));
        bytes.extend(e.finish());
        let got = events_of(&bytes);
        assert_eq!(got[0].0, "message_start");
        assert_eq!(got[0].1["message"]["model"], json!("claude"));
        assert_eq!(got[1].0, "content_block_start");
        assert_eq!(got[1].1["content_block"]["type"], json!("text"));
        assert_eq!(got[2].1["delta"]["text"], json!("look"));
        assert_eq!(got[3].1["delta"]["text"], json!(" again"));
        assert_eq!(got[4].0, "content_block_stop");
        assert_eq!(got[5].1["content_block"]["id"], json!("t1"));
        assert_eq!(got[6].1["delta"]["partial_json"], json!("{}"));
        assert_eq!(got[8].1["delta"]["stop_reason"], json!("tool_use"));
        assert_eq!(got[9].0, "message_stop");
    }

    #[test]
    fn an_error_ends_the_stream_it_was_in() {
        let mut e = Encoder::new(ApiProtocol::Anthropic, "claude");
        let mut bytes = e.event(told(EventKind::Text, "half"));
        bytes.extend(e.event(told(EventKind::Error, "out of quota")));
        let got = events_of(&bytes);
        assert_eq!(got.last().unwrap().0, "error");
        assert_eq!(got.last().unwrap().1["error"]["message"], json!("out of quota"));
    }

    #[test]
    fn a_reply_with_nothing_said_is_still_a_reply() {
        let mut e = Encoder::new(ApiProtocol::Responses, "m");
        let bytes = e.finish();
        let got = events_of(&bytes);
        assert_eq!(got[0].0, "response.created");
        assert_eq!(got.last().unwrap().1["status"], json!("completed"));
        assert_eq!(got.last().unwrap().1["output"], json!([]));
        // words that are no words at all add nothing to the reply
        let mut e = Encoder::new(ApiProtocol::Responses, "m");
        let mut bytes = e.event(told(EventKind::Text, ""));
        bytes.extend(e.finish());
        let got = events_of(&bytes);
        assert_eq!(got.last().unwrap().1["output"], json!([]));
    }

    #[test]
    fn usage_comes_from_whatever_said_it() {
        let usage = Usage {
            input: 3,
            output: 4,
            ..Usage::default()
        };
        assert_eq!(usage.anthropic()["input_tokens"], json!(3));
        assert_eq!(usage.chat()["total_tokens"], json!(7));
        assert_eq!(usage.responses()["output_tokens"], json!(4));
    }
}
