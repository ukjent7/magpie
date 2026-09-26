use std::collections::BTreeMap;

use serde_json::{Value, json};

const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

pub(crate) fn to_chat_request(body: &Value, model: &str, stream: bool) -> Value {
    let mut messages = Vec::new();
    let system = text_content(body.get("systemInstruction"));
    if !system.is_empty() {
        messages.push(json!({"role":"system","content":system}));
    }

    let mut function_ids = BTreeMap::new();
    for (content_index, content) in body
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let role = if matches!(
            content.get("role").and_then(Value::as_str),
            Some("model" | "assistant")
        ) {
            "assistant"
        } else {
            "user"
        };
        let mut parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();

        for (part_index, part) in content
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(call) = part.get("functionCall") {
                let Some(name) = call.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_gemini_{content_index}_{part_index}"));
                function_ids.insert(name.to_owned(), id.clone());
                let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}
                }));
            } else if let Some(result) = part.get("functionResponse") {
                let Some(name) = result.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let id = result
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| function_ids.get(name).cloned())
                    .unwrap_or_else(|| format!("call_gemini_{name}"));
                let response = result.get("response").cloned().unwrap_or(Value::Null);
                let is_error = response.get("error").is_some();
                let output = response
                    .get(if is_error { "error" } else { "output" })
                    .cloned()
                    .unwrap_or(response);
                let output = output
                    .as_str()
                    .map_or_else(|| output.to_string(), str::to_owned);
                tool_results.push(json!({
                    "role":"tool",
                    "tool_call_id":id,
                    "content":output
                }));
            } else if let Some(text) = part
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                parts.push(json!({"type":"text","text":text}));
            } else if let Some(data) = part.get("inlineData") {
                if let (Some(mime), Some(encoded)) = (
                    data.get("mimeType").and_then(Value::as_str),
                    data.get("data").and_then(Value::as_str),
                ) {
                    if mime.starts_with("image/") {
                        parts.push(json!({
                            "type":"image_url",
                            "image_url":{"url":format!("data:{mime};base64,{encoded}")}
                        }));
                    } else {
                        parts.push(json!({"type":"text","text":format!("[attachment {mime}]")}));
                    }
                }
            } else if let Some(data) = part.get("fileData")
                && let Some(uri) = data.get("fileUri").and_then(Value::as_str)
            {
                parts.push(json!({"type":"image_url","image_url":{"url":uri}}));
            }
        }

        if !parts.is_empty() || !tool_calls.is_empty() {
            let content = if parts.is_empty() {
                Value::Null
            } else if parts
                .iter()
                .all(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            {
                Value::String(
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
            } else {
                Value::Array(parts)
            };
            let mut message = json!({"role":role,"content":content});
            if !tool_calls.is_empty() {
                message["tool_calls"] = Value::Array(tool_calls);
            }
            messages.push(message);
        }
        messages.extend(tool_results);
    }

    let mut request = json!({"model":model,"stream":stream,"messages":messages});
    if let Some(config) = body.get("generationConfig") {
        copy_number(config, "maxOutputTokens", &mut request, "max_tokens");
        copy_number(config, "temperature", &mut request, "temperature");
        copy_number(config, "topP", &mut request, "top_p");
        if let Some(stop) = config.get("stopSequences") {
            request["stop"] = stop.clone();
        }
        if let Some(thinking) = config.get("thinkingConfig") {
            let effort = thinking
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .or_else(|| {
                    thinking
                        .get("thinkingBudget")
                        .and_then(Value::as_i64)
                        .map(|budget| {
                            match budget {
                                value if value < 0 => "medium",
                                0..=1024 => "low",
                                1025..=8192 => "medium",
                                _ => "high",
                            }
                            .to_owned()
                        })
                });
            if let Some(effort) = effort {
                request["reasoning_effort"] = Value::String(effort);
            }
        }
        if config
            .get("responseMimeType")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("application/json"))
        {
            let schema = config
                .get("responseJsonSchema")
                .or_else(|| config.get("responseSchema"));
            let mut instruction = String::from("Respond with one JSON value and nothing else.");
            if let Some(schema) = schema {
                instruction.push_str(" Match this JSON schema:\n");
                instruction.push_str(&schema.to_string());
            }
            let system_message = json!({"role":"system","content":instruction});
            if let Some(messages) = request["messages"].as_array_mut() {
                messages.insert(0, system_message);
            }
        }
    }

    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|tool| {
            tool.get("functionDeclarations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|function| {
            let name = function.get("name")?.as_str()?;
            let mut definition = json!({"name":name});
            copy_string(function, "description", &mut definition);
            let schema = function
                .get("parametersJsonSchema")
                .or_else(|| function.get("parameters"));
            if let Some(schema) = schema {
                definition["parameters"] = normalize_schema(schema);
            }
            Some(json!({"type":"function","function":definition}))
        })
        .collect::<Vec<_>>();
    if !tools.is_empty() {
        request["tools"] = Value::Array(tools);
    }

    if let Some(config) = body.pointer("/toolConfig/functionCallingConfig") {
        let mode = config
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("AUTO")
            .to_ascii_uppercase();
        request["tool_choice"] = match mode.as_str() {
            "NONE" => json!("none"),
            "ANY" => {
                let names = config
                    .get("allowedFunctionNames")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let [name] = names.as_slice() {
                    json!({"type":"function","function":{"name":name}})
                } else {
                    json!("required")
                }
            }
            _ => json!("auto"),
        };
    }
    request
}

