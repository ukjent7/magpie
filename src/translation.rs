use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::gateway::ApiProtocol;

pub(crate) fn request(
    body: &Value,
    from: ApiProtocol,
    to: ApiProtocol,
    model: &str,
) -> Result<Value> {
    match (from, to) {
        (ApiProtocol::Chat, ApiProtocol::Anthropic) => chat_to_anthropic(body, model),
        (ApiProtocol::Anthropic, ApiProtocol::Chat) => anthropic_to_chat(body, model),
        _ => bail!("translation between these API protocols is not supported yet"),
    }
}

pub(crate) fn response(
    body: &Value,
    from: ApiProtocol,
    to: ApiProtocol,
    model: &str,
) -> Result<Value> {
    match (from, to) {
        (ApiProtocol::Anthropic, ApiProtocol::Chat) => anthropic_response_to_chat(body, model),
        (ApiProtocol::Chat, ApiProtocol::Anthropic) => chat_response_to_anthropic(body, model),
        _ => bail!("translation between these API protocols is not supported yet"),
    }
}

const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

pub(crate) struct SseTranslator {
    model: String,
    pending: Vec<u8>,
    direction: StreamDirection,
}

enum StreamDirection {
    AnthropicToChat(ChatStream),
    ChatToAnthropic(AnthropicStream),
}

#[derive(Default)]
struct ChatStream {
    id: String,
    started: bool,
    ended: bool,
    stop_reason: Option<String>,
    usage: Option<Value>,
    next_tool_index: usize,
    tool_indices: HashMap<usize, usize>,
}

#[derive(Default)]
struct AnthropicStream {
    id: String,
    started: bool,
    ended: bool,
    next_block_index: usize,
    text_block: Option<usize>,
    tools: BTreeMap<usize, ToolStream>,
    stop_reason: Option<String>,
    usage: Option<Value>,
}

#[derive(Default)]
struct ToolStream {
    id: String,
    name: String,
    block_index: Option<usize>,
}

impl SseTranslator {
    pub(crate) fn new(from: ApiProtocol, to: ApiProtocol, model: &str) -> Option<Self> {
        let direction = match (from, to) {
            (ApiProtocol::Anthropic, ApiProtocol::Chat) => {
                StreamDirection::AnthropicToChat(ChatStream::default())
            }
            (ApiProtocol::Chat, ApiProtocol::Anthropic) => {
                StreamDirection::ChatToAnthropic(AnthropicStream::default())
            }
            _ => return None,
        };
        Some(Self {
            model: model.to_owned(),
            pending: Vec::new(),
            direction,
        })
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(chunk);
        let mut output = Vec::new();
        while let Some((end, separator_len)) = event_boundary(&self.pending) {
            let event = self.pending[..end].to_vec();
            self.pending.drain(..end + separator_len);
            output.extend(self.process_event(&event));
        }
        if self.pending.len() > MAX_SSE_EVENT_BYTES {
            self.pending.clear();
            output.extend(self.failure("provider sent an oversized event"));
        }
        output
    }

