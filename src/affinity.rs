use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use axum::http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::gateway::ApiProtocol;

const CACHE_WORTH: usize = 1024;
const CACHE_COLD: Duration = Duration::from_secs(5 * 60);
const STICK_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_STICKS: usize = 4096;
const MAX_JSON_BYTES: usize = 8 << 20;
const MAX_SSE_LINE_BYTES: usize = 1 << 20;

#[derive(Clone, Debug)]
pub(crate) struct Context {
    key: String,
    pub(crate) mode: String,
    pub(crate) within: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Previous {
    pub(crate) route: String,
    pub(crate) at: Instant,
    pub(crate) cache_read: usize,
}

#[derive(Clone, Debug)]
struct Stick {
    route: String,
    at: Instant,
    cache_read: usize,
}

static STICKS: LazyLock<Mutex<HashMap<String, Stick>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn context(
    scope: &str,
    configured_mode: &str,
    headers: &HeaderMap,
    protocol: ApiProtocol,
    body: &Value,
) -> Context {
    let conversation = conversation_id(headers, body);
    let (_, within) = turn_of(protocol, body);
    Context {
        key: format!("{scope}|{conversation}"),
        mode: if configured_mode.is_empty() {
            "auto".to_owned()
        } else {
            configured_mode.to_owned()
        },
        within,
    }
}

pub(crate) fn previous(context: &Context) -> Option<Previous> {
    let now = Instant::now();
    let mut sticks = STICKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let expired = sticks
        .get(&context.key)
        .is_some_and(|stick| now.saturating_duration_since(stick.at) > STICK_KEEP);
    if expired {
        sticks.remove(&context.key);
        return None;
    }
    sticks.get(&context.key).map(|stick| Previous {
        route: stick.route.clone(),
        at: stick.at,
        cache_read: stick.cache_read,
    })
}

pub(crate) fn should_keep(context: &Context, previous: &Previous, rotate: bool) -> bool {
    if context.mode == "off" {
        return false;
    }
    if context.mode == "session" || context.within {
        return true;
    }
    if context.mode == "turn" || rotate || previous.cache_read < CACHE_WORTH {
        return false;
    }
    Instant::now().saturating_duration_since(previous.at) <= CACHE_COLD
}

pub(crate) fn should_advance(context: &Context, rotate: bool) -> bool {
    context.mode != "off" && !context.within && rotate
}

pub(crate) fn record(context: &Context, route: String, cache_read: usize) {
    let now = Instant::now();
    let mut sticks = STICKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sticks.insert(
        context.key.clone(),
        Stick {
            route,
            at: now,
            cache_read,
        },
    );
    if sticks.len() > MAX_STICKS {
        sticks.retain(|_, stick| now.saturating_duration_since(stick.at) <= STICK_KEEP);
    }
}

fn conversation_id(headers: &HeaderMap, body: &Value) -> String {
    for name in [
        "x-opencode-session",
        "x-session-affinity",
        "x-session-id",
        "session_id",
        "session-id",
        "x-claude-code-session-id",
    ] {
        if let Some(value) = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return value.to_owned();
        }
    }

    let messages = body.get("messages").and_then(Value::as_array);
    let input = body.get("input");
    let items = messages.or_else(|| input.and_then(Value::as_array));
    let first = items.and_then(|items| {
        items
            .iter()
            .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
            .or_else(|| items.first())
    });
    let identity = first.or(input).unwrap_or(body);
    let serialized = serde_json::to_vec(identity).unwrap_or_default();
    let digest = Sha256::digest(serialized);
    let mut id = String::from("magpie-");
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

fn turn_of(protocol: ApiProtocol, body: &Value) -> (usize, bool) {
    match protocol {
        ApiProtocol::Chat => chat_turns(body),
        ApiProtocol::Anthropic => anthropic_turns(body),
        ApiProtocol::Responses => responses_turns(body),
    }
}

fn chat_turns(body: &Value) -> (usize, bool) {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return (0, false);
    };
    let mut turn = 0;
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("user") {
            let (text_or_image, result) = chat_content(message.get("content"));
            if text_or_image && !result {
                turn += 1;
            }
        }
    }
    let within =
        messages.last().is_some_and(
            |message| match message.get("role").and_then(Value::as_str) {
                Some("tool") => true,
                Some("user") => chat_content(message.get("content")).1,
                _ => false,
            },
        );
    (turn, within)
}

fn anthropic_turns(body: &Value) -> (usize, bool) {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return (0, false);
    };
    let mut turn = 0;
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("user") {
            let (text_or_image, result) = anthropic_content(message.get("content"));
            if text_or_image && !result {
                turn += 1;
            }
        }
    }
    let within = messages.last().is_some_and(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
            && anthropic_content(message.get("content")).1
    });
    (turn, within)
}

