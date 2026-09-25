use std::{
    collections::{HashSet, VecDeque},
    env,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde_json::{Value, json};
use url::Url;

use crate::provider::{self, GatewayProvider};

const DEFAULT_ADDR: &str = "127.0.0.1:3425";
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct GatewayState {
    client: Client,
}

pub async fn command(args: &[String]) -> Result<()> {
    let mut addr = env::var("MAGPIE_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("usage: magpie serve [--addr <host:port>]");
                return Ok(());
            }
            "--addr" | "-a" => {
                addr = args
                    .next()
                    .context("--addr requires a host and port")?
                    .clone();
            }
            _ if arg.starts_with("--addr=") => {
                addr = arg["--addr=".len()..].to_owned();
            }
            _ => bail!("unknown serve option {arg:?}; usage: magpie serve [--addr <host:port>]"),
        }
    }

    serve(&addr).await
}

async fn serve(addr: &str) -> Result<()> {
    let state = GatewayState {
        client: Client::builder()
            .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(UPSTREAM_TIMEOUT)
            .pool_max_idle_per_host(8)
            .build()
            .context("create gateway HTTP client")?,
    };
    let app = Router::new()
        .route("/", get(info))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .fallback(not_found)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind gateway to {addr}"))?;
    let address = listener.local_addr().context("read gateway address")?;
    println!("magpie gateway listening on http://{address}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve gateway")
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn configured_providers() -> std::result::Result<Vec<GatewayProvider>, ApiError> {
    match tokio::task::spawn_blocking(provider::gateway_providers).await {
        Ok(Ok(providers)) => Ok(providers),
        Ok(Err(error)) => {
            eprintln!("magpie: load provider configuration: {error:#}");
            Err(ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "could not read provider configuration",
            })
        }
        Err(_) => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "provider worker stopped",
        }),
    }
}

async fn info() -> std::result::Result<Json<Value>, ApiError> {
    let providers = configured_providers().await?;
    let count = exposed_models(&providers).count();
    Ok(Json(json!({
        "name": "magpie",
        "version": env!("CARGO_PKG_VERSION"),
        "models": count,
        "apis": [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1/models"
        ]
    })))
}

async fn models() -> std::result::Result<Json<Value>, ApiError> {
    let providers = configured_providers().await?;
    let data = exposed_models(&providers)
        .map(|(provider, model)| {
            json!({
                "id": format!("{}/{}", provider.id, model),
                "object": "model",
                "type": "model",
                "created": 0,
                "created_at": "2025-01-01T00:00:00Z",
                "owned_by": provider.id,
                "display_name": model
            })
        })
        .collect::<Vec<_>>();
    let first_id = data
        .first()
        .and_then(|model| model["id"].as_str())
        .map(str::to_owned);
    let last_id = data
        .last()
        .and_then(|model| model["id"].as_str())
        .map(str::to_owned);
    let mut result = json!({"object": "list", "data": data, "has_more": false});
    if let Some(id) = first_id {
        result["first_id"] = json!(id);
    }
    if let Some(id) = last_id {
        result["last_id"] = json!(id);
    }
    Ok(Json(result))
}

fn exposed_models(providers: &[GatewayProvider]) -> impl Iterator<Item = (&GatewayProvider, &str)> {
    providers
        .iter()
        .filter(|provider| !provider.hidden && has_endpoint(provider))
        .flat_map(|provider| {
            provider
                .models
                .iter()
                .map(move |model| (provider, model.as_str()))
        })
}

async fn chat_completions(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    forward(&state, request, ApiProtocol::Chat, None).await
}

async fn responses(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    forward(&state, request, ApiProtocol::Responses, None).await
}

async fn messages(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    forward(&state, request, ApiProtocol::Anthropic, None).await
}

async fn count_tokens(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    forward(
        &state,
        request,
        ApiProtocol::Anthropic,
        Some("/v1/messages/count_tokens"),
    )
    .await
}