    pub(crate) fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            let event = std::mem::take(&mut self.pending);
            output.extend(self.process_event(&event));
        }
        output.extend(match &mut self.direction {
            StreamDirection::AnthropicToChat(state) => finish_chat_stream(state, &self.model),
            StreamDirection::ChatToAnthropic(state) => finish_anthropic_stream(state, &self.model),
        });
        output
    }

    pub(crate) fn is_ended(&self) -> bool {
        match &self.direction {
            StreamDirection::AnthropicToChat(state) => state.ended,
            StreamDirection::ChatToAnthropic(state) => state.ended,
        }
    }

    fn process_event(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        if match &self.direction {
            StreamDirection::AnthropicToChat(state) => state.ended,
            StreamDirection::ChatToAnthropic(state) => state.ended,
        } {
            return Vec::new();
        }
        let (event_name, data) = parse_sse_event(event);
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            return self.finish();
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return Vec::new();
        };
        let kind = event_name
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        if kind == "error" || value.get("type").and_then(Value::as_str) == Some("error") {
            return self.failure(
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider returned an error"),
            );
        }
        match &mut self.direction {
            StreamDirection::AnthropicToChat(state) => {
                anthropic_event_to_chat(state, &self.model, kind, &value)
            }
            StreamDirection::ChatToAnthropic(state) => {
                chat_event_to_anthropic(state, &self.model, &value)
            }
        }
    }

    fn failure(&mut self, message: &str) -> Vec<Vec<u8>> {
        let mut output = Vec::new();
        match &mut self.direction {
            StreamDirection::AnthropicToChat(state) => {
                state.ended = true;
                output.push(data_frame(&json!({
                    "error": {"message":message,"type":"api_error","code":null}
                })));
                output.push(b"data: [DONE]\n\n".to_vec());
            }
            StreamDirection::ChatToAnthropic(state) => {
                state.ended = true;
                output.push(event_frame(
                    "error",
                    &json!({"type":"error","error":{"type":"api_error","message":message}}),
                ));
                output.push(event_frame("message_stop", &json!({"type":"message_stop"})));
            }
        }
        output
    }
}

fn event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if crlf < lf => Some((crlf, 4)),
        (Some(lf), _) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

fn parse_sse_event(event: &[u8]) -> (Option<String>, String) {
    let text = String::from_utf8_lossy(event);
    let mut name = None;
    let mut data = Vec::new();
    for line in text.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(value) = line.strip_prefix("event:") {
            name = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    (name, data.join("\n"))
}

fn merge_usage(current: &mut Option<Value>, incoming: &Value) {
    if let Some(current) = current.as_mut().and_then(Value::as_object_mut)
        && let Some(incoming) = incoming.as_object()
    {
        current.extend(incoming.clone());
    } else {
        *current = Some(incoming.clone());
    }
}

fn anthropic_event_to_chat(
    state: &mut ChatStream,
    model: &str,
    kind: &str,
    value: &Value,
) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    match kind {
        "message_start" => {
            let message = value.get("message").unwrap_or(&Value::Null);
            state.id = message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("chatcmpl_magpie")
                .to_owned();
            emit_chat_role(state, model, &mut output);
            if let Some(usage) = message.get("usage") {
                merge_usage(&mut state.usage, usage);
            }
        }
        "content_block_start" => {
            let block_index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = value.get("content_block").unwrap_or(&Value::Null);
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                emit_chat_role(state, model, &mut output);
                let tool_index = if let Some(index) = state.tool_indices.get(&block_index) {
                    *index
                } else {
                    let index = state.next_tool_index;
                    state.next_tool_index += 1;
                    state.tool_indices.insert(block_index, index);
                    index
                };
                output.push(chat_delta_frame(
                    state,
                    model,
                    json!({"tool_calls":[{
                        "index":tool_index,
                        "id":block.get("id").cloned().unwrap_or(Value::Null),
                        "type":"function",
                        "function":{"name":block.get("name").cloned().unwrap_or(Value::Null),"arguments":""}
                    }]}),
                ));
            }
        }
        "content_block_delta" => {
            emit_chat_role(state, model, &mut output);
            let block_index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta = value.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    output.push(chat_delta_frame(
                        state,
                        model,
                        json!({"content":delta.get("text").cloned().unwrap_or(Value::Null)}),
                    ));
                }
                Some("input_json_delta") => {
                    if let Some(index) = state.tool_indices.get(&block_index) {
                        output.push(chat_delta_frame(
                            state,
                            model,
                            json!({"tool_calls":[{
                                "index":index,
                                "function":{"arguments":delta.get("partial_json").cloned().unwrap_or(Value::Null)}
                            }]}),
                        ));
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(stop) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                state.stop_reason = Some(stop.to_owned());
            }
            if let Some(usage) = value.get("usage") {
                merge_usage(&mut state.usage, usage);
            }
        }
        "message_stop" => output.extend(finish_chat_stream(state, model)),
        _ => {}
    }
    output
}

