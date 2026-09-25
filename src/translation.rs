use std::collections::{BTreeMap, HashMap, HashSet};

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
        (ApiProtocol::Chat, ApiProtocol::Responses) => chat_to_responses(body, model),
        (ApiProtocol::Responses, ApiProtocol::Chat) => responses_to_chat(body, model),
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
        (ApiProtocol::Responses, ApiProtocol::Chat) => responses_response_to_chat(body, model),
        (ApiProtocol::Chat, ApiProtocol::Responses) => chat_response_to_responses(body, model),
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
    ResponsesToChat(ChatStream),
    ChatToResponses(ResponsesStream),
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
    tool_args_seen: HashSet<usize>,
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

#[derive(Default)]
struct ResponsesStream {
    id: String,
    model: String,
    created: u64,
    sequence: u64,
    started: bool,
    ended: bool,
    next_output_index: usize,
    open: Option<OpenResponseItem>,
    tools: BTreeMap<usize, ResponsesTool>,
    output: Vec<Value>,
    usage: Option<Value>,
    finish_reason: Option<String>,
}

struct OpenResponseItem {
    id: String,
    output_index: usize,
    text: String,
    kind: OpenResponseKind,
}

enum OpenResponseKind {
    Text,
    Reasoning,
}

#[derive(Clone, Default)]
struct ResponsesTool {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
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
            (ApiProtocol::Responses, ApiProtocol::Chat) => {
                StreamDirection::ResponsesToChat(ChatStream::default())
            }
            (ApiProtocol::Chat, ApiProtocol::Responses) => {
                StreamDirection::ChatToResponses(ResponsesStream::default())
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
            StreamDirection::ResponsesToChat(state) => finish_chat_stream(state, &self.model),
            StreamDirection::ChatToResponses(state) => finish_responses_stream(state, &self.model),
        });
        output
    }

    pub(crate) fn is_ended(&self) -> bool {
        match &self.direction {
            StreamDirection::AnthropicToChat(state) => state.ended,
            StreamDirection::ChatToAnthropic(state) => state.ended,
            StreamDirection::ResponsesToChat(state) => state.ended,
            StreamDirection::ChatToResponses(state) => state.ended,
        }
    }

    fn process_event(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        if match &self.direction {
            StreamDirection::AnthropicToChat(state) => state.ended,
            StreamDirection::ChatToAnthropic(state) => state.ended,
            StreamDirection::ResponsesToChat(state) => state.ended,
            StreamDirection::ChatToResponses(state) => state.ended,
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
            StreamDirection::ResponsesToChat(state) => {
                responses_event_to_chat(state, &self.model, kind, &value)
            }
            StreamDirection::ChatToResponses(state) => {
                chat_event_to_responses(state, &self.model, &value)
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
            StreamDirection::ResponsesToChat(state) => {
                state.ended = true;
                output.push(data_frame(&json!({
                    "error": {"message":message,"type":"api_error","code":null}
                })));
                output.push(b"data: [DONE]\n\n".to_vec());
            }
            StreamDirection::ChatToResponses(state) => {
                state.ended = true;
                output.push(event_frame(
                    "error",
                    &json!({"type":"error","error":{"type":"api_error","message":message}}),
                ));
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

fn responses_event_to_chat(
    state: &mut ChatStream,
    model: &str,
    kind: &str,
    value: &Value,
) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    let response = value.get("response").unwrap_or(&Value::Null);
    match kind {
        "response.created" | "response.in_progress" => {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                state.id = id.to_owned();
            }
            emit_chat_role(state, model, &mut output);
        }
        "response.output_item.added" => {
            let item = value.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let tool_index = chat_tool_index(state, output_index);
                emit_chat_role(state, model, &mut output);
                output.push(chat_delta_frame(
                    state,
                    model,
                    json!({"tool_calls":[{
                        "index":tool_index,
                        "id":item.get("call_id").cloned().unwrap_or_else(|| item.get("id").cloned().unwrap_or(Value::Null)),
                        "type":"function",
                        "function":{"name":item.get("name").cloned().unwrap_or(Value::Null),"arguments":""}
                    }]}),
                ));
            }
        }
        "response.output_text.delta" => {
            if let Some(text) = value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                emit_chat_role(state, model, &mut output);
                output.push(chat_delta_frame(state, model, json!({"content":text})));
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(text) = value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                emit_chat_role(state, model, &mut output);
                output.push(chat_delta_frame(
                    state,
                    model,
                    json!({"reasoning_content":text}),
                ));
            }
        }
        "response.function_call_arguments.delta" => {
            let output_index = value
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let tool_index = chat_tool_index(state, output_index);
            if let Some(arguments) = value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                state.tool_args_seen.insert(tool_index);
                emit_chat_role(state, model, &mut output);
                output.push(chat_delta_frame(
                    state,
                    model,
                    json!({"tool_calls":[{"index":tool_index,"function":{"arguments":arguments}}]}),
                ));
            }
        }
        "response.output_item.done" => {
            let item = value.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let tool_index = chat_tool_index(state, output_index);
                if !state.tool_args_seen.contains(&tool_index)
                    && let Some(arguments) = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                {
                    emit_chat_role(state, model, &mut output);
                    output.push(chat_delta_frame(
                        state,
                        model,
                        json!({"tool_calls":[{"index":tool_index,"function":{"arguments":arguments}}]}),
                    ));
                    state.tool_args_seen.insert(tool_index);
                }
            }
        }
        "response.completed" | "response.incomplete" => {
            let status = response
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let reason = response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            state.stop_reason = Some(if status == "incomplete" || kind == "response.incomplete" {
                if reason == "content_filter" {
                    "content_filter".to_owned()
                } else {
                    "length".to_owned()
                }
            } else if response
                .get("output")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.get("type").and_then(Value::as_str) == Some("function_call")
                    })
                })
                || !state.tool_indices.is_empty()
            {
                "tool_calls".to_owned()
            } else {
                "stop".to_owned()
            });
            if let Some(usage) = response.get("usage") {
                state.usage = Some(responses_usage_for_chat(usage));
            }
            output.extend(finish_chat_stream(state, model));
        }
        "response.failed" => {
            state.ended = true;
            let message = response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("provider response failed");
            output.push(data_frame(&json!({
                "error":{"message":message,"type":"api_error","code":null}
            })));
            output.push(b"data: [DONE]\n\n".to_vec());
        }
        "error" => {
            state.ended = true;
            let message = value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("provider returned an error");
            output.push(data_frame(&json!({
                "error":{"message":message,"type":"api_error","code":null}
            })));
            output.push(b"data: [DONE]\n\n".to_vec());
        }
        _ => {}
    }
    output
}