async fn forward(
    state: &GatewayState,
    request: Request<Body>,
    protocol: ApiProtocol,
    path_override: Option<&str>,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return api_error_for(
                protocol,
                if error.to_string().contains("length limit") {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                },
                "could not read request body",
            );
        }
    };
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return api_error_for(
                protocol,
                StatusCode::BAD_REQUEST,
                "request body must be valid JSON",
            );
        }
    };
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return api_error_for(
            protocol,
            StatusCode::BAD_REQUEST,
            "request must include a model",
        );
    };
    let providers = match configured_providers().await {
        Ok(providers) => providers,
        Err(error) => return api_error_for(protocol, error.status, error.message),
    };
    let (primary, primary_model, primary_protocol) =
        match resolve_model(&model, &providers, protocol, path_override.is_none()) {
            Ok(result) => result,
            Err(ResolveError::Unknown) => {
                return api_error_for(
                    protocol,
                    StatusCode::NOT_FOUND,
                    &format!(
                        "unknown model {model:?}; use provider/model or list available models"
                    ),
                );
            }
            Err(ResolveError::Ambiguous) => {
                return api_error_for(
                    protocol,
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "model {model:?} belongs to more than one provider; use provider/model"
                    ),
                );
            }
        };
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut candidates = vec![(primary, primary_model, primary_protocol)];
    if path_override.is_none() {
        let mut seen = HashSet::from([(primary.id.clone(), primary_model.to_owned())]);
        for fallback in &primary.fallback {
            let Ok(target) = resolve_model(fallback, &providers, protocol, true) else {
                eprintln!(
                    "magpie: skip unavailable fallback {fallback:?} configured for {}",
                    primary.id
                );
                continue;
            };
            if seen.insert((target.0.id.clone(), target.1.to_owned())) {
                candidates.push(target);
            }
        }
    }

    let mut selected = None;
    for (index, (provider, upstream_model, upstream_protocol)) in candidates.iter().enumerate() {
        let is_last = index + 1 == candidates.len();
        let response = match send_upstream(
            state,
            provider,
            upstream_model,
            *upstream_protocol,
            protocol,
            &body,
            streaming,
            path_override,
            parts.uri.query(),
            &parts.headers,
        )
        .await
        {
            Ok(response) => response,
            Err(error) if !is_last => {
                eprintln!(
                    "magpie: provider {} failed before responding; trying its fallback: {error:#}",
                    provider.id
                );
                continue;
            }
            Err(error) => {
                eprintln!("magpie: upstream request to {} failed: {error:#}", provider.id);
                if error
                    .chain()
                    .any(|cause| cause.to_string() == "translate request for provider API")
                {
                    return api_error_for(
                        protocol,
                        StatusCode::BAD_REQUEST,
                        "request cannot be represented by the selected provider API",
                    );
                }
                return api_error_for(protocol, StatusCode::BAD_GATEWAY, "provider request failed");
            }
        };
        if !is_last && retryable_status(response.status()) {
            eprintln!(
                "magpie: provider {} returned {}; trying its fallback",
                provider.id,
                response.status()
            );
            continue;
        }
        selected = Some((response, *provider, *upstream_model, *upstream_protocol));
        break;
    }
    let Some((response, provider, upstream_model, upstream_protocol)) = selected else {
        return api_error_for(protocol, StatusCode::BAD_GATEWAY, "provider request failed");
    };
    let translated = upstream_protocol != protocol;
    if !translated || !response.status().is_success() {
        return relay(response);
    }
    let upstream_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media_type| media_type.trim() == "text/event-stream")
        });
    if streaming && upstream_sse {
        return translated_stream_response(response, upstream_protocol, protocol, &model);
    }

    let status = response.status();
    let upstream_headers = response.headers().clone();
    let mut response = response;
    let mut bytes = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                eprintln!(
                    "magpie: read translated response from {}: {error}",
                    provider.id
                );
                return api_error_for(
                    protocol,
                    StatusCode::BAD_GATEWAY,
                    "provider response failed",
                );
            }
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return api_error_for(
                protocol,
                StatusCode::BAD_GATEWAY,
                "provider response exceeds the gateway size limit",
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    let upstream_body: Value = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => {
            eprintln!(
                "magpie: parse translated response from {}: {error}",
                provider.id
            );
            return api_error_for(
                protocol,
                StatusCode::BAD_GATEWAY,
                "provider returned invalid JSON",
            );
        }
    };
    let translated_body = match crate::translation::response(
        &upstream_body,
        upstream_protocol,
        protocol,
        &model,
    ) {
        Ok(body) => body,
        Err(error) => {
            eprintln!(
                "magpie: translate response from {upstream_protocol:?} to {protocol:?}: {error:#}"
            );
            return api_error_for(
                protocol,
                StatusCode::BAD_GATEWAY,
                "provider response could not be translated",
            );
        }
    };
    let bytes = if streaming {
        match crate::translation::completed_stream(&translated_body, protocol, &model) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("magpie: encode translated stream for {protocol:?}: {error:#}");
                return api_error_for(
                    protocol,
                    StatusCode::BAD_GATEWAY,
                    "provider response could not be streamed",
                );
            }
        }
    } else {
        match serde_json::to_vec(&translated_body) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("magpie: serialize translated response: {error}");
                return api_error_for(
                    protocol,
                    StatusCode::BAD_GATEWAY,
                    "provider response could not be translated",
                );
            }
        }
    };
    translated_response(status, &upstream_headers, bytes, streaming)
}