fn emit_chat_role(state: &mut ChatStream, model: &str, output: &mut Vec<Vec<u8>>) {
    if state.started {
        return;
    }
    if state.id.is_empty() {
        state.id = "chatcmpl_magpie".to_owned();
    }
    state.started = true;
    output.push(chat_delta_frame(state, model, json!({"role":"assistant"})));
}

fn chat_delta_frame(state: &ChatStream, model: &str, delta: Value) -> Vec<u8> {
    data_frame(&json!({
        "id":state.id,
        "object":"chat.completion.chunk",
        "created":0,
        "model":model,
        "choices":[{"index":0,"delta":delta,"finish_reason":null}]
    }))
}

fn finish_chat_stream(state: &mut ChatStream, model: &str) -> Vec<Vec<u8>> {
    if state.ended {
        return Vec::new();
    }
    let mut output = Vec::new();
    emit_chat_role(state, model, &mut output);
    let stop = match state.stop_reason.as_deref().unwrap_or("end_turn") {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    };
    output.push(data_frame(&json!({
        "id":state.id,
        "object":"chat.completion.chunk",
        "created":0,
        "model":model,
        "choices":[{"index":0,"delta":{},"finish_reason":stop}]
    })));
    if let Some(usage) = &state.usage {
        let input = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(
                usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
            .saturating_add(
                usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
        let output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        output.push(data_frame(&json!({
            "id":state.id,
            "object":"chat.completion.chunk",
            "created":0,
            "model":model,
            "choices":[],
            "usage":{"prompt_tokens":input,"completion_tokens":output_tokens,"total_tokens":input.saturating_add(output_tokens)}
        })));
    }
    output.push(b"data: [DONE]\n\n".to_vec());
    state.ended = true;
    output
}

fn chat_event_to_anthropic(
    state: &mut AnthropicStream,
    model: &str,
    value: &Value,
) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(id) = value.get("id").and_then(Value::as_str) {
        state.id = id.to_owned();
    }
    if let Some(usage) = value.get("usage") {
        state.usage = Some(usage.clone());
    }
    emit_anthropic_start(state, model, &mut output);

    if let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    {
        if let Some(content) = choice.pointer("/delta/content").and_then(Value::as_str) {
            if !content.is_empty() {
                let index = ensure_text_block(state, model, &mut output);
                output.push(event_frame(
                    "content_block_delta",
                    &json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":content}}),
                ));
            }
        }
        if let Some(calls) = choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
        {
            for (ordinal, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                    .unwrap_or(ordinal);
                let tool = state.tools.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    tool.id.push_str(id);
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    tool.name.push_str(name);
                }
                let arguments = call.pointer("/function/arguments").and_then(Value::as_str);
                if arguments.is_some() {
                    let block_index = ensure_tool_block(state, model, index, &mut output);
                    if let Some(arguments) = arguments.filter(|arguments| !arguments.is_empty()) {
                        output.push(event_frame(
                            "content_block_delta",
                            &json!({"type":"content_block_delta","index":block_index,"delta":{"type":"input_json_delta","partial_json":arguments}}),
                        ));
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            state.stop_reason = Some(chat_stop_reason(reason).to_owned());
        }
    }
    output
}

fn emit_anthropic_start(state: &mut AnthropicStream, model: &str, output: &mut Vec<Vec<u8>>) {
    if state.started {
        return;
    }
    if state.id.is_empty() {
        state.id = "msg_magpie".to_owned();
    }
    let input_tokens = state
        .usage
        .as_ref()
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    output.push(event_frame(
        "message_start",
        &json!({"type":"message_start","message":{"id":state.id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":input_tokens,"output_tokens":0}}}),
    ));
    state.started = true;
}

fn ensure_text_block(state: &mut AnthropicStream, model: &str, output: &mut Vec<Vec<u8>>) -> usize {
    emit_anthropic_start(state, model, output);
    if let Some(index) = state.text_block {
        return index;
    }
    let index = state.next_block_index;
    state.next_block_index += 1;
    state.text_block = Some(index);
    output.push(event_frame(
        "content_block_start",
        &json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
    ));
    index
}

fn ensure_tool_block(
    state: &mut AnthropicStream,
    model: &str,
    tool_index: usize,
    output: &mut Vec<Vec<u8>>,
) -> usize {
    emit_anthropic_start(state, model, output);
    if let Some(index) = state
        .tools
        .get(&tool_index)
        .and_then(|tool| tool.block_index)
    {
        return index;
    }
    let index = state.next_block_index;
    state.next_block_index += 1;
    let tool = state.tools.entry(tool_index).or_default();
    if tool.id.is_empty() {
        tool.id = format!("toolu_magpie_{tool_index}");
    }
    if tool.name.is_empty() {
        tool.name = "unknown_tool".to_owned();
    }
    tool.block_index = Some(index);
    output.push(event_frame(
        "content_block_start",
        &json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":tool.id,"name":tool.name,"input":{}}}),
    ));
    index
}

fn finish_anthropic_stream(state: &mut AnthropicStream, model: &str) -> Vec<Vec<u8>> {
    if state.ended {
        return Vec::new();
    }
    let mut output = Vec::new();
    emit_anthropic_start(state, model, &mut output);
    let tool_indices = state.tools.keys().copied().collect::<Vec<_>>();
    for tool_index in tool_indices {
        let block_index = ensure_tool_block(state, model, tool_index, &mut output);
        output.push(event_frame(
            "content_block_stop",
            &json!({"type":"content_block_stop","index":block_index}),
        ));
    }
    if let Some(index) = state.text_block {
        output.push(event_frame(
            "content_block_stop",
            &json!({"type":"content_block_stop","index":index}),
        ));
    }
    let usage = state.usage.as_ref();
    let output_tokens = usage
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    output.push(event_frame(
        "message_delta",
        &json!({"type":"message_delta","delta":{"stop_reason":state.stop_reason.as_deref().unwrap_or("end_turn"),"stop_sequence":null},"usage":{"output_tokens":output_tokens}}),
    ));
    output.push(event_frame("message_stop", &json!({"type":"message_stop"})));
    state.ended = true;
    output
}

fn chat_stop_reason(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    }
}

pub(crate) fn completed_stream(
    body: &Value,
    protocol: ApiProtocol,
    model: &str,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    match protocol {
        ApiProtocol::Chat => {
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("chatcmpl_magpie");
            let choice = body
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
                .unwrap_or(&Value::Null);
            let message = choice.get("message").unwrap_or(&Value::Null);
            output.push(data_frame(&json!({
                "id":id,
                "object":"chat.completion.chunk",
                "created":body.get("created").cloned().unwrap_or_else(|| json!(0)),
                "model":model,
                "choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]
            })));
            if let Some(content) = message.get("content").filter(|content| !content.is_null()) {
                output.push(data_frame(&json!({
                    "id":id,
                    "object":"chat.completion.chunk",
                    "created":body.get("created").cloned().unwrap_or_else(|| json!(0)),
                    "model":model,
                    "choices":[{"index":0,"delta":{"content":content},"finish_reason":null}]
                })));
            }
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                let fragments = calls
                    .iter()
                    .enumerate()
                    .map(|(index, call)| {
                        json!({
                            "index":index,
                            "id":call.get("id").cloned().unwrap_or(Value::Null),
                            "type":"function",
                            "function":{
                                "name":call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                                "arguments":call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}")
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                output.push(data_frame(&json!({
                    "id":id,
                    "object":"chat.completion.chunk",
                    "created":body.get("created").cloned().unwrap_or_else(|| json!(0)),
                    "model":model,
                    "choices":[{"index":0,"delta":{"tool_calls":fragments},"finish_reason":null}]
                })));
            }
            output.push(data_frame(&json!({
                "id":id,
                "object":"chat.completion.chunk",
                "created":body.get("created").cloned().unwrap_or_else(|| json!(0)),
                "model":model,
                "choices":[{"index":0,"delta":{},"finish_reason":choice.get("finish_reason").cloned().unwrap_or_else(|| json!("stop"))}]
            })));
            if let Some(usage) = body.get("usage") {
                output.push(data_frame(&json!({
                    "id":id,
                    "object":"chat.completion.chunk",
                    "created":body.get("created").cloned().unwrap_or_else(|| json!(0)),
                    "model":model,
                    "choices":[],
                    "usage":usage
                })));
            }
            output.push(b"data: [DONE]\n\n".to_vec());
        }
        ApiProtocol::Anthropic => {
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("msg_magpie");
            let usage = body.get("usage").unwrap_or(&Value::Null);
            output.push(event_frame(
                "message_start",
                &json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":usage.get("input_tokens").cloned().unwrap_or_else(|| json!(0)),"output_tokens":0}}}),
            ));
            for (index, block) in body
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let start = match block_type {
                    "text" => Some(
                        json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                    ),
                    "tool_use" => Some(
                        json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":block.get("id").cloned().unwrap_or(Value::Null),"name":block.get("name").cloned().unwrap_or(Value::Null),"input":{}}}),
                    ),
                    _ => None,
                };
                let Some(start) = start else { continue };
                output.push(event_frame("content_block_start", &start));
                let delta = match block_type {
                    "text" => Some(
                        json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block.get("text").cloned().unwrap_or_else(|| json!(""))}}),
                    ),
                    "tool_use" => Some(
                        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":serde_json::to_string(block.get("input").unwrap_or(&Value::Null))?}}),
                    ),
                    _ => None,
                };
                if let Some(delta) = delta {
                    output.push(event_frame("content_block_delta", &delta));
                }
                output.push(event_frame(
                    "content_block_stop",
                    &json!({"type":"content_block_stop","index":index}),
                ));
            }
            output.push(event_frame(
                "message_delta",
                &json!({"type":"message_delta","delta":{"stop_reason":body.get("stop_reason").cloned().unwrap_or_else(|| json!("end_turn")),"stop_sequence":null},"usage":{"output_tokens":usage.get("output_tokens").cloned().unwrap_or_else(|| json!(0))}}),
            ));
            output.push(event_frame("message_stop", &json!({"type":"message_stop"})));
        }
        ApiProtocol::Responses => bail!("Responses streaming is not supported by this translation"),
    }
    Ok(output.into_iter().flatten().collect())
}

fn data_frame(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn event_frame(event: &str, value: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {value}\n\n").into_bytes()
}

fn chat_to_anthropic(body: &Value, model: &str) -> Result<Value> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .context("chat request must include a messages array")?;
    let mut system = Vec::new();
    let mut translated = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" | "developer" => {
                let text = content_text(message.get("content"));
                if !text.is_empty() {
                    system.push(text);
                }
            }
            "tool" => translated.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": message.get("content").cloned().unwrap_or(Value::Null)
                }]
            })),
            "user" | "assistant" => {
                let mut content = chat_content_blocks(message.get("content"));
                if role == "assistant"
                    && let Some(calls) = message.get("tool_calls").and_then(Value::as_array)
                {
                    for call in calls {
                        let function = call.get("function").unwrap_or(&Value::Null);
                        content.push(json!({
                            "type": "tool_use",
                            "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                            "name": function.get("name").and_then(Value::as_str).unwrap_or(""),
                            "input": parse_arguments(function.get("arguments"))
                        }));
                    }
                }
                if content.is_empty() {
                    content.push(json!({"type":"text", "text":""}));
                }
                translated.push(json!({"role": role, "content": content}));
            }
            _ => {}
        }
    }

    let max_tokens = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(4096);
    let mut result = json!({
        "model": model,
        "messages": translated,
        "max_tokens": max_tokens,
        "stream": false
    });
    if !system.is_empty() {
        result["system"] = json!(system.join("\n\n"));
    }
    for (source, target) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(value) = body.get(source).filter(|value| value.is_number()) {
            result[target] = value.clone();
        }
    }
    if let Some(stop) = body.get("stop") {
        result["stop_sequences"] = match stop {
            Value::String(_) => json!([stop]),
            Value::Array(_) => stop.clone(),
            _ => Value::Null,
        };
    }
    let tools_disabled = body.get("tool_choice").and_then(Value::as_str) == Some("none");
    if let Some(tools) = body
        .get("tools")
        .and_then(Value::as_array)
        .filter(|_| !tools_disabled)
    {
        let translated_tools = tools
            .iter()
            .filter_map(|tool| {
                let function = tool.get("function")?;
                let name = function.get("name")?.as_str()?;
                Some(json!({
                    "name": name,
                    "description": function.get("description").cloned().unwrap_or(Value::Null),
                    "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))
                }))
            })
            .collect::<Vec<_>>();
        if !translated_tools.is_empty() {
            result["tools"] = json!(translated_tools);
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        let translated_choice = match choice.as_str() {
            Some("auto") => Some(json!({"type":"auto"})),
            Some("required") => Some(json!({"type":"any"})),
            Some("none") => None,
            _ => choice
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(|name| json!({"type":"tool", "name":name})),
        };
        if let Some(choice) = translated_choice {
            result["tool_choice"] = choice;
        }
    }
    Ok(result)
}