pub(crate) fn from_chat_response(body: &Value, model: &str) -> Value {
    let candidates = body
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, choice)| {
            let message = choice.get("message").unwrap_or(&Value::Null);
            let mut parts = text_parts(message.get("content"));
            if let Some(reasoning) = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                parts.insert(0, json!({"text":reasoning,"thought":true}));
            }
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let function = call.get("function").unwrap_or(&Value::Null);
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                    .unwrap_or_else(|| json!({}));
                parts.push(json!({
                    "functionCall": {
                        "id": call.get("id").cloned().unwrap_or(Value::Null),
                        "name": name,
                        "args": args
                    }
                }));
            }
            json!({
                "content":{"role":"model","parts":parts},
                "finishReason":finish_reason(choice.pointer("/finish_reason").and_then(Value::as_str)),
                "index":choice.get("index").and_then(Value::as_u64).unwrap_or(index as u64)
            })
        })
        .collect::<Vec<_>>();
    json!({
        "candidates":candidates,
        "usageMetadata":usage_metadata(body.get("usage")),
        "modelVersion":body.get("model").and_then(Value::as_str).unwrap_or(model),
        "responseId":body.get("id").and_then(Value::as_str).unwrap_or("magpie")
    })
}

pub(crate) fn estimate_tokens(body: &Value) -> u64 {
    let text = body.to_string();
    text.chars().count().div_ceil(4) as u64
}

pub(crate) fn single_event(body: &Value) -> Vec<u8> {
    let encoded = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_owned());
    format!("data: {encoded}\n\n").into_bytes()
}

fn copy_number(source: &Value, from: &str, target: &mut Value, to: &str) {
    if let Some(value) = source.get(from).filter(|value| value.is_number()) {
        target[to] = value.clone();
    }
}

fn copy_string(source: &Value, from: &str, target: &mut Value) {
    if let Some(value) = source.get(from).and_then(Value::as_str) {
        target[from] = Value::String(value.to_owned());
    }
}

fn text_content(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    let parts = content
        .get("parts")
        .or_else(|| content.as_array().map(|_| content));
    parts
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn text_parts(content: Option<&Value>) -> Vec<Value> {
    let text = text_content(content);
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"text":text})]
    }
}

fn normalize_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => {
            let mut normalized = serde_json::Map::new();
            for (key, value) in object {
                match key.as_str() {
                    "nullable" | "propertyOrdering" | "example" => {}
                    "type" => {
                        let lower = |value: &Value| {
                            value
                                .as_str()
                                .map(|text| Value::String(text.to_ascii_lowercase()))
                                .unwrap_or_else(|| normalize_schema(value))
                        };
                        normalized.insert(
                            key.clone(),
                            value
                                .as_array()
                                .map(|values| Value::Array(values.iter().map(lower).collect()))
                                .unwrap_or_else(|| lower(value)),
                        );
                    }
                    _ => {
                        normalized.insert(key.clone(), normalize_schema(value));
                    }
                }
            }
            Value::Object(normalized)
        }
        Value::Array(values) => Value::Array(values.iter().map(normalize_schema).collect()),
        _ => schema.clone(),
    }
}

fn usage_metadata(usage: Option<&Value>) -> Value {
    let prompt = usage
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output = usage
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cache_read = usage
        .and_then(|usage| usage.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let reasoning = usage
        .and_then(|usage| usage.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let mut metadata = json!({
        "promptTokenCount":prompt,
        "candidatesTokenCount":output.saturating_sub(reasoning),
        "totalTokenCount":prompt.saturating_add(output)
    });
    if cache_read > 0 {
        metadata["cachedContentTokenCount"] = json!(cache_read);
    }
    if reasoning > 0 {
        metadata["thoughtsTokenCount"] = json!(reasoning);
    }
    metadata
}

fn finish_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("length" | "max_tokens") => "MAX_TOKENS",
        Some("content_filter") => "SAFETY",
        _ => "STOP",
    }
}