async fn send_upstream(
    state: &GatewayState,
    provider: &GatewayProvider,
    model: &str,
    upstream_protocol: ApiProtocol,
    client_protocol: ApiProtocol,
    request: &Value,
    streaming: bool,
    path_override: Option<&str>,
    query: Option<&str>,
    incoming_headers: &HeaderMap,
) -> Result<reqwest::Response> {
    let translated = upstream_protocol != client_protocol;
    let mut body = if translated {
        crate::translation::request(request, client_protocol, upstream_protocol, model)
            .context("translate request for provider API")?
    } else {
        let mut body = request.clone();
        let object = body
            .as_object_mut()
            .context("request body must be a JSON object")?;
        object.insert("model".to_owned(), json!(model));
        body
    };
    if translated && streaming {
        body["stream"] = json!(true);
    }
    let body = serde_json::to_vec(&body).context("serialize upstream request")?;
    let url = upstream_url(
        upstream_protocol.base(provider),
        path_override.unwrap_or_else(|| upstream_protocol.path()),
        query,
    )
    .with_context(|| format!("build URL for provider {}", provider.id))?;
    let headers = upstream_headers(provider, upstream_protocol, incoming_headers)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("build headers for provider {}", provider.id))?;
    state
        .client
        .post(url)
        .headers(headers)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(body)
        .send()
        .await
        .with_context(|| format!("send request to provider {}", provider.id))
}

fn retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(status.as_u16(), 401 | 402 | 403 | 404 | 408 | 429)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApiProtocol {
    Chat,
    Responses,
    Anthropic,
}

impl ApiProtocol {
    fn base(self, provider: &GatewayProvider) -> &str {
        match self {
            Self::Chat => &provider.chat,
            Self::Responses => &provider.responses,
            Self::Anthropic => &provider.anthropic,
        }
    }

    const fn path(self) -> &'static str {
        match self {
            Self::Chat => "/chat/completions",
            Self::Responses => "/responses",
            Self::Anthropic => "/v1/messages",
        }
    }
}

fn has_endpoint(provider: &GatewayProvider) -> bool {
    !provider.chat.is_empty() || !provider.responses.is_empty() || !provider.anthropic.is_empty()
}

#[derive(Debug, Clone, Copy)]
enum ResolveError {
    Unknown,
    Ambiguous,
}

fn resolve_model<'a>(
    requested: &'a str,
    providers: &'a [GatewayProvider],
    protocol: ApiProtocol,
    allow_translation: bool,
) -> std::result::Result<(&'a GatewayProvider, &'a str, ApiProtocol), ResolveError> {
    if let Some((provider_id, model)) = requested.split_once('/') {
        if model.is_empty() {
            return Err(ResolveError::Unknown);
        }
        return providers
            .iter()
            .filter(|provider| {
                !provider.hidden
                    && (provider.id == provider_id
                        || provider.name.eq_ignore_ascii_case(provider_id))
            })
            .find_map(|provider| {
                endpoint_for(protocol, provider, allow_translation)
                    .map(|upstream| (provider, model, upstream))
            })
            .ok_or(ResolveError::Unknown);
    }

    let mut matches = providers.iter().filter_map(|provider| {
        if provider.hidden || !provider.models.iter().any(|model| model == requested) {
            return None;
        }
        endpoint_for(protocol, provider, allow_translation).map(|upstream| (provider, upstream))
    });
    let (provider, upstream) = matches.next().ok_or(ResolveError::Unknown)?;
    if matches.next().is_some() {
        return Err(ResolveError::Ambiguous);
    }
    Ok((provider, requested, upstream))
}

fn endpoint_for(
    protocol: ApiProtocol,
    provider: &GatewayProvider,
    allow_translation: bool,
) -> Option<ApiProtocol> {
    if !protocol.base(provider).is_empty() {
        return Some(protocol);
    }
    if !allow_translation {
        return None;
    }
    let alternative = match protocol {
        ApiProtocol::Chat => ApiProtocol::Anthropic,
        ApiProtocol::Anthropic => ApiProtocol::Chat,
        ApiProtocol::Responses => return None,
    };
    (!alternative.base(provider).is_empty()).then_some(alternative)
}

