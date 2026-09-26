// The shape the subscription backends speak in. The wire APIs are close
// cousins; a request is parsed into this from whichever one it arrived in,
// and the model's reply is produced from it again for the API the agent
// asked for.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// Kind is what a part of a message holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Text,
    Image,
    ToolCall,
    ToolResult,
    Thinking,
}

// Part is one block of a message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Part {
    pub kind: Kind,
    // text, thinking, or a tool result's output
    pub text: String,
    // image
    pub media_type: String,
    // base64
    pub data: String,
    pub url: String,
    // tool call
    pub id: String,
    pub name: String,
    // a JSON object; none until the arguments have all arrived
    pub args: Option<Value>,
    // tool result
    pub call_id: String,
    pub is_error: bool,
    // thinking
    pub signature: String,
}

// Message is one turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message {
    // user | assistant
    pub role: String,
    pub parts: Vec<Part>,
}

// Tool is a function the model may call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    // JSON schema of the arguments
    pub schema: Value,
}

// Request is a call to a model, whichever API it arrived in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    // "" | auto | none | required | name:<tool>
    pub tool_choice: String,
    pub max_tokens: i64,
    pub temp: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub stream: bool,
    // low | medium | high | xhigh | max, when the client asked
    pub effort: String,
    // the client asked for visible reasoning
    pub thinking: bool,
    // parallel tool calls allowed
    pub parallel: Option<bool>,
}

// EventKind is what a streamed event carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EventKind {
    // the reply's own, and what the call came to so far
    #[default]
    Start,
    Text,
    // reasoning
    Think,
    // a thinking block's signature
    Sig,
    ToolStart,
    // partial JSON of the arguments
    ToolArgs,
    Stop,
    Usage,
    Error,
}

// Event is one thing a streaming reply said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub text: String,
    pub id: String,
    pub name: String,
    pub msg_id: String,
    pub model: String,
    // stop | length | tool | filter
    pub stop: String,
    pub usage: Usage,
}

// Usage counts tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub output: i64,
    #[serde(default)]
    pub cache_read: i64,
    #[serde(default)]
    pub cache_write: i64,
    #[serde(default)]
    pub reasoning: i64,
}

impl Usage {
    // prompt is every token the prompt came to, as OpenAI's and Gemini's
    // counts have it: Anthropic's leaves out what was read from its cache
    // and what was written to it.
    pub fn prompt(&self) -> i64 {
        self.input + self.cache_read + self.cache_write
    }

    // add takes what a reply counted over what was known: an API that
    // reports no number for something has not changed it.
    pub fn add(&mut self, came: &Usage) {
        for (known, incoming) in [
            (&mut self.input, came.input),
            (&mut self.output, came.output),
            (&mut self.cache_read, came.cache_read),
            (&mut self.cache_write, came.cache_write),
            (&mut self.reasoning, came.reasoning),
        ] {
            if incoming > 0 {
                *known = incoming;
            }
        }
    }
}

// Reply is a whole reply, for non-streaming clients.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub id: String,
    pub model: String,
    pub parts: Vec<Part>,
    pub stop: String,
    pub usage: Usage,
}

// Collector assembles a Reply from events. Encoders use the same logic to
// know what the reply contained so far.
#[derive(Default)]
pub struct Collector {
    reply: Reply,
    // arguments of the open tool call
    args: String,
    error: String,
}

impl Collector {
    // add works one event through the reply.
    pub fn add(&mut self, event: Event) {
        match event.kind {
            EventKind::Start => {
                self.reply.id = event.msg_id;
                self.reply.model = event.model;
                self.reply.usage.add(&event.usage);
            }
            EventKind::Text => self.push_text(Kind::Text, event.text),
            EventKind::Think => self.push_text(Kind::Thinking, event.text),
            // a signature belongs to the reasoning it signs, which an API
            // that sends them apart only ever sends after some
            EventKind::Sig => {
                if let Some(last) = self
                    .reply
                    .parts
                    .last_mut()
                    .filter(|last| last.kind == Kind::Thinking)
                {
                    last.signature.push_str(&event.text);
                }
            }
            EventKind::ToolStart => {
                self.close_tool();
                self.reply.parts.push(Part {
                    kind: Kind::ToolCall,
                    id: event.id,
                    name: event.name,
                    ..Part::default()
                });
            }
            EventKind::ToolArgs => self.args.push_str(&event.text),
            EventKind::Stop => {
                self.close_tool();
                self.reply.stop = event.stop;
            }
            EventKind::Usage => self.reply.usage.add(&event.usage),
            EventKind::Error => self.error = event.text,
        }
    }

    // finish closes what is open and gives the reply as a whole.
    pub fn finish(&mut self) -> Reply {
        self.close_tool();
        if self.reply.stop.is_empty() {
            self.reply.stop = if has_tool(&self.reply.parts) {
                "tool"
            } else {
                "stop"
            }
            .to_owned();
        }
        std::mem::take(&mut self.reply)
    }

    // error is what the reply said went wrong, if it did.
    pub fn error(&self) -> &str {
        &self.error
    }

    // push_text adds to the part the reply is building when it is of that
    // kind, and opens one of it when the reply is building something else.
    fn push_text(&mut self, kind: Kind, text: String) {
        if let Some(last) = self.reply.parts.last_mut().filter(|last| last.kind == kind) {
            last.text.push_str(&text);
            return;
        }
        self.close_tool();
        self.reply.parts.push(Part {
            kind,
            text,
            ..Part::default()
        });
    }