fn anthropic_to_chat(body: &Value, model: &str) -> Result<Value> {
    let source = body
        .get("messages")
        .and_then(Value::as_array)
        .context("Anthropic request must include a messages array")?;
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        let content = content_text(Some(system));
        if !content.is_empty() {
            messages.push(json!({"role":"system", "content":content}));
        }
    }

    for message in source {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let blocks = anthropic_blocks(message.get("content"));
        let mut parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => parts.push(json!({"type":"text", "text":block.get("text").cloned().unwrap_or(Value::Null)})),
                "image" => {
                    if let Some(url) = anthropic_image_url(&block) {
                        parts.push(json!({"type":"image_url", "image_url":{"url":url}}));
                    }
                }
                "tool_use" if role == "assistant" => tool_calls.push(json!({
                    "id": block.get("id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": block.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": serde_json::to_string(block.get("input").unwrap_or(&Value::Null))?
                    }
                })),
                "tool_result" => tool_results.push(json!({
                    "role":"tool",
                    "tool_call_id":block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                    "content":content_text(block.get("content"))
                })),
                _ => {}
            }
        }
        if !parts.is_empty() || !tool_calls.is_empty() || role == "assistant" {
            let content = if parts.is_empty() {
                Value::Null
            } else if parts
                .iter()
                .all(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            {
                json!(
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>()
                )
            } else {
                json!(parts)
            };
            let mut translated = json!({"role":role, "content":content});
            if !tool_calls.is_empty() {
                translated["tool_calls"] = json!(tool_calls);
            }
            messages.push(translated);
        }
        messages.extend(tool_results);
    }

    let mut result = json!({
        "model": model,
        "messages": messages,
        "stream": false
    });
    if let Some(value) = body.get("max_tokens") {
        result["max_tokens"] = value.clone();
    }
    for field in ["temperature", "top_p"] {
        if let Some(value) = body.get(field).filter(|value| value.is_number()) {
            result[field] = value.clone();
        }
    }
    if let Some(stops) = body.get("stop_sequences") {
        result["stop"] = stops.clone();
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        result["tools"] = json!(tools.iter().map(|tool| json!({
            "type":"function",
            "function":{
                "name":tool.get("name").cloned().unwrap_or(Value::Null),
                "description":tool.get("description").cloned().unwrap_or(Value::Null),
                "parameters":tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object"}))
            }
        })).collect::<Vec<_>>());
    }
    if let Some(choice) = body.get("tool_choice") {
        result["tool_choice"] = match choice.get("type").and_then(Value::as_str) {
            Some("auto") => json!("auto"),
            Some("none") => json!("none"),
            Some("any") => json!("required"),
            Some("tool") => {
                json!({"type":"function", "function":{"name":choice.get("name").cloned().unwrap_or(Value::Null)}})
            }
            _ => Value::Null,
        };
    }
    Ok(result)
}