fn upstream_url(base: &str, path_suffix: &str, query: Option<&str>) -> Result<Url> {
    let mut url = Url::parse(base).context("parse provider URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("provider URL must use http:// or https://");
    }
    let path = format!("{}{path_suffix}", url.path().trim_end_matches('/'));
    url.set_path(&path);
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        let combined = match url.query() {
            Some(existing) => format!("{existing}&{query}"),
            None => query.to_owned(),
        };
        url.set_query(Some(&combined));
    }
    url.set_fragment(None);
    Ok(url)
}

fn upstream_headers(
    provider: &GatewayProvider,
    protocol: ApiProtocol,
    incoming: &HeaderMap,
) -> std::result::Result<HeaderMap, &'static str> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if !provider.key.is_empty() {
        let (name, raw_value) = match protocol {
            ApiProtocol::Anthropic => (HeaderName::from_static("x-api-key"), provider.key.clone()),
            ApiProtocol::Chat | ApiProtocol::Responses => {
                (header::AUTHORIZATION, format!("Bearer {}", provider.key))
            }
        };
        let value = HeaderValue::from_str(&raw_value)
            .map_err(|_| "provider API key cannot be used as an HTTP header")?;
        headers.insert(name, value);
    }
    if matches!(protocol, ApiProtocol::Anthropic) {
        let version = incoming
            .get("anthropic-version")
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("2023-06-01"));
        headers.insert("anthropic-version", version);
        if let Some(beta) = incoming.get("anthropic-beta") {
            headers.insert("anthropic-beta", beta.clone());
        }
    }
    for (name, value) in &provider.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "provider contains an invalid custom header name")?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| "provider contains an invalid custom header value")?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn relay(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;

    let connection_tokens = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect::<HashSet<_>>();
    for (name, value) in &headers {
        let name_text = name.as_str();
        if matches!(
            name_text,
            "connection"
                | "content-length"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) || connection_tokens.contains(name_text)
        {
            continue;
        }
        response.headers_mut().append(name.clone(), value.clone());
    }
    response
}

fn translated_stream_response(
    upstream: reqwest::Response,
    from: ApiProtocol,
    to: ApiProtocol,
    model: &str,
) -> Response {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let Some(translator) = crate::translation::SseTranslator::new(from, to, model) else {
        return api_error_for(
            to,
            StatusCode::BAD_GATEWAY,
            "streaming translation is not supported for this protocol pair",
        );
    };
    let body_stream = stream::unfold(
        (upstream.bytes_stream(), translator, VecDeque::new(), false),
        |(mut input, mut translator, mut pending, mut ended)| async move {
            loop {
                if let Some(frame) = pending.pop_front() {
                    return Some((
                        Ok::<_, reqwest::Error>(frame),
                        (input, translator, pending, ended),
                    ));
                }
                if ended {
                    return None;
                }
                match input.next().await {
                    Some(Ok(chunk)) => {
                        pending.extend(translator.push(&chunk));
                        ended = translator.is_ended();
                    }
                    Some(Err(error)) => {
                        ended = true;
                        return Some((Err(error), (input, translator, pending, ended)));
                    }
                    None => {
                        pending.extend(translator.finish());
                        ended = true;
                    }
                }
            }
        },
    );
    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = status;
    copy_translated_headers(&mut response, &upstream_headers);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

fn translated_response(
    status: StatusCode,
    upstream_headers: &HeaderMap,
    body: Vec<u8>,
    streaming: bool,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    copy_translated_headers(&mut response, upstream_headers);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        if streaming {
            HeaderValue::from_static("text/event-stream; charset=utf-8")
        } else {
            HeaderValue::from_static("application/json")
        },
    );
    if streaming {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        );
        response.headers_mut().insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
    }
    response
}

fn copy_translated_headers(response: &mut Response, upstream_headers: &HeaderMap) {
    let connection_tokens = upstream_headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect::<HashSet<_>>();
    for (name, value) in upstream_headers {
        let name_text = name.as_str();
        if matches!(
            name_text,
            "connection"
                | "content-encoding"
                | "content-length"
                | "content-type"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) || connection_tokens.contains(name_text)
        {
            continue;
        }
        response.headers_mut().append(name.clone(), value.clone());
    }
}

async fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "unknown gateway endpoint")
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": null
            }
        })),
    )
        .into_response()
}

fn api_error_for(protocol: ApiProtocol, status: StatusCode, message: &str) -> Response {
    let body = match protocol {
        ApiProtocol::Anthropic => json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": message
            }
        }),
        ApiProtocol::Chat | ApiProtocol::Responses => json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": null
            }
        }),
    };
    (status, Json(body)).into_response()
}