fn chat_tool_index(state: &mut ChatStream, output_index: usize) -> usize {
    if let Some(index) = state.tool_indices.get(&output_index) {
        *index
    } else {
        let index = state.next_tool_index;
        state.next_tool_index += 1;
        state.tool_indices.insert(output_index, index);
        index
    }
}

fn responses_usage_for_chat(usage: &Value) -> Value {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_tokens = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input_tokens);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "input_tokens":input_tokens - cached_tokens,
        "cache_read_input_tokens":cached_tokens,
        "output_tokens":output_tokens
    })
}

fn chat_event_to_responses(
    state: &mut ResponsesStream,
    model: &str,
    value: &Value,
) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(id) = value.get("id").and_then(Value::as_str) {
        state.id = id.to_owned();
    }
    if let Some(created) = value.get("created").and_then(Value::as_u64) {
        state.created = created;
    }
    if let Some(usage) = value.get("usage") {
        state.usage = Some(responses_usage_from_chat(usage));
    }
    if let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    {
        start_responses_stream(state, model, &mut output);
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let output_index = ensure_responses_text(state, model, false, &mut output);
            let item_id = if let Some(open) = state.open.as_mut() {
                open.text.push_str(text);
                Some(open.id.clone())
            } else {
                None
            };
            if let Some(item_id) = item_id {
                output.push(responses_stream_frame(
                    state,
                    "response.output_text.delta",
                    json!({"item_id":item_id,"output_index":output_index,"content_index":0,"delta":text,"logprobs":[]}),
                ));
            }
        }
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str);
        if let Some(text) = reasoning.filter(|text| !text.is_empty()) {
            let output_index = ensure_responses_text(state, model, true, &mut output);
            let item_id = if let Some(open) = state.open.as_mut() {
                open.text.push_str(text);
                Some(open.id.clone())
            } else {
                None
            };
            if let Some(item_id) = item_id {
                output.push(responses_stream_frame(
                    state,
                    "response.reasoning_summary_text.delta",
                    json!({"item_id":item_id,"output_index":output_index,"summary_index":0,"delta":text}),
                ));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (ordinal, call) in calls.iter().enumerate() {
                let tool_index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|index| index as usize)
                    .unwrap_or(ordinal);
                let id = call.get("id").and_then(Value::as_str);
                let name = call.pointer("/function/name").and_then(Value::as_str);
                let is_new_tool = !state.tools.contains_key(&tool_index);
                let output_index =
                    ensure_responses_tool(state, model, tool_index, id, name, &mut output);
                let tool = state.tools.entry(tool_index).or_default();
                if !is_new_tool && let Some(id) = id {
                    if tool.call_id.starts_with("call_magpie_") {
                        tool.call_id = id.to_owned();
                    } else {
                        tool.call_id.push_str(id);
                    }
                }
                if !is_new_tool && let Some(name) = name {
                    tool.name.push_str(name);
                }
                if let Some(arguments) = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .filter(|arguments| !arguments.is_empty())
                {
                    tool.arguments.push_str(arguments);
                    let item_id = tool.item_id.clone();
                    output.push(responses_stream_frame(
                        state,
                        "response.function_call_arguments.delta",
                        json!({"item_id":item_id,"output_index":output_index,"delta":arguments}),
                    ));
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            state.finish_reason = Some(reason.to_owned());
        }
    } else if value.get("usage").is_some() {
        start_responses_stream(state, model, &mut output);
    }
    output
}