#[derive(Default)]
struct ToolDelta {
    id: String,
    name: String,
    arguments: String,
}

pub(crate) struct StreamTranslator {
    model: String,
    id: String,
    pending: Vec<u8>,
    tools: BTreeMap<usize, ToolDelta>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    ended: bool,
}

impl StreamTranslator {
    pub(crate) fn new(model: String) -> Self {
        Self {
            model,
            id: String::new(),
            pending: Vec::new(),
            tools: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            ended: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        while let Some((end, separator)) = event_boundary(&self.pending) {
            let event = self.pending[..end].to_vec();
            self.pending.drain(..end + separator);
            output.extend(self.process_event(&event));
        }
        if self.pending.len() > MAX_SSE_EVENT_BYTES {
            self.pending.clear();
            self.ended = true;
            output.push(error_event("provider sent an oversized event"));
        }
        output
    }

    pub(crate) fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            let event = std::mem::take(&mut self.pending);
            output.extend(self.process_event(&event));
        }
        if !self.ended {
            output.extend(self.finish_response());
        }
        output
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ended
    }

    pub(crate) fn fail(&mut self, message: &str) -> Vec<u8> {
        self.ended = true;
        error_event(message)
    }

    fn process_event(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        if self.ended {
            return Vec::new();
        }
        let data = event
            .split(|byte| *byte == b'\n')
            .filter_map(|line| {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                line.strip_prefix(b"data:")
                    .map(|value| value.strip_prefix(b" ").unwrap_or(value))
            })
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            self.ended = true;
            return self.finish_response();
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return Vec::new();
        };
        if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
            self.ended = true;
            return vec![error_event(message)];
        }
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.id = id.to_owned();
        }
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            self.model = model.to_owned();
        }
        if let Some(usage) = value.get("usage") {
            self.usage = Some(usage.clone());
        }
        let mut output = Vec::new();
        if let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                output.push(candidate_event(
                    vec![json!({"text":text})],
                    None,
                    None,
                    &self.model,
                    &self.id,
                ));
            }
            if let Some(text) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                output.push(candidate_event(
                    vec![json!({"text":text,"thought":true})],
                    None,
                    None,
                    &self.model,
                    &self.id,
                ));
            }
            for tool in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = tool
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                let state = self.tools.entry(index).or_default();
                if let Some(id) = tool.get("id").and_then(Value::as_str) {
                    state.id = id.to_owned();
                }
                if let Some(function) = tool.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        state.name.push_str(name);
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        state.arguments.push_str(arguments);
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
        }
        output
    }

    fn finish_response(&mut self) -> Vec<Vec<u8>> {
        let mut parts = self
            .tools
            .values()
            .filter(|tool| !tool.name.is_empty())
            .map(|tool| {
                let args =
                    serde_json::from_str::<Value>(&tool.arguments).unwrap_or_else(|_| json!({}));
                json!({"functionCall":{"id":tool.id,"name":tool.name,"args":args}})
            })
            .collect::<Vec<_>>();
        let reason = finish_reason(self.finish_reason.as_deref());
        vec![candidate_event(
            std::mem::take(&mut parts),
            Some(reason),
            Some(usage_metadata(self.usage.as_ref())),
            &self.model,
            &self.id,
        )]
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

fn candidate_event(
    parts: Vec<Value>,
    finish: Option<&str>,
    usage: Option<Value>,
    model: &str,
    id: &str,
) -> Vec<u8> {
    let mut candidate = json!({
        "content":{"role":"model","parts":parts},
        "index":0
    });
    if let Some(finish) = finish {
        candidate["finishReason"] = json!(finish);
    }
    let mut event = json!({
        "candidates":[candidate],
        "modelVersion":model,
        "responseId":if id.is_empty() { "magpie" } else { id }
    });
    if let Some(usage) = usage {
        event["usageMetadata"] = usage;
    }
    let encoded = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned());
    format!("data: {encoded}\n\n").into_bytes()
}

fn error_event(message: &str) -> Vec<u8> {
    let encoded = json!({"error":{"code":502,"message":message,"status":"UNAVAILABLE"}});
    format!("data: {encoded}\n\n").into_bytes()
}