fn responses_turns(body: &Value) -> (usize, bool) {
    let input = body.get("input");
    if let Some(text) = input.and_then(Value::as_str) {
        return (if text.is_empty() { 0 } else { 1 }, false);
    }
    let Some(items) = input.and_then(Value::as_array) else {
        return (0, false);
    };
    let mut turn = 0;
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
            continue;
        }
        let message = item.get("type").and_then(Value::as_str) == Some("message")
            || (item.get("type").is_none() && item.get("role").is_some());
        if message && item.get("role").and_then(Value::as_str) != Some("assistant") {
            let (text_or_image, result) = responses_content(item.get("content"));
            if text_or_image && !result {
                turn += 1;
            }
        }
    }
    let within = items.last().is_some_and(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            || (item.get("role").and_then(Value::as_str) == Some("user")
                && responses_content(item.get("content")).1)
    });
    (turn, within)
}

fn chat_content(content: Option<&Value>) -> (bool, bool) {
    let Some(content) = content else {
        return (false, false);
    };
    if let Some(text) = content.as_str() {
        return (!text.is_empty(), false);
    }
    content.as_array().map_or((false, false), |blocks| {
        let text_or_image = blocks.iter().any(|block| {
            matches!(block.get("type").and_then(Value::as_str), Some("image_url"))
                || (block.get("type").and_then(Value::as_str) == Some("text")
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty()))
        });
        let result = blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
        (text_or_image, result)
    })
}

fn anthropic_content(content: Option<&Value>) -> (bool, bool) {
    let Some(content) = content else {
        return (false, false);
    };
    if let Some(text) = content.as_str() {
        return (!text.is_empty(), false);
    }
    content.as_array().map_or((false, false), |blocks| {
        let text_or_image =
            blocks
                .iter()
                .any(|block| match block.get("type").and_then(Value::as_str) {
                    Some("image") => true,
                    Some("text") => block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty()),
                    _ => false,
                });
        let result = blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
        (text_or_image, result)
    })
}

fn responses_content(content: Option<&Value>) -> (bool, bool) {
    let Some(content) = content else {
        return (false, false);
    };
    if let Some(text) = content.as_str() {
        return (!text.is_empty(), false);
    }
    content.as_array().map_or((false, false), |blocks| {
        let text_or_image =
            blocks
                .iter()
                .any(|block| match block.get("type").and_then(Value::as_str) {
                    Some("input_image" | "image") => true,
                    Some("input_text" | "text") => block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty()),
                    _ => false,
                });
        let result = blocks.iter().any(|block| {
            matches!(
                block.get("type").and_then(Value::as_str),
                Some("function_call_output" | "tool_result")
            )
        });
        (text_or_image, result)
    })
}

pub(crate) struct UsageScanner {
    protocol: ApiProtocol,
    sse: bool,
    body: Vec<u8>,
    line: Vec<u8>,
    event_data: Vec<u8>,
    cache_read: usize,
    overflowed: bool,
}

impl UsageScanner {
    pub(crate) fn new(protocol: ApiProtocol, content_type: Option<&str>) -> Self {
        Self {
            protocol,
            sse: content_type.is_some_and(|value| value.starts_with("text/event-stream")),
            body: Vec::new(),
            line: Vec::new(),
            event_data: Vec::new(),
            cache_read: 0,
            overflowed: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if !self.sse {
            if self.body.len().saturating_add(bytes.len()) > MAX_JSON_BYTES {
                self.overflowed = true;
                self.body.clear();
                return;
            }
            if !self.overflowed {
                self.body.extend_from_slice(bytes);
            }
            return;
        }

        for byte in bytes {
            if *byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                self.process_line();
                self.line.clear();
            } else if self.line.len() < MAX_SSE_LINE_BYTES {
                self.line.push(*byte);
            } else {
                self.line.clear();
                self.event_data.clear();
            }
        }
    }

    pub(crate) fn finish(&mut self) -> usize {
        if self.sse {
            if !self.line.is_empty() {
                self.process_line();
                self.line.clear();
            }
            self.process_event();
        } else if !self.overflowed {
            let body = std::mem::take(&mut self.body);
            self.parse(&body);
        }
        self.cache_read
    }

    fn process_line(&mut self) {
        if self.line.is_empty() {
            self.process_event();
            return;
        }
        if let Some(data) = self.line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if !self.event_data.is_empty() {
                self.event_data.push(b'\n');
            }
            if self.event_data.len().saturating_add(data.len()) <= MAX_SSE_LINE_BYTES {
                self.event_data.extend_from_slice(data);
            } else {
                self.event_data.clear();
            }
        }
    }

    fn process_event(&mut self) {
        let data = std::mem::take(&mut self.event_data);
        if data.first() == Some(&b'{') {
            self.parse(&data);
        }
    }

    fn parse(&mut self, bytes: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };
        let usage = match self.protocol {
            ApiProtocol::Chat => value.get("usage").into_iter().collect::<Vec<_>>(),
            ApiProtocol::Responses => value
                .get("response")
                .and_then(|response| response.get("usage"))
                .or_else(|| value.get("usage"))
                .into_iter()
                .collect(),
            ApiProtocol::Anthropic => [
                value
                    .get("message")
                    .and_then(|message| message.get("usage")),
                value.get("usage"),
            ]
            .into_iter()
            .flatten()
            .collect(),
        };
        for usage in usage {
            self.cache_read = self.cache_read.max(cache_read_tokens(usage));
        }
    }
}