fn start_responses_stream(state: &mut ResponsesStream, model: &str, output: &mut Vec<Vec<u8>>) {
    if state.started {
        return;
    }
    if state.id.is_empty() {
        state.id = "resp_magpie".to_owned();
    } else if !state.id.starts_with("resp_") {
        let id = state
            .id
            .strip_prefix("chatcmpl-")
            .or_else(|| state.id.strip_prefix("chatcmpl_"))
            .unwrap_or(&state.id);
        state.id = format!("resp_{id}");
    }
    state.model = model.to_owned();
    state.started = true;
    let response = responses_stream_response(state, "in_progress", Value::Null);
    output.push(responses_stream_frame(
        state,
        "response.created",
        json!({"response":response}),
    ));
    let response = responses_stream_response(state, "in_progress", Value::Null);
    output.push(responses_stream_frame(
        state,
        "response.in_progress",
        json!({"response":response}),
    ));
}

fn ensure_responses_text(
    state: &mut ResponsesStream,
    model: &str,
    reasoning: bool,
    output: &mut Vec<Vec<u8>>,
) -> usize {
    let same_kind = state.open.as_ref().is_some_and(|open| {
        matches!(
            (&open.kind, reasoning),
            (OpenResponseKind::Text, false) | (OpenResponseKind::Reasoning, true)
        )
    });
    if same_kind {
        return state.open.as_ref().map_or(0, |open| open.output_index);
    }
    close_responses_item(state, model, output);
    let output_index = state.next_output_index;
    state.next_output_index += 1;
    let item_id = if reasoning {
        format!("rs_magpie_{output_index}")
    } else {
        format!("msg_magpie_{output_index}")
    };
    state.output.push(Value::Null);
    if reasoning {
        output.push(responses_stream_frame(
            state,
            "response.output_item.added",
            json!({"output_index":output_index,"item":{"id":item_id,"type":"reasoning","status":"in_progress","summary":[]}}),
        ));
        output.push(responses_stream_frame(
            state,
            "response.reasoning_summary_part.added",
            json!({"item_id":item_id,"output_index":output_index,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        ));
    } else {
        output.push(responses_stream_frame(
            state,
            "response.output_item.added",
            json!({"output_index":output_index,"item":{"id":item_id,"type":"message","role":"assistant","status":"in_progress","content":[]}}),
        ));
        output.push(responses_stream_frame(
            state,
            "response.content_part.added",
            json!({"item_id":item_id,"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}),
        ));
    }
    state.open = Some(OpenResponseItem {
        id: item_id,
        output_index,
        text: String::new(),
        kind: if reasoning {
            OpenResponseKind::Reasoning
        } else {
            OpenResponseKind::Text
        },
    });
    output_index
}

fn close_responses_item(state: &mut ResponsesStream, _model: &str, output: &mut Vec<Vec<u8>>) {
    let Some(open) = state.open.take() else {
        return;
    };
    let item = match open.kind {
        OpenResponseKind::Text => {
            let part =
                json!({"type":"output_text","text":open.text,"annotations":[],"logprobs":[]});
            output.push(responses_stream_frame(
                state,
                "response.output_text.done",
                json!({"item_id":open.id,"output_index":open.output_index,"content_index":0,"text":open.text,"logprobs":[]}),
            ));
            output.push(responses_stream_frame(
                state,
                "response.content_part.done",
                json!({"item_id":open.id,"output_index":open.output_index,"content_index":0,"part":part}),
            ));
            json!({"id":open.id,"type":"message","role":"assistant","status":"completed","content":[part]})
        }
        OpenResponseKind::Reasoning => {
            let part = json!({"type":"summary_text","text":open.text});
            output.push(responses_stream_frame(
                state,
                "response.reasoning_summary_text.done",
                json!({"item_id":open.id,"output_index":open.output_index,"summary_index":0,"text":open.text}),
            ));
            output.push(responses_stream_frame(
                state,
                "response.reasoning_summary_part.done",
                json!({"item_id":open.id,"output_index":open.output_index,"summary_index":0,"part":part}),
            ));
            json!({"id":open.id,"type":"reasoning","status":"completed","summary":[part]})
        }
    };
    output.push(responses_stream_frame(
        state,
        "response.output_item.done",
        json!({"output_index":open.output_index,"item":item}),
    ));
    if let Some(slot) = state.output.get_mut(open.output_index) {
        *slot = item;
    }
}

fn ensure_responses_tool(
    state: &mut ResponsesStream,
    model: &str,
    tool_index: usize,
    id: Option<&str>,
    name: Option<&str>,
    output: &mut Vec<Vec<u8>>,
) -> usize {
    if let Some(tool) = state.tools.get(&tool_index) {
        return tool.output_index;
    }
    close_responses_item(state, model, output);
    let output_index = state.next_output_index;
    state.next_output_index += 1;
    let call_id = id
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("call_magpie_{tool_index}"));
    let item_id = format!("fc_magpie_{output_index}");
    let name = name.unwrap_or_default().to_owned();
    state.output.push(Value::Null);
    state.tools.insert(
        tool_index,
        ResponsesTool {
            item_id: item_id.clone(),
            call_id: call_id.clone(),
            name: name.clone(),
            arguments: String::new(),
            output_index,
        },
    );
    output.push(responses_stream_frame(
        state,
        "response.output_item.added",
        json!({"output_index":output_index,"item":{"id":item_id,"type":"function_call","status":"in_progress","call_id":call_id,"name":name,"arguments":""}}),
    ));
    output_index
}

fn finish_responses_stream(state: &mut ResponsesStream, model: &str) -> Vec<Vec<u8>> {
    if state.ended {
        return Vec::new();
    }
    let mut output = Vec::new();
    start_responses_stream(state, model, &mut output);
    close_responses_item(state, model, &mut output);
    let mut tools = state.tools.values().cloned().collect::<Vec<_>>();
    tools.sort_by_key(|tool| tool.output_index);
    for tool in tools {
        let arguments = if tool.arguments.is_empty() {
            "{}"
        } else {
            tool.arguments.as_str()
        };
        output.push(responses_stream_frame(
            state,
            "response.function_call_arguments.done",
            json!({"item_id":tool.item_id,"output_index":tool.output_index,"call_id":tool.call_id,"name":tool.name,"arguments":arguments}),
        ));
        let item = json!({"id":tool.item_id,"type":"function_call","status":"completed","call_id":tool.call_id,"name":tool.name,"arguments":arguments});
        output.push(responses_stream_frame(
            state,
            "response.output_item.done",
            json!({"output_index":tool.output_index,"item":item}),
        ));
        if let Some(slot) = state.output.get_mut(tool.output_index) {
            *slot = item;
        }
    }
    let incomplete_reason = match state.finish_reason.as_deref().unwrap_or("stop") {
        "length" => Some("max_output_tokens"),
        "content_filter" => Some("content_filter"),
        _ => None,
    };
    let status = if incomplete_reason.is_some() {
        "incomplete"
    } else {
        "completed"
    };
    let incomplete = incomplete_reason
        .map(|reason| json!({"reason":reason}))
        .unwrap_or(Value::Null);
    let response = responses_stream_response(state, status, incomplete);
    let event = if status == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    output.push(responses_stream_frame(
        state,
        event,
        json!({"response":response}),
    ));
    state.ended = true;
    output
}

fn responses_stream_response(state: &ResponsesStream, status: &str, incomplete: Value) -> Value {
    json!({
        "id":state.id,
        "object":"response",
        "created_at":state.created,
        "status":status,
        "model":state.model,
        "output":state.output,
        "usage":state.usage,
        "incomplete_details":incomplete,
        "parallel_tool_calls":true,
        "tool_choice":"auto",
        "tools":[],
        "error":null
    })
}

fn responses_stream_frame(state: &mut ResponsesStream, event: &str, value: Value) -> Vec<u8> {
    responses_event_frame(&mut state.sequence, event, value)
}

fn responses_usage_from_chat(usage: &Value) -> Value {
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input_tokens);
    let reasoning_tokens = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "input_tokens":input_tokens,
        "output_tokens":output_tokens,
        "total_tokens":usage.get("total_tokens").and_then(Value::as_u64).unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
        "input_tokens_details":{"cached_tokens":cached_tokens},
        "output_tokens_details":{"reasoning_tokens":reasoning_tokens}
    })
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
        "max_tokens" | "length" => "length",
        "tool_use" | "tool_calls" => "tool_calls",
        "content_filter" | "filter" => "content_filter",
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
        if let Some(content) = choice
            .pointer("/delta/content")
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
        {
            let index = ensure_text_block(state, model, &mut output);
            output.push(event_frame(
                "content_block_delta",
                &json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":content}}),
            ));
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
        ApiProtocol::Responses => {
            return responses_completed_stream(body, model);
        }
    }
    Ok(output.into_iter().flatten().collect())
}

