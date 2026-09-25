use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::Url;

use super::{Provider, find, key_id, refresh_models};

#[derive(Clone, Copy)]
struct ProbeProtocol<'a> {
    name: &'static str,
    base: &'a str,
    suffix: &'static str,
}

struct ProbeResult {
    protocol: &'static str,
    ok: bool,
    status: Option<StatusCode>,
    millis: u128,
    model: String,
    error: String,
}

pub(super) async fn test_provider(id: &str) -> Result<()> {
    let provider = find(id)?;
    let _ = tokio::time::timeout(Duration::from_secs(8), refresh_models(&provider, false)).await;

    let probes = [
        ProbeProtocol {
            name: "chat",
            base: &provider.chat,
            suffix: "/chat/completions",
        },
        ProbeProtocol {
            name: "responses",
            base: &provider.responses,
            suffix: "/responses",
        },
        ProbeProtocol {
            name: "anthropic",
            base: &provider.anthropic,
            suffix: "/v1/messages",
        },
    ]
    .into_iter()
    .filter(|probe| !probe.base.is_empty())
    .collect::<Vec<_>>();
    ensure!(
        !probes.is_empty(),
        "{} has no API endpoint to test",
        provider.name
    );

    let client = Client::builder()
        .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .context("create provider test HTTP client")?;
    let results = futures_util::future::join_all(
        probes
            .into_iter()
            .map(|probe| probe_provider(&client, &provider, probe)),
    )
    .await;

    let mut failed = false;
    for result in results {
        let mark = if result.ok { "✓" } else { "✗" };
        let detail = if result.ok {
            format!("{} ms · {}", result.millis, result.model)
        } else {
            let error = result.status.map_or_else(
                || result.error.clone(),
                |status| format!("{} · {}", status.as_u16(), result.error),
            );
            failed = true;
            error
        };
        println!("  {mark} {:10} {detail}", result.protocol);
    }

    if failed {
        bail!("one or more provider API checks failed");
    }
    Ok(())
}

