use std::{env, fs, sync::Arc};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
    task::JoinSet,
};

const MAX_REQUEST_BYTES: usize = 16 << 20;
const MAX_RESPONSE_BYTES: usize = 32 << 20;

#[derive(Deserialize)]
struct RpcRequest {
    id: Option<Value>,
    #[serde(default)]
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
struct CallbackResponse {
    #[serde(default)]
    content: Value,
    #[serde(default, rename = "is_error")]
    is_error: bool,
}

pub async fn run(args: &[String]) -> Result<()> {
    let args = if args.is_empty() {
        let callback = env::var("MAGPIE_MCP_CALLBACK").unwrap_or_default();
        if callback.is_empty() {
            Vec::new()
        } else {
            vec![callback, env::var("MAGPIE_MCP_TOOLS").unwrap_or_default()]
        }
    } else {
        args.to_vec()
    };
    let [callback_url, tools_path] = args.as_slice() else {
        bail!("claude MCP helper expects callback URL and tools file");
    };

    let bytes = fs::read(tools_path).with_context(|| format!("read {tools_path}"))?;
    let tools: Arc<[Value]> =
        Arc::from(serde_json::from_slice::<Vec<Value>>(&bytes).context("read MCP tools")?);
    let callback_url: Arc<str> = Arc::from(callback_url.as_str());
    let client = reqwest::Client::new();
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut input = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let mut tasks = JoinSet::new();

    loop {
        line.clear();
        let bytes_read = input.read_line(&mut line).await?;
        if bytes_read == 0 {
            break;
        }
        if bytes_read > MAX_REQUEST_BYTES {
            bail!("MCP request exceeds 16 MiB");
        }

        let Ok(request) = serde_json::from_str::<RpcRequest>(&line) else {
            continue;
        };
        let Some(id) = request.id.clone() else {
            continue;
        };

        let callback_url = Arc::clone(&callback_url);
        let tools = Arc::clone(&tools);
        let client = client.clone();
        let output = Arc::clone(&output);
        tasks.spawn(async move {
            let response = handle(request, id, &callback_url, &tools, &client).await;
            if let Ok(mut bytes) = serde_json::to_vec(&response) {
                bytes.push(b'\n');
                let mut output = output.lock().await;
                let _ = output.write_all(&bytes).await;
                let _ = output.flush().await;
            }
        });
    }

    while let Some(result) = tasks.join_next().await {
        result.context("MCP request task failed")?;
    }
    Ok(())
}

async fn handle(
    request: RpcRequest,
    id: Value,
    callback_url: &str,
    tools: &[Value],
    client: &reqwest::Client,
) -> Value {
    let RpcRequest { method, params, .. } = request;
    let result = match method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "magpie", "version": "1"}
        })),
        "tools/list" => Ok(json!({"tools": tools})),
        "tools/call" => call_tool(params, callback_url, client).await,
        _ => Err((-32601, "method not found".to_owned())),
    };

    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }),
    }
}

async fn call_tool(
    params: Value,
    callback_url: &str,
    client: &reqwest::Client,
) -> std::result::Result<Value, (i32, String)> {
    if !params.is_null() && params.as_object().is_none() {
        return Err((-32602, "invalid tools/call params".to_owned()));
    }
    let object = params.as_object();
    let name = object
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = object
        .and_then(|params| params.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    let call_id = object
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("claudecode/toolUseId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(new_call_id);
    let payload = json!({
        "tool_call_id": call_id,
        "name": name,
        "arguments": arguments
    });

    let response = client
        .post(callback_url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| (-32000, error.to_string()))?;
    let status = response.status();
    let body = read_limited(response)
        .await
        .map_err(|error| (-32000, error))?;
    if status != reqwest::StatusCode::OK {
        return Err((-32000, String::from_utf8_lossy(&body).into_owned()));
    }

    let callback: CallbackResponse =
        serde_json::from_slice(&body).map_err(|error| (-32000, error.to_string()))?;
    Ok(json!({"content": callback.content, "isError": callback.is_error}))
}

async fn read_limited(response: reqwest::Response) -> std::result::Result<Vec<u8>, String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("MCP callback response exceeds 32 MiB".to_owned());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn new_call_id() -> String {
    let mut bytes = [0; 12];
    let _ = getrandom::fill(&mut bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut suffix = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        suffix.push(HEX[usize::from(byte >> 4)] as char);
        suffix.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    format!("call_{suffix}")
}