fn responses_completed_stream(body: &Value, model: &str) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut sequence = 0_u64;
    let mut in_progress = body.clone();
    in_progress["object"] = json!("response");
    in_progress["status"] = json!("in_progress");
    in_progress["model"] = json!(body.get("model").and_then(Value::as_str).unwrap_or(model));
    in_progress["output"] = json!([]);
    output.push(responses_event_frame(
        &mut sequence,
        "response.created",
        json!({"response":in_progress}),
    ));
    output.push(responses_event_frame(
        &mut sequence,
        "response.in_progress",
        json!({"response":in_progress}),
    ));

    for (output_index, item) in body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let item_id = item.get("id").cloned().unwrap_or(Value::Null);
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        output.push(responses_event_frame(
            &mut sequence,
            "response.output_item.added",
            json!({"output_index":output_index,"item":added}),
        ));
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                for (content_index, part) in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if part.get("type").and_then(Value::as_str) != Some("output_text") {
                        continue;
                    }
                    let text = part.get("text").cloned().unwrap_or_else(|| json!(""));
                    let mut streaming_part = part.clone();
                    streaming_part["text"] = json!("");
                    output.push(responses_event_frame(
                        &mut sequence,
                        "response.content_part.added",
                        json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"part":streaming_part}),
                    ));
                    if text.as_str().is_some_and(|text| !text.is_empty()) {
                        output.push(responses_event_frame(
                            &mut sequence,
                            "response.output_text.delta",
                            json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"delta":text,"logprobs":[]}),
                        ));
                    }
                    output.push(responses_event_frame(
                        &mut sequence,
                        "response.output_text.done",
                        json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"text":text,"logprobs":[]}),
                    ));
                    output.push(responses_event_frame(
                        &mut sequence,
                        "response.content_part.done",
                        json!({"item_id":item_id,"output_index":output_index,"content_index":content_index,"part":part}),
                    ));
                }
            }
            "reasoning" => {
                for (summary_index, part) in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let text = part.get("text").cloned().unwrap_or_else(|| json!(""));
                    let mut streaming_part = part.clone();
                    streaming_part["text"] = json!("");
                    output.push(responses_event_frame(
                        &mut sequence,
                        "response.reasoning_summary_part.added",
                        json!({"item_id":item_id,"output_index":output_index,"summary_index":summary_index,"part":streaming_part}),
                    ));
                    if text.as_str().is_some_and(|text| !text.is_empty()) {
                        output.push(responses_event_frame(
                            &mut sequence,
                            "response.reasoning_summary_text.delta",
                            json!({"item_id":item_id,"output_index":output_index,"summary_index":summary_index,"delta":text}),
                        ));
                    }
                    output.push(responses_event_frame(
                        &mut sequence,
                        "response.reasoning_summary_text.done",
                        json!({"item_id":item_id,"output_index":output_index,"summary_index":summary_index,"text":text}),
                    ));
                    output.push(responses_event_frame(
                        &mut sequence,
                        "response.reasoning_summary_part.done",
                        json!({"item_id":item_id,"output_index":output_index,"summary_index":summary_index,"part":part}),
                    ));
                }
            }
            "function_call" => {
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                if !arguments.is_empty() {
                    output.push(responses_event_frame(
                        &mut sequence,
                        "response.function_call_arguments.delta",
                        json!({"item_id":item_id,"output_index":output_index,"delta":arguments}),
                    ));
                }
                output.push(responses_event_frame(
                    &mut sequence,
                    "response.function_call_arguments.done",
                    json!({"item_id":item_id,"output_index":output_index,"call_id":item.get("call_id").cloned().unwrap_or(Value::Null),"name":item.get("name").cloned().unwrap_or(Value::Null),"arguments":arguments}),
                ));
            }
            _ => {}
        }
        output.push(responses_event_frame(
            &mut sequence,
            "response.output_item.done",
            json!({"output_index":output_index,"item":item}),
        ));
    }

    let incomplete = body.get("status").and_then(Value::as_str) == Some("incomplete");
    let terminal_event = if incomplete {
        "response.incomplete"
    } else {
        "response.completed"
    };
    output.push(responses_event_frame(
        &mut sequence,
        terminal_event,
        json!({"response":body}),
    ));
    Ok(output.into_iter().flatten().collect())
}