async fn probe_provider(
    client: &Client,
    provider: &Provider,
    probe: ProbeProtocol<'_>,
) -> ProbeResult {
    let key = match key_for_protocol(provider, probe.name) {
        Some(key) => key,
        None => {
            return ProbeResult {
                protocol: probe.name,
                ok: false,
                status: None,
                millis: 0,
                model: String::new(),
                error: "no key is on for this endpoint".to_owned(),
            };
        }
    };
    let model = test_model(provider, probe.name, &key);
    if model.is_empty() {
        return ProbeResult {
            protocol: probe.name,
            ok: false,
            status: None,
            millis: 0,
            model,
            error: "no model to try: expose one, or refresh the model list".to_owned(),
        };
    }

    let url = match endpoint_url(probe.base, probe.suffix) {
        Ok(url) => url,
        Err(error) => {
            return ProbeResult {
                protocol: probe.name,
                ok: false,
                status: None,
                millis: 0,
                model,
                error: format!("{error:#}"),
            };
        }
    };
    let body = match probe.name {
        "chat" => serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16
        }),
        "responses" => serde_json::json!({
            "model": model,
            "input": "hi",
            "max_output_tokens": 16
        }),
        "anthropic" => serde_json::json!({
            "model": model,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        _ => unreachable!("probe protocol comes from the fixed endpoint list"),
    };

    let body = match serde_json::to_vec(&body) {
        Ok(body) => body,
        Err(error) => {
            return ProbeResult {
                protocol: probe.name,
                ok: false,
                status: None,
                millis: 0,
                model,
                error: error.to_string(),
            };
        }
    };
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .body(body);
    if !key.is_empty() {
        if probe.name == "anthropic" {
            request = request.header("x-api-key", key.as_str());
            if !Url::parse(probe.base)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| host == "anthropic.com" || host.ends_with(".anthropic.com"))
            {
                request = request.bearer_auth(&key);
            }
        } else {
            request = request.bearer_auth(&key);
        }
    }
    if probe.name == "anthropic" {
        request = request.header("anthropic-version", "2023-06-01");
    }
    if let Some(host) = Url::parse(probe.base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        && (host == "opencode.ai" || host.ends_with(".opencode.ai"))
    {
        let mut nonce = [0_u8; 16];
        let session = if getrandom::fill(&mut nonce).is_ok() {
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join("")
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_string()
        };
        request = request.header("x-opencode-session", format!("magpie-test-{session}"));
    }
    for (name, value) in &provider.headers {
        request = request.header(name.as_str(), value.as_str());
    }

    let start = std::time::Instant::now();
    match request.send().await {
        Ok(mut response) => {
            let millis = start.elapsed().as_millis();
            let status = response.status();
            if status.is_success() {
                return ProbeResult {
                    protocol: probe.name,
                    ok: true,
                    status: Some(status),
                    millis,
                    model,
                    error: String::new(),
                };
            }
            let mut bytes = Vec::with_capacity(4096);
            while bytes.len() < 4096 {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let remaining = 4096 - bytes.len();
                        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            ProbeResult {
                protocol: probe.name,
                ok: false,
                status: Some(status),
                millis,
                model,
                error: api_error(&bytes, status),
            }
        }
        Err(error) => ProbeResult {
            protocol: probe.name,
            ok: false,
            status: None,
            millis: start.elapsed().as_millis(),
            model,
            error: error.without_url().to_string(),
        },
    }
}

fn key_for_protocol(provider: &Provider, protocol: &str) -> Option<String> {
    let mut keys = Vec::with_capacity(provider.keys.len() + 1);
    if !provider.key.is_empty() {
        keys.push((provider.key.clone(), provider.key_protocol.clone()));
    }
    keys.extend(
        provider
            .keys
            .iter()
            .filter(|key| !key.off && !key.key.is_empty())
            .map(|key| (key.key.clone(), key.protocol.clone())),
    );

    if keys.is_empty() {
        return provider.is_local().then(String::new);
    }

    keys.iter()
        .find(|(_, key_protocol)| key_protocol.as_str() == protocol)
        .or_else(|| {
            keys.iter()
                .find(|(_, key_protocol)| key_protocol.is_empty())
        })
        .map(|(key, _)| key.clone())
}

fn test_model(provider: &Provider, protocol: &str, key: &str) -> String {
    let key_id = (!key.is_empty()).then(|| key_id(key));
    let mut models =
        crate::catalog::exposed_models(&provider.id, provider.catalog_id(), &provider.models);
    let available = crate::catalog::available_models(&provider.id, provider.catalog_id());
    for model in available {
        if !models.iter().any(|existing| existing.id == model.id) {
            models.push(model);
        }
    }

    let supports_endpoint = |model: &crate::catalog::Model| {
        key_id
            .as_ref()
            .is_none_or(|key_id| model.keys.is_empty() || model.keys.contains(key_id))
            && (protocol != "anthropic" || is_claude_model(&model.id))
    };
    models
        .iter()
        .find(|model| supports_endpoint(model))
        .or_else(|| {
            models.iter().find(|model| {
                key_id
                    .as_ref()
                    .is_none_or(|key_id| model.keys.is_empty() || model.keys.contains(key_id))
            })
        })
        .map_or_else(String::new, |model| model.id.clone())
}

fn is_claude_model(id: &str) -> bool {
    id.rsplit('/')
        .next()
        .is_some_and(|id| id.to_ascii_lowercase().starts_with("claude"))
}

fn endpoint_url(base: &str, suffix: &str) -> Result<Url> {
    let mut url = Url::parse(base).context("parse provider URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "provider URL must use http:// or https://"
    );
    url.set_path(&format!("{}{suffix}", url.path().trim_end_matches('/')));
    url.set_fragment(None);
    Ok(url)
}

fn api_error(bytes: &[u8], status: StatusCode) -> String {
    let value = serde_json::from_slice::<Value>(bytes).unwrap_or(Value::Null);
    let nested = value.get("error");
    let message = nested
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| nested.and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str));
    if let Some(message) = message.filter(|message| !message.is_empty()) {
        return message.to_owned();
    }
    let body = String::from_utf8_lossy(bytes).trim().to_owned();
    if !body.is_empty() && body.len() < 200 && !body.starts_with('<') {
        format!("{status}: {body}")
    } else {
        status.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_test_urls_keep_base_paths_and_queries() {
        let url = endpoint_url("https://api.example/v1/?tenant=team", "/chat/completions")
            .expect("provider URL should be valid");
        assert_eq!(
            url.as_str(),
            "https://api.example/v1/chat/completions?tenant=team"
        );
    }

    #[test]
    fn provider_test_errors_extract_api_messages() {
        assert_eq!(
            api_error(
                br#"{"error":{"message":"invalid key"}}"#,
                StatusCode::UNAUTHORIZED
            ),
            "invalid key"
        );
        assert_eq!(
            api_error(b"quota exceeded", StatusCode::TOO_MANY_REQUESTS),
            "429 Too Many Requests: quota exceeded"
        );
    }
}