pub(crate) fn cache_read_from_value(protocol: ApiProtocol, value: &Value) -> usize {
    let mut scanner = UsageScanner::new(protocol, None);
    scanner.parse(&serde_json::to_vec(value).unwrap_or_default());
    scanner.cache_read
}

fn cache_read_tokens(usage: &Value) -> usize {
    let direct = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .and_then(|tokens| usize::try_from(tokens).ok())
        .unwrap_or_default();
    let chat = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .and_then(|tokens| usize::try_from(tokens).ok())
        .unwrap_or_default();
    let responses = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .and_then(|tokens| usize::try_from(tokens).ok())
        .unwrap_or_default();
    direct.max(chat).max(responses)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(value: &str) -> Value {
        serde_json::from_str(value).expect("test JSON should parse")
    }

    #[test]
    fn affinity_prefers_explicit_session_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "  session-42  ".parse().unwrap());
        let context = context(
            "provider/model",
            "",
            &headers,
            ApiProtocol::Chat,
            &body(r#"{"messages":[{"role":"user","content":"hello"}]}"#),
        );
        assert_eq!(context.key, "provider/model|session-42");
        assert_eq!(context.mode, "auto");
    }

    #[test]
    fn turn_affinity_keeps_tool_rounds_and_rotates_on_new_user_turn() {
        let tool_round = body(
            r#"{"messages":[{"role":"user","content":"question"},{"role":"assistant","content":null},{"role":"tool","content":"answer"}]}"#,
        );
        let user_turn = body(
            r#"{"messages":[{"role":"user","content":"question"},{"role":"assistant","content":"answer"},{"role":"user","content":"next"}]}"#,
        );
        let tool_context = context(
            "test-turn",
            "turn",
            &HeaderMap::new(),
            ApiProtocol::Chat,
            &tool_round,
        );
        let user_context = context(
            "test-turn-user",
            "turn",
            &HeaderMap::new(),
            ApiProtocol::Chat,
            &user_turn,
        );
        assert!(tool_context.within);
        assert_eq!(turn_of(ApiProtocol::Chat, &tool_round).0, 1);
        assert!(should_keep(
            &tool_context,
            &Previous {
                route: "route".to_owned(),
                at: Instant::now(),
                cache_read: 0,
            },
            false,
        ));
        assert!(!user_context.within);
        assert_eq!(turn_of(ApiProtocol::Chat, &user_turn).0, 2);
        assert!(!should_keep(
            &user_context,
            &Previous {
                route: "route".to_owned(),
                at: Instant::now(),
                cache_read: CACHE_WORTH,
            },
            false,
        ));
        assert!(!should_advance(&user_context, false));
    }

    #[test]
    fn default_affinity_requires_a_warm_cache_across_turns() {
        let request = body(r#"{"messages":[{"role":"user","content":"next"}]}"#);
        let context = context(
            "test-auto",
            "",
            &HeaderMap::new(),
            ApiProtocol::Chat,
            &request,
        );
        let previous = Previous {
            route: "route".to_owned(),
            at: Instant::now(),
            cache_read: 2048,
        };
        assert!(should_keep(&context, &previous, false));
        assert!(!should_keep(
            &context,
            &Previous {
                cache_read: 1023,
                ..previous.clone()
            },
            false
        ));
        assert!(!should_keep(&context, &previous, true));
    }

    #[test]
    fn usage_scanner_reads_chat_json_and_anthropic_sse_across_chunks() {
        let mut chat = UsageScanner::new(ApiProtocol::Chat, Some("application/json"));
        chat.push(br#"{"usage":{"prompt_tokens_details":{"cached_tokens":3072}}}"#);
        assert_eq!(chat.finish(), 3072);

        let mut anthropic = UsageScanner::new(ApiProtocol::Anthropic, Some("text/event-stream"));
        anthropic.push(b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"cache_read_input_tokens\":");
        anthropic.push(b"2048}}}\r\n\r\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n");
        assert_eq!(anthropic.finish(), 2048);
    }

    #[test]
    fn responses_usage_can_be_nested_in_completed_event() {
        let usage = cache_read_from_value(
            ApiProtocol::Responses,
            &body(
                r#"{"type":"response.completed","response":{"usage":{"input_tokens_details":{"cached_tokens":1536}}}}"#,
            ),
        );
        assert_eq!(usage, 1536);
    }
}