fn responses_event_frame(sequence: &mut u64, event: &str, mut value: Value) -> Vec<u8> {
    if let Some(object) = value.as_object_mut() {
        object.insert("type".to_owned(), json!(event));
        object.insert("sequence_number".to_owned(), json!(*sequence));
    }
    *sequence = (*sequence).saturating_add(1);
    event_frame(event, &value)
}

fn data_frame(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn event_frame(event: &str, value: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {value}\n\n").into_bytes()
}

fn chat_to_responses(body: &Value, model: &str) -> Result<Value> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .context("chat request must include a messages array")?;
    let mut instructions = Vec::new();
    let mut input = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" | "developer" => {
                let text = content_text(message.get("content"));
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            "tool" => input.push(json!({
                "type":"function_call_output",
                "call_id":message.get("tool_call_id").cloned().unwrap_or_else(|| json!("")),
                "output":responses_output(message.get("content"))?
            })),
            "user" | "assistant" => {
                let content = chat_content_for_responses(message.get("content"), role);
                if !content.is_empty() {
                    input.push(json!({"type":"message","role":role,"content":content}));
                }
                if role == "assistant"
                    && let Some(calls) = message.get("tool_calls").and_then(Value::as_array)
                {
                    for call in calls {
                        let function = call.get("function").unwrap_or(&Value::Null);
                        let arguments = function.get("arguments").unwrap_or(&Value::Null);
                        input.push(json!({
                            "type":"function_call",
                            "call_id":call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).unwrap_or("call_magpie"),
                            "name":function.get("name").cloned().unwrap_or_else(|| json!("")),
                            "arguments":string_or_json(arguments)?
                        }));
                    }
                }
            }
            _ => {}
        }
    }

    let mut result = json!({
        "model":model,
        "input":input,
        "stream":body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "store":false
    });
    if !instructions.is_empty() {
        result["instructions"] = json!(instructions.join("\n\n"));
    }
    if let Some(max_tokens) = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .filter(|value| value.is_number())
    {
        result["max_output_tokens"] = max_tokens.clone();
    }
    for field in ["temperature", "top_p"] {
        if let Some(value) = body.get(field).filter(|value| value.is_number()) {
            result[field] = value.clone();
        }
    }
    if let Some(value) = body
        .get("parallel_tool_calls")
        .filter(|value| value.is_boolean())
    {
        result["parallel_tool_calls"] = value.clone();
    }
    if let Some(effort) = body.get("reasoning_effort").and_then(Value::as_str) {
        result["reasoning"] = json!({"effort":effort,"summary":"auto"});
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let translated = tools
            .iter()
            .filter_map(|tool| {
                let function = tool.get("function")?;
                let mut translated = json!({
                    "type":"function",
                    "name":function.get("name")?,
                    "parameters":function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))
                });
                if let Some(description) = function.get("description").filter(|value| !value.is_null()) {
                    translated["description"] = description.clone();
                }
                if let Some(strict) = function.get("strict").filter(|value| value.is_boolean()) {
                    translated["strict"] = strict.clone();
                }
                Some(translated)
            })
            .collect::<Vec<_>>();
        if !translated.is_empty() {
            result["tools"] = json!(translated);
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        let translated = match choice.as_str() {
            Some("none") => Some(json!("none")),
            Some("auto") => Some(json!("auto")),
            Some("required") => Some(json!("required")),
            _ => choice
                .pointer("/function/name")
                .cloned()
                .map(|name| json!({"type":"function","name":name})),
        };
        if let Some(translated) = translated {
            result["tool_choice"] = translated;
        }
    }
    Ok(result)
}