fn chat_response_to_anthropic(body: &Value, model: &str) -> Result<Value> {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .context("chat response has no choices")?;
    let message = choice
        .get("message")
        .context("chat response has no message")?;
    let mut content = Vec::new();
    if let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        content.push(json!({"type":"text", "text":text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let function = call.get("function").unwrap_or(&Value::Null);
            content.push(json!({
                "type":"tool_use",
                "id":call.get("id").cloned().unwrap_or(Value::Null),
                "name":function.get("name").cloned().unwrap_or(Value::Null),
                "input":parse_arguments(function.get("arguments"))
            }));
        }
    }
    let usage = body.get("usage").unwrap_or(&Value::Null);
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    Ok(json!({
        "id":body.get("id").cloned().unwrap_or_else(|| json!("msg_magpie")),
        "type":"message",
        "role":"assistant",
        "model":model,
        "content":content,
        "stop_reason":match finish { "length" => "max_tokens", "tool_calls" => "tool_use", _ => "end_turn" },
        "stop_sequence":null,
        "usage":{
            "input_tokens":usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens":usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    }))
}

fn anthropic_response_to_chat(body: &Value, model: &str) -> Result<Value> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            "tool_use" => calls.push(json!({
                "id":block.get("id").cloned().unwrap_or(Value::Null),
                "type":"function",
                "function":{
                    "name":block.get("name").cloned().unwrap_or(Value::Null),
                    "arguments":serde_json::to_string(block.get("input").unwrap_or(&Value::Null))?
                }
            })),
            _ => {}
        }
    }
    let mut message = json!({"role":"assistant", "content":if text.is_empty() { Value::Null } else { json!(text) }});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let usage = body.get("usage").unwrap_or(&Value::Null);
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt = input
        .saturating_add(
            usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
        .saturating_add(
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    let stop = body
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    Ok(json!({
        "id":body.get("id").cloned().unwrap_or_else(|| json!("chatcmpl_magpie")),
        "object":"chat.completion",
        "created":0,
        "model":model,
        "choices":[{"index":0,"message":message,"finish_reason":match stop { "max_tokens" => "length", "tool_use" => "tool_calls", _ => "stop" }}],
        "usage":{"prompt_tokens":prompt,"completion_tokens":output,"total_tokens":prompt.saturating_add(output)}
    }))
}

fn chat_content_blocks(content: Option<&Value>) -> Vec<Value> {
    let Some(content) = content else {
        return Vec::new();
    };
    if let Some(text) = content.as_str() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type":"text", "text":text})]
        };
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(
            |part| match part.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => Some(
                    json!({"type":"text", "text":part.get("text").cloned().unwrap_or(Value::Null)}),
                ),
                "image_url" => part
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .and_then(chat_image_source),
                _ => None,
            },
        )
        .collect()
}

fn chat_image_source(url: &str) -> Option<Value> {
    if let Some(data) = url.strip_prefix("data:") {
        let (metadata, encoded) = data.split_once(',')?;
        let media_type = metadata.strip_suffix(";base64")?;
        Some(
            json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":encoded}}),
        )
    } else {
        Some(json!({"type":"image","source":{"type":"url","url":url}}))
    }
}

fn anthropic_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({"type":"text", "text":text})],
        Some(Value::Array(blocks)) => blocks.clone(),
        _ => Vec::new(),
    }
}