#[derive(Clone, Copy)]
struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        api_error(self.status, self.message)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn provider(id: &str, name: &str, models: &[&str]) -> GatewayProvider {
        GatewayProvider {
            id: id.to_owned(),
            name: name.to_owned(),
            key: String::new(),
            chat: "https://api.example/v1".to_owned(),
            responses: String::new(),
            anthropic: String::new(),
            fallback: Vec::new(),
            headers: BTreeMap::new(),
            models: models.iter().map(|model| (*model).to_owned()).collect(),
            hidden: false,
        }
    }

    #[test]
    fn appends_chat_path_and_preserves_query() {
        let url = upstream_url(
            "https://api.example/v1/",
            ApiProtocol::Chat.path(),
            Some("stream=true&n=2"),
        )
        .expect("provider URL should be valid");
        assert_eq!(
            url.as_str(),
            "https://api.example/v1/chat/completions?stream=true&n=2"
        );
    }

    #[test]
    fn uses_protocol_specific_upstream_paths() {
        let url = upstream_url("https://api.example", ApiProtocol::Anthropic.path(), None)
            .expect("provider URL should be valid");
        assert_eq!(url.as_str(), "https://api.example/v1/messages");
    }

    #[test]
    fn resolves_qualified_models_without_truncating_model_id() {
        let providers = [provider("relay", "Relay", &["visible-model"])];
        let (provider, model, upstream) =
            resolve_model("relay/vendor/model", &providers, ApiProtocol::Chat, true)
                .expect("qualified provider/model should resolve");
        assert_eq!(provider.id, "relay");
        assert_eq!(model, "vendor/model");
        assert_eq!(upstream, ApiProtocol::Chat);
    }

    #[test]
    fn rejects_ambiguous_bare_model_ids() {
        let providers = [
            provider("first", "First", &["shared-model"]),
            provider("second", "Second", &["shared-model"]),
        ];
        assert!(matches!(
            resolve_model("shared-model", &providers, ApiProtocol::Chat, true),
            Err(ResolveError::Ambiguous)
        ));
    }

    #[test]
    fn resolves_only_providers_that_speak_the_requested_protocol() {
        let mut provider = provider("relay", "Relay", &["model"]);
        provider.chat.clear();
        provider.responses = "https://api.example/v1".to_owned();
        let providers = [provider];

        assert!(matches!(
            resolve_model("relay/model", &providers, ApiProtocol::Chat, true),
            Err(ResolveError::Unknown)
        ));
        assert!(resolve_model("relay/model", &providers, ApiProtocol::Responses, true).is_ok());
    }

    #[test]
    fn resolves_a_translatable_endpoint_when_the_native_api_is_missing() {
        let mut provider = provider("relay", "Relay", &["model"]);
        provider.chat.clear();
        provider.anthropic = "https://api.example".to_owned();
        let providers = [provider];

        let (_, _, upstream) = resolve_model("relay/model", &providers, ApiProtocol::Chat, true)
            .expect("Chat requests can be translated to Anthropic");
        assert_eq!(upstream, ApiProtocol::Anthropic);
        assert!(matches!(
            resolve_model("relay/model", &providers, ApiProtocol::Chat, false),
            Err(ResolveError::Unknown)
        ));
    }

    #[test]
    fn retries_provider_failures_that_can_recover_elsewhere() {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::PAYMENT_REQUIRED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(retryable_status(status), "{status} should allow fallback");
        }
        assert!(!retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn custom_auth_header_overrides_the_default_bearer_key() {
        let mut provider = provider("relay", "Relay", &[]);
        provider.key = "stored-secret".to_owned();
        provider
            .headers
            .insert("authorization".to_owned(), "Token custom-scheme".to_owned());

        let headers = upstream_headers(&provider, ApiProtocol::Chat, &HeaderMap::new())
            .expect("headers should be valid");
        assert_eq!(headers[header::AUTHORIZATION], "Token custom-scheme");
    }

    #[test]
    fn anthropic_auth_uses_its_protocol_headers() {
        let mut provider = provider("relay", "Relay", &[]);
        provider.key = "secret".to_owned();
        let mut incoming = HeaderMap::new();
        incoming.insert("anthropic-version", HeaderValue::from_static("2024-01-01"));

        let headers = upstream_headers(&provider, ApiProtocol::Anthropic, &incoming)
            .expect("headers should be valid");
        assert_eq!(headers["x-api-key"], "secret");
        assert_eq!(headers["anthropic-version"], "2024-01-01");
        assert!(!headers.contains_key(header::AUTHORIZATION));
    }
}