fn responses_to_chat(body: &Value, model: &str) -> Result<Value> {
    let mut messages = Vec::new();
    let mut instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let input = body
        .get("input")
        .context("Responses request must include input")?;
    if let Some(text) = input.as_str() {
        messages.push(json!({"role":"user","content":text}));
    } else {
        let items = input
            .as_array()
            .context("Responses input must be a string or an array")?;
        for item in items {
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "message" | "" if item.get("role").is_some() => {
                    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                    let content = responses_content_for_chat(item.get("content"));
                    if matches!(role, "system" | "developer") {
                        let text = content_text(Some(&content));
                        if !text.is_empty() {
                            if !instructions.is_empty() {
                                instructions.push_str("\n\n");
                            }
                            instructions.push_str(&text);
                        }
                    } else {
                        messages.push(json!({
                            "role":if role == "assistant" {"assistant"} else {"user"},
                            "content":content
                        }));
                    }
                }
                "function_call" => {
                    let arguments = item.get("arguments").unwrap_or(&Value::Null);
                    messages.push(json!({
                        "role":"assistant",
                        "content":Value::Null,
                        "tool_calls":[{
                            "id":item.get("call_id").cloned().unwrap_or_else(|| json!("")),
                            "type":"function",
                            "function":{
                                "name":item.get("name").cloned().unwrap_or(Value::Null),
                                "arguments":string_or_json(arguments)?
                            }
                        }]
                    }));
                }
                "function_call_output" => messages.push(json!({
                    "role":"tool",
                    "tool_call_id":item.get("call_id").cloned().unwrap_or_else(|| json!("")),
                    "content":responses_output(item.get("output"))?
                })),
                "reasoning" => {
                    let summary = item
                        .get("summary")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>();
                    if !summary.is_empty() {
                        messages.push(json!({"role":"assistant","content":Value::Null,"reasoning_content":summary}));
                    }
                }
                _ => {}
            }
        }
    }
    if !instructions.is_empty() {
        messages.insert(0, json!({"role":"system","content":instructions}));
    }

    let mut result = json!({
        "model":model,
        "messages":messages,
        "stream":body.get("stream").and_then(Value::as_bool).unwrap_or(false)
    });
    if let Some(max_tokens) = body
        .get("max_output_tokens")
        .filter(|value| value.is_number())
    {
        result["max_completion_tokens"] = max_tokens.clone();
    }
    for field in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(value) = body.get(field) {
            result[field] = value.clone();
        }
    }
    if let Some(effort) = body.pointer("/reasoning/effort").and_then(Value::as_str) {
        result["reasoning_effort"] = json!(effort);
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let translated = tools
            .iter()
            .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
            .map(|tool| {
                let mut function = json!({
                    "name":tool.get("name").cloned().unwrap_or(Value::Null),
                    "parameters":tool.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))
                });
                if let Some(description) = tool.get("description").filter(|value| !value.is_null()) {
                    function["description"] = description.clone();
                }
                if let Some(strict) = tool.get("strict").filter(|value| value.is_boolean()) {
                    function["strict"] = strict.clone();
                }
                json!({"type":"function","function":function})
            })
            .collect::<Vec<_>>();
        if !translated.is_empty() {
            result["tools"] = json!(translated);
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        result["tool_choice"] = match choice.as_str() {
            Some("none") => json!("none"),
            Some("auto") => json!("auto"),
            Some("required") => json!("required"),
            _ if choice.get("name").is_some() => json!({
                "type":"function",
                "function":{"name":choice.get("name").cloned().unwrap_or(Value::Null)}
            }),
            _ => Value::Null,
        };
    }
    Ok(result)
}

