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

pub(crate) fn stream(body: &Value, protocol: ApiProtocol, model: &str) -> Result<Vec<u8>> {
    let mut output = String::new();
    match protocol {
        ApiProtocol::Chat => chat_stream(body, model, &mut output),
        ApiProtocol::Anthropic => anthropic_stream(body, model, &mut output),
        ApiProtocol::Responses => bail!("Responses streaming is not supported by this translation"),
    }
    Ok(output.into_bytes())
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

fn chat_stream(body: &Value, model: &str, output: &mut String) {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl_magpie");
    let created = body.get("created").cloned().unwrap_or_else(|| json!(0));
    let start = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]});
    sse(output, None, &start);
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .unwrap_or(&Value::Null);
    let message = choice.get("message").unwrap_or(&Value::Null);
    if let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        let chunk = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
        sse(output, None, &chunk);
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        let fragments = calls.iter().enumerate().map(|(index, call)| json!({
            "index":index,
            "id":call.get("id").cloned().unwrap_or(Value::Null),
            "type":"function",
            "function":{
                "name":call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                "arguments":call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}")
            }
        })).collect::<Vec<_>>();
        let chunk = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{"tool_calls":fragments},"finish_reason":null}]});
        sse(output, None, &chunk);
    }
    let finish = body
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let final_chunk = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":finish}]});
    sse(output, None, &final_chunk);
    if let Some(usage) = body.get("usage") {
        let chunk = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[],"usage":usage});
        sse(output, None, &chunk);
    }
    output.push_str("data: [DONE]\n\n");
}

fn anthropic_stream(body: &Value, model: &str, output: &mut String) {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_magpie");
    let usage = body.get("usage").unwrap_or(&Value::Null);
    let start = json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":usage.get("input_tokens").cloned().unwrap_or_else(|| json!(0)),"output_tokens":0}}});
    sse(output, Some("message_start"), &start);
    for (index, block) in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let start = json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}});
                sse(output, Some("content_block_start"), &start);
                let delta = json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block.get("text").and_then(Value::as_str).unwrap_or("")}});
                sse(output, Some("content_block_delta"), &delta);
            }
            "tool_use" => {
                let start = json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":block.get("id").cloned().unwrap_or(Value::Null),"name":block.get("name").cloned().unwrap_or(Value::Null),"input":{}}});
                sse(output, Some("content_block_start"), &start);
                let delta = json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":serde_json::to_string(block.get("input").unwrap_or(&Value::Null)).unwrap_or_else(|_| "{}".to_owned())}});
                sse(output, Some("content_block_delta"), &delta);
            }
            _ => continue,
        }
        let stop = json!({"type":"content_block_stop","index":index});
        sse(output, Some("content_block_stop"), &stop);
    }
    let delta = json!({"type":"message_delta","delta":{"stop_reason":body.get("stop_reason").cloned().unwrap_or_else(|| json!("end_turn")),"stop_sequence":null},"usage":{"output_tokens":usage.get("output_tokens").cloned().unwrap_or_else(|| json!(0))}});
    sse(output, Some("message_delta"), &delta);
    sse(
        output,
        Some("message_stop"),
        &json!({"type":"message_stop"}),
    );
}

fn sse(output: &mut String, event: Option<&str>, data: &Value) {
    if let Some(event) = event {
        output.push_str("event: ");
        output.push_str(event);
        output.push('\n');
    }
    output.push_str("data: ");
    output.push_str(&data.to_string());
    output.push_str("\n\n");
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
            stream(&chat, ApiProtocol::Chat, "provider/model").expect("encode chat stream"),
        )
        .expect("valid UTF-8 SSE");
        assert!(events.contains("data: [DONE]\n\n"));
        assert!(events.contains("hello"));

        let anthropic = json!({"id":"msg_1","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}});
        let events = String::from_utf8(
            stream(&anthropic, ApiProtocol::Anthropic, "provider/model")
                .expect("encode Anthropic stream"),
        )
        .expect("valid UTF-8 SSE");
        assert!(events.contains("event: message_start"));
        assert!(events.contains("event: message_stop"));
    }
}