    // close_tool gives the open tool call the arguments gathered for it.
    fn close_tool(&mut self) {
        let open = self
            .reply
            .parts
            .last()
            .is_some_and(|last| last.kind == Kind::ToolCall && last.args.is_none());
        if !open {
            return;
        }
        let args = std::mem::take(&mut self.args);
        if let Some(last) = self.reply.parts.last_mut() {
            last.args = Some(args_object(&args));
        }
    }
}

// has_tool reports whether a reply calls any tool.
pub fn has_tool(parts: &[Part]) -> bool {
    parts.iter().any(|part| part.kind == Kind::ToolCall)
}

// args_of is a tool call's arguments as a JSON object, never none.
pub fn args_of(part: &Part) -> Value {
    part.args.clone().unwrap_or_else(|| json!({}))
}

// args_string is the same as text, for the APIs that want one.
pub fn args_string(part: &Part) -> String {
    args_of(part).to_string()
}

// args_object reads text of tool arguments as a JSON object; text that is
// no object is wrapped so nothing is lost.
pub fn args_object(text: &str) -> Value {
    let text = text.trim();
    if text.is_empty() {
        return json!({});
    }
    match serde_json::from_str(text) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ "input": text }),
    }
}

// text_of joins the text parts of a message.
pub fn text_of(parts: &[Part]) -> String {
    parts
        .iter()
        .filter(|part| part.kind == Kind::Text)
        .map(|part| part.text.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, text: &str) -> Event {
        Event {
            kind,
            text: text.to_owned(),
            ..Event::default()
        }
    }

    #[test]
    fn text_grows_till_something_else_opens() {
        let mut c = Collector::default();
        c.add(event(EventKind::Text, "hello "));
        c.add(event(EventKind::Text, "there"));
        c.add(event(EventKind::Think, "hmm"));
        c.add(event(EventKind::Sig, "sig"));
        c.add(event(EventKind::Text, " again"));
        let reply = c.finish();
        assert_eq!(reply.parts.len(), 3);
        assert_eq!(reply.parts[0].text, "hello there");
        assert_eq!(reply.parts[1].text, "hmm");
        assert_eq!(reply.parts[1].signature, "sig");
        assert_eq!(reply.parts[2].text, " again");
        assert_eq!(reply.stop, "stop");
    }

    #[test]
    fn a_signature_needs_the_reasoning_it_signs() {
        let mut c = Collector::default();
        c.add(event(EventKind::Sig, "sig"));
        assert!(c.finish().parts.is_empty());
    }

    #[test]
    fn tool_arguments_close_the_call_they_were_gathered_for() {
        let mut c = Collector::default();
        c.add(event(EventKind::Text, "reading"));
        c.add(Event {
            kind: EventKind::ToolStart,
            id: "call-1".to_owned(),
            name: "read".to_owned(),
            ..Event::default()
        });
        c.add(event(EventKind::ToolArgs, "{\"path\":"));
        c.add(event(EventKind::ToolArgs, "\"/tmp/x\"}"));
        let reply = c.finish();
        assert_eq!(reply.stop, "tool", "a call is why it stopped");
        assert_eq!(reply.parts[1].args, Some(json!({"path": "/tmp/x"})));
    }

    #[test]
    fn tool_arguments_that_are_no_object_are_kept_whole() {
        let mut c = Collector::default();
        c.add(Event {
            kind: EventKind::ToolStart,
            name: "ask".to_owned(),
            ..Event::default()
        });
        c.add(event(EventKind::ToolArgs, " say hi "));
        let reply = c.finish();
        assert_eq!(reply.parts[0].args, Some(json!({"input": "say hi"})));
        assert_eq!(args_string(&reply.parts[0]), r#"{"input":"say hi"}"#);
    }

    #[test]
    fn a_call_without_arguments_is_an_empty_object() {
        let mut c = Collector::default();
        c.add(Event {
            kind: EventKind::ToolStart,
            name: "ping".to_owned(),
            ..Event::default()
        });
        assert_eq!(args_of(&c.finish().parts[0]), json!({}));
    }

    #[test]
    fn usage_keeps_the_last_number_said() {
        let mut c = Collector::default();
        c.add(Event {
            kind: EventKind::Start,
            msg_id: "resp-1".to_owned(),
            model: "m".to_owned(),
            usage: Usage {
                input: 10,
                ..Usage::default()
            },
            ..Event::default()
        });
        c.add(Event {
            kind: EventKind::Usage,
            usage: Usage {
                output: 4,
                cache_read: 6,
                ..Usage::default()
            },
            ..Event::default()
        });
        c.add(Event {
            kind: EventKind::Usage,
            usage: Usage {
                output: 0,
                ..Usage::default()
            },
            ..Event::default()
        });
        let reply = c.finish();
        assert_eq!(reply.id, "resp-1");
        assert_eq!(reply.usage.output, 4, "no number is not a zero");
        assert_eq!(reply.usage.prompt(), 16);
    }

    #[test]
    fn only_the_reply_that_ended_says_what_went_wrong() {
        let mut c = Collector::default();
        c.add(event(EventKind::Error, "rate limited"));
        assert_eq!(c.error(), "rate limited");
        assert!(c.finish().parts.is_empty());
    }

    #[test]
    fn text_of_is_the_message_said_in_words() {
        let parts = [
            Part {
                kind: Kind::Text,
                text: "a".to_owned(),
                ..Part::default()
            },
            Part {
                kind: Kind::Thinking,
                text: "think".to_owned(),
                ..Part::default()
            },
            Part {
                kind: Kind::Text,
                text: "b".to_owned(),
                ..Part::default()
            },
        ];
        assert_eq!(text_of(&parts), "ab");
        assert!(!has_tool(&parts), "thinking is no call");
    }
}