fn chat_content_for_responses(content: Option<&Value>, role: &str) -> Vec<Value> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"type":text_type,"text":text})]
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                "text" => Some(json!({"type":text_type,"text":part.get("text").cloned().unwrap_or(Value::Null)})),
                "image_url" if role != "assistant" => part
                    .pointer("/image_url/url")
                    .or_else(|| part.pointer("/image_url"))
                    .filter(|url| url.is_string())
                    .map(|url| json!({"type":"input_image","image_url":url})),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn responses_content_for_chat(content: Option<&Value>) -> Value {
    if let Some(text) = content.and_then(Value::as_str) {
        return json!(text);
    }
    let parts = content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(
            |part| match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                "input_text" | "output_text" | "text" => Some(json!({
                    "type":"text",
                    "text":part.get("text").cloned().unwrap_or(Value::Null)
                })),
                "input_image" => part
                    .get("image_url")
                    .and_then(|url| {
                        url.as_str()
                            .or_else(|| url.get("url").and_then(Value::as_str))
                    })
                    .map(|url| json!({"type":"image_url","image_url":{"url":url}})),
                _ => None,
            },
        )
        .collect::<Vec<_>>();
    if parts
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
    }
}

fn responses_output(value: Option<&Value>) -> Result<Value> {
    let Some(value) = value else {
        return Ok(json!(""));
    };
    match value {
        Value::String(_) => Ok(value.clone()),
        _ => Ok(json!(string_or_json(value)?)),
    }
}