fn anthropic_image_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type")?.as_str()? {
        "base64" => Some(format!(
            "data:{};base64,{}",
            source.get("media_type")?.as_str()?,
            source.get("data")?.as_str()?
        )),
        "url" => source.get("url")?.as_str().map(str::to_owned),
        _ => None,
    }
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn parse_arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(_)) | Some(Value::Array(_)) => {
            value.cloned().unwrap_or_else(|| json!({}))
        }
        Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_tools_translate_to_anthropic_and_back() {
        let body = json!({
            "model":"vendor/model",
            "messages":[{"role":"user","content":"hi"},{"role":"assistant","tool_calls":[{"id":"call_1","function":{"name":"search","arguments":"{\"q\":\"rust\"}"}}]}],
            "tools":[{"type":"function","function":{"name":"search","parameters":{"type":"object"}}}],
            "max_tokens":100
        });
        let anthropic = request(&body, ApiProtocol::Chat, ApiProtocol::Anthropic, "model")
            .expect("convert chat request");
        assert_eq!(anthropic["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(anthropic["messages"][1]["content"][0]["input"]["q"], "rust");
        assert_eq!(anthropic["tools"][0]["input_schema"]["type"], "object");

        let result = json!({"id":"msg_1","model":"model","content":[{"type":"tool_use","id":"call_1","name":"search","input":{"q":"rust"}}],"stop_reason":"tool_use","usage":{"input_tokens":7,"output_tokens":3}});
        let chat = response(
            &result,
            ApiProtocol::Anthropic,
            ApiProtocol::Chat,
            "vendor/model",
        )
        .expect("convert Anthropic response");
        assert_eq!(
            chat["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_1"
        );
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["model"], "vendor/model");
    }

    #[test]
    fn translated_streams_emit_protocol_framing() {
        let chat = json!({"id":"chat_1","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]});
        let events = String::from_utf8(
            completed_stream(&chat, ApiProtocol::Chat, "provider/model")
                .expect("encode completed chat response as a stream"),
        )
        .expect("valid UTF-8 SSE");
        assert!(events.contains("data: [DONE]\n\n"));
        assert!(events.contains("hello"));

        let anthropic = json!({"id":"msg_1","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}});
        let events = String::from_utf8(
            completed_stream(&anthropic, ApiProtocol::Anthropic, "provider/model")
                .expect("encode completed Anthropic response as a stream"),
        )
        .expect("valid UTF-8 SSE");
        assert!(events.contains("event: message_start"));
        assert!(events.contains("event: message_stop"));
    }

    #[test]
    fn anthropic_events_are_translated_incrementally_across_chunk_boundaries() {
        let source = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":2}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut translator =
            SseTranslator::new(ApiProtocol::Anthropic, ApiProtocol::Chat, "provider/model")
                .expect("supported protocol pair");
        let mut output = Vec::new();
        for chunk in source.as_bytes().chunks(11) {
            output.extend(translator.push(chunk));
        }
        output.extend(translator.finish());
        let output = String::from_utf8(output.into_iter().flatten().collect())
            .expect("valid UTF-8 translated SSE");

        assert!(output.contains("chat.completion.chunk"));
        assert!(output.contains("hello"));
        assert!(output.contains("data: [DONE]\n\n"));
    }

    #[test]
    fn chat_tool_events_become_anthropic_tool_blocks() {
        let events = [
            json!({"id":"chat_1","choices":[{"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}),
            json!({"id":"chat_1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"search","arguments":"{\"q\":"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust\"}"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let source = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let mut translator = SseTranslator::new(ApiProtocol::Chat, ApiProtocol::Anthropic, "model")
            .expect("supported protocol pair");
        let mut output = Vec::new();
        for chunk in source.as_bytes().chunks(13) {
            output.extend(translator.push(chunk));
        }
        output.extend(translator.finish());
        let output = String::from_utf8(output.into_iter().flatten().collect())
            .expect("valid UTF-8 translated SSE");

        assert!(output.contains("event: message_start"));
        assert!(output.contains("\"type\":\"tool_use\""));
        assert!(output.contains("\"name\":\"search\""));
        assert!(output.contains("\"partial_json\":\"{\\\"q\\\":\""));
        assert!(output.contains("\"stop_reason\":\"tool_use\""));
        assert!(output.contains("event: message_stop"));
    }
}