fn string_or_json(value: &Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        _ => serde_json::to_string(value).context("serialize tool arguments"),
    }
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

fn responses_response_to_chat(body: &Value, model: &str) -> Result<Value> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "output_text" | "text" => {
                            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                        }
                        "refusal" => {
                            text.push_str(part.get("refusal").and_then(Value::as_str).unwrap_or(""));
                        }
                        _ => {}
                    }
                }
            }
            "reasoning" => {
                reasoning.push_str(
                    &item
                        .get("summary")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>(),
                );
            }
            "function_call" => calls.push(json!({
                "id":item.get("call_id").cloned().unwrap_or_else(|| item.get("id").cloned().unwrap_or(Value::Null)),
                "type":"function",
                "function":{
                    "name":item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments":string_or_json(item.get("arguments").unwrap_or(&Value::Null))?
                }
            })),
            _ => {}
        }
    }
    let mut message = json!({
        "role":"assistant",
        "content":if text.is_empty() { Value::Null } else { json!(text) }
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    let has_calls = !calls.is_empty();
    if has_calls {
        message["tool_calls"] = json!(calls);
    }

    let usage = body.get("usage").unwrap_or(&Value::Null);
    let prompt_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut chat_usage = json!({
        "prompt_tokens":prompt_tokens,
        "completion_tokens":completion_tokens,
        "total_tokens":usage.get("total_tokens").and_then(Value::as_u64).unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens))
    });
    if let Some(cached) = usage.pointer("/input_tokens_details/cached_tokens") {
        chat_usage["prompt_tokens_details"] = json!({"cached_tokens":cached});
    }
    if let Some(reasoning) = usage.pointer("/output_tokens_details/reasoning_tokens") {
        chat_usage["completion_tokens_details"] = json!({"reasoning_tokens":reasoning});
    }

    let status = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let incomplete_reason = body
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let finish = match (status, incomplete_reason, has_calls) {
        ("incomplete", "content_filter", _) => "content_filter",
        ("incomplete", _, _) => "length",
        (_, _, true) => "tool_calls",
        _ => "stop",
    };
    Ok(json!({
        "id":body.get("id").cloned().unwrap_or_else(|| json!("chatcmpl_magpie")),
        "object":"chat.completion",
        "created":0,
        "model":model,
        "choices":[{"index":0,"message":message,"finish_reason":finish}],
        "usage":chat_usage
    }))
}

fn chat_response_to_responses(body: &Value, model: &str) -> Result<Value> {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .context("chat response has no choices")?;
    let message = choice
        .get("message")
        .context("chat response has no message")?;
    let mut output = Vec::new();
    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        output.push(json!({
            "id":"rs_magpie",
            "type":"reasoning",
            "status":"completed",
            "summary":[{"type":"summary_text","text":reasoning}]
        }));
    }
    if let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        output.push(json!({
            "id":"msg_magpie",
            "type":"message",
            "role":"assistant",
            "status":"completed",
            "content":[{"type":"output_text","text":text,"annotations":[]}]
        }));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_magpie_{index}"));
            let function = call.get("function").unwrap_or(&Value::Null);
            output.push(json!({
                "id":format!("fc_{call_id}"),
                "type":"function_call",
                "status":"completed",
                "call_id":call_id,
                "name":function.get("name").cloned().unwrap_or(Value::Null),
                "arguments":string_or_json(function.get("arguments").unwrap_or(&Value::Null))?
            }));
        }
    }

    let usage = body.get("usage").unwrap_or(&Value::Null);
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let (status, incomplete_details) = match finish {
        "length" => ("incomplete", json!({"reason":"max_output_tokens"})),
        "content_filter" => ("incomplete", json!({"reason":"content_filter"})),
        _ => ("completed", Value::Null),
    };
    let id = body.get("id").and_then(Value::as_str).unwrap_or("magpie");
    let id = if id.starts_with("resp_") {
        id.to_owned()
    } else {
        format!("resp_{id}")
    };
    Ok(json!({
        "id":id,
        "object":"response",
        "created_at":body.get("created").and_then(Value::as_u64).unwrap_or(0),
        "status":status,
        "model":model,
        "output":output,
        "usage":{
            "input_tokens":input_tokens,
            "output_tokens":output_tokens,
            "total_tokens":usage.get("total_tokens").and_then(Value::as_u64).unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
            "input_tokens_details":{"cached_tokens":cached_tokens},
            "output_tokens_details":{"reasoning_tokens":reasoning_tokens}
        },
        "incomplete_details":incomplete_details,
        "parallel_tool_calls":true,
        "tool_choice":"auto",
        "tools":[],
        "error":null
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
