use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
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

use crate::provider::{self, GatewayCatalog, GatewayProvider};

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

async fn configured_catalog() -> std::result::Result<GatewayCatalog, ApiError> {
    match tokio::task::spawn_blocking(provider::gateway_catalog).await {
        Ok(Ok(catalog)) => Ok(catalog),
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
    let catalog = configured_catalog().await?;
    let count = exposed_models(&catalog.providers).count() + catalog.groups.len();
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
    let catalog = configured_catalog().await?;
    let mut data = exposed_models(&catalog.providers)
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
    data.extend(catalog.groups.iter().map(|group| {
        json!({
            "id": format!("group/{}", group.id),
            "object": "model",
            "type": "model",
            "created": 0,
            "created_at": "2025-01-01T00:00:00Z",
            "owned_by": "magpie",
            "display_name": group.name
        })
    }));
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
    let catalog = match configured_catalog().await {
        Ok(catalog) => catalog,
        Err(error) => return api_error_for(protocol, error.status, error.message),
    };
    let providers = &catalog.providers;
    let group = model
        .strip_prefix("group/")
        .and_then(|id| catalog.groups.iter().find(|group| group.id == id));
    let mut candidates = if let Some(group) = group {
        group_candidates(group, providers, protocol, path_override.is_none())
    } else if model.starts_with("group/") {
        Vec::new()
    } else {
        let target = match resolve_model(&model, providers, protocol, path_override.is_none()) {
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
        vec![target]
    };
    if candidates.is_empty() {
        return api_error_for(
            protocol,
            StatusCode::NOT_FOUND,
            &format!("unknown or unavailable model {model:?}; list available models"),
        );
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if path_override.is_none() {
        let mut seen = HashSet::new();
        let mut routed = Vec::new();
        for target in candidates {
            if seen.insert((target.0.id.clone(), target.1.to_owned())) {
                routed.push(target);
            }
            if group.is_some() {
                continue;
            }
            for fallback in &target.0.fallback {
                let Ok(fallback_target) = resolve_model(fallback, providers, protocol, true) else {
                    eprintln!(
                        "magpie: skip unavailable fallback {fallback:?} configured for {}",
                        target.0.id
                    );
                    continue;
                };
                if seen.insert((fallback_target.0.id.clone(), fallback_target.1.to_owned())) {
                    routed.push(fallback_target);
                }
            }
        }
        candidates = routed;
    }

    let mut candidates = candidates
        .into_iter()
        .flat_map(|(provider, model, upstream)| {
            key_candidates(
                provider,
                model,
                protocol,
                upstream,
                path_override.is_none(),
            )
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return api_error_for(
            protocol,
            StatusCode::NOT_FOUND,
            &format!("unknown or unavailable model {model:?}; list available models"),
        );
    }
    if group.is_some_and(|group| group.routing == "usage") {
        sort_by_recent_usage(&mut candidates);
        candidates.sort_by_key(|candidate| {
            (!candidate.model_listed, key_fit(*candidate, protocol))
        });
    }
    move_resting_routes_last(&mut candidates);

    let mut selected = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let is_last = index + 1 == candidates.len();
        let response = match send_upstream(
            state,
            UpstreamRequest {
                provider: candidate.provider,
                model: candidate.model,
                upstream_protocol: candidate.upstream,
                key: candidate.key,
                client_protocol: protocol,
                body: &body,
                streaming,
                path_override,
                query: parts.uri.query(),
                incoming_headers: &parts.headers,
            },
        )
        .await
        {
            Ok(response) => response,
            Err(error) if !is_last => {
                if !error
                    .chain()
                    .any(|cause| cause.to_string() == "translate request for provider API")
                {
                    rest_route(*candidate, ROUTE_COOLDOWN);
                }
                eprintln!(
                    "magpie: {} failed before responding; trying the next route: {error:#}",
                    candidate.label()
                );
                continue;
            }
            Err(error) => {
                if !error
                    .chain()
                    .any(|cause| cause.to_string() == "translate request for provider API")
                {
                    rest_route(*candidate, ROUTE_COOLDOWN);
                }
                eprintln!(
                    "magpie: upstream request to {} failed: {error:#}",
                    candidate.label()
                );
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
        if retryable_status(response.status()) {
            let retry_after = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map_or(ROUTE_COOLDOWN, Duration::from_secs);
            rest_route(*candidate, retry_after);
        } else {
            clear_route_rest(*candidate);
        }
        if !is_last && retryable_status(response.status()) {
            eprintln!(
                "magpie: {} returned {}; trying the next route",
                candidate.label(),
                response.status()
            );
            continue;
        }
        selected = Some((response, *candidate));
        break;
    }
    let Some((response, candidate)) = selected else {
        return api_error_for(protocol, StatusCode::BAD_GATEWAY, "provider request failed");
    };
    if response.status().is_success() {
        mark_route_used(candidate);
    }
    let provider = candidate.provider;
    let upstream_protocol = candidate.upstream;
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

struct UpstreamRequest<'a> {
    provider: &'a GatewayProvider,
    model: &'a str,
    upstream_protocol: ApiProtocol,
    key: Option<&'a str>,
    client_protocol: ApiProtocol,
    body: &'a Value,
    streaming: bool,
    path_override: Option<&'a str>,
    query: Option<&'a str>,
    incoming_headers: &'a HeaderMap,
}

async fn send_upstream(
    state: &GatewayState,
    request: UpstreamRequest<'_>,
) -> Result<reqwest::Response> {
    let UpstreamRequest {
        provider,
        model,
        upstream_protocol,
        key,
        client_protocol,
        body: request_body,
        streaming,
        path_override,
        query,
        incoming_headers,
    } = request;
    let translated = upstream_protocol != client_protocol;
    let mut body = if translated {
        crate::translation::request(request_body, client_protocol, upstream_protocol, model)
            .context("translate request for provider API")?
    } else {
        let mut body = request_body.clone();
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
    let headers = upstream_headers(provider, upstream_protocol, key, incoming_headers)
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
    status.is_server_error() || matches!(status.as_u16(), 401 | 402 | 403 | 404 | 408 | 429)
}

#[derive(Default)]
struct RoutingState {
    rotations: HashMap<String, usize>,
    usage: HashMap<String, (f64, Instant)>,
    resting: HashMap<String, Instant>,
}

static ROUTING_STATE: LazyLock<Mutex<RoutingState>> =
    LazyLock::new(|| Mutex::new(RoutingState::default()));

const USAGE_HALF_LIFE: Duration = Duration::from_secs(60 * 60);
const ROUTE_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_ROUTE_COOLDOWN: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy)]
struct RouteCandidate<'a> {
    provider: &'a GatewayProvider,
    model: &'a str,
    upstream: ApiProtocol,
    key: Option<&'a str>,
    key_protocol: Option<ApiProtocol>,
    model_listed: bool,
}

impl RouteCandidate<'_> {
    fn id(self) -> String {
        self.key.map_or_else(
            || self.provider.id.clone(),
            |key| format!("{}#{}", self.provider.id, provider::key_id(key)),
        )
    }

    fn label(self) -> String {
        self.key.map_or_else(
            || self.provider.id.clone(),
            |key| format!("{} ({})", self.provider.id, provider::key_id(key)),
        )
    }
}

fn key_candidates<'a>(
    provider: &'a GatewayProvider,
    model: &'a str,
    client_protocol: ApiProtocol,
    default_upstream: ApiProtocol,
    allow_translation: bool,
) -> Vec<RouteCandidate<'a>> {
    let configured_keys = provider
        .keys
        .iter()
        .filter(|key| key.active)
        .collect::<Vec<_>>();
    let unkeyed = !provider.has_configured_keys;
    if !unkeyed && configured_keys.is_empty() {
        return Vec::new();
    }

    let mut candidates = if unkeyed {
        vec![RouteCandidate {
            provider,
            model,
            upstream: default_upstream,
            key: None,
            key_protocol: None,
            model_listed: true,
        }]
    } else {
        configured_keys
            .into_iter()
            .filter_map(|key| {
                let bound_protocol = if key.protocol.is_empty() {
                    None
                } else {
                    Some(parse_api_protocol(&key.protocol)?)
                };
                let upstream = match bound_protocol {
                    None => default_upstream,
                    Some(bound) if bound == client_protocol => bound,
                    Some(bound)
                        if allow_translation && translation_supported(client_protocol, bound) =>
                    {
                        bound
                    }
                    Some(_) => return None,
                };
                (!upstream.base(provider).is_empty()).then_some(RouteCandidate {
                    provider,
                    model,
                    upstream,
                    key: Some(key.key.as_str()),
                    key_protocol: bound_protocol,
                    model_listed: true,
                })
            })
            .collect::<Vec<_>>()
    };

    let mut unlisted = Vec::new();
    if let Some(listed_keys) = provider.model_keys.get(model) {
        let (listed, mut others): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|candidate| {
            candidate
                .key
                .is_some_and(|key| listed_keys.contains(&provider::key_id(key)))
        });
        if listed.is_empty() {
            candidates = others;
        } else {
            candidates = listed;
            for candidate in &mut others {
                candidate.model_listed = false;
            }
            unlisted = others;
        }
    }

    match provider.routing.as_str() {
        "rotate" => rotate_candidates(&format!("provider/{}", provider.id), &mut candidates),
        "usage" => sort_by_recent_usage(&mut candidates),
        _ => {}
    }
    candidates.sort_by_key(|candidate| key_fit(*candidate, client_protocol));
    unlisted.sort_by_key(|candidate| key_fit(*candidate, client_protocol));
    candidates.extend(unlisted);
    candidates
}

fn parse_api_protocol(protocol: &str) -> Option<ApiProtocol> {
    match protocol {
        "chat" => Some(ApiProtocol::Chat),
        "responses" => Some(ApiProtocol::Responses),
        "anthropic" => Some(ApiProtocol::Anthropic),
        _ => None,
    }
}

fn translation_supported(from: ApiProtocol, to: ApiProtocol) -> bool {
    matches!(
        (from, to),
        (ApiProtocol::Chat, ApiProtocol::Anthropic)
            | (ApiProtocol::Anthropic, ApiProtocol::Chat)
    )
}

fn key_fit(candidate: RouteCandidate<'_>, client_protocol: ApiProtocol) -> u8 {
    let Some(key_protocol) = candidate.key_protocol else {
        return 0;
    };
    let model = candidate
        .model
        .rsplit('/')
        .next()
        .unwrap_or(candidate.model)
        .to_ascii_lowercase();
    if model.starts_with("claude") {
        return u8::from(key_protocol != ApiProtocol::Anthropic) * 2;
    }
    let o_series = model.starts_with('o')
        && model
            .as_bytes()
            .get(1)
            .is_some_and(|character| character.is_ascii_digit());
    if model.starts_with("gpt-") || model.contains("codex") || o_series {
        return u8::from(key_protocol == ApiProtocol::Anthropic) * 2;
    }
    u8::from(candidate.upstream != client_protocol)
}

fn rotate_candidates<T>(key: &str, candidates: &mut [T]) {
    if candidates.len() < 2 {
        return;
    }
    let mut state = ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let next = state.rotations.entry(key.to_owned()).or_default();
    let offset = *next % candidates.len();
    *next = (*next + 1) % candidates.len();
    candidates.rotate_left(offset);
}

fn usage_score(id: &str, now: Instant, state: &RoutingState) -> f64 {
    state.usage.get(id).map_or(0.0, |(requests, last_seen)| {
        let elapsed = now.saturating_duration_since(*last_seen).as_secs_f64();
        requests * 0.5_f64.powf(elapsed / USAGE_HALF_LIFE.as_secs_f64())
    })
}

fn sort_by_recent_usage(candidates: &mut [RouteCandidate<'_>]) {
    let now = Instant::now();
    let state = ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut weighted = candidates
        .iter()
        .copied()
        .map(|candidate| {
            let score = usage_score(&candidate.id(), now, &state);
            (candidate, score)
        })
        .collect::<Vec<_>>();
    weighted.sort_by(|left, right| left.1.total_cmp(&right.1));
    for (slot, (candidate, _)) in candidates.iter_mut().zip(weighted) {
        *slot = candidate;
    }
}

fn move_resting_routes_last(candidates: &mut [RouteCandidate<'_>]) {
    let now = Instant::now();
    let mut state = ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.resting.retain(|_, until| *until > now);
    let mut ordered = candidates
        .iter()
        .copied()
        .map(|candidate| {
            let resting = state
                .resting
                .get(&candidate.id())
                .is_some_and(|until| *until > now);
            (candidate, resting)
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(_, resting)| *resting);
    for (slot, (candidate, _)) in candidates.iter_mut().zip(ordered) {
        *slot = candidate;
    }
}

fn rest_route(candidate: RouteCandidate<'_>, duration: Duration) {
    let until = Instant::now() + duration.min(MAX_ROUTE_COOLDOWN);
    ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .resting
        .insert(candidate.id(), until);
}

fn clear_route_rest(candidate: RouteCandidate<'_>) {
    ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .resting
        .remove(&candidate.id());
}

fn mark_route_used(candidate: RouteCandidate<'_>) {
    let now = Instant::now();
    let id = candidate.id();
    let mut state = ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let current = usage_score(&id, now, &state) + 1.0;
    state.usage.insert(id, (current, now));
}

fn rotate_group_candidates<'a>(
    group_id: &str,
    candidates: &mut [(&'a GatewayProvider, &'a str, ApiProtocol)],
) {
    if candidates.len() < 2 {
        return;
    }
    rotate_candidates(&format!("group/{group_id}"), candidates);
}

fn group_candidates<'a>(
    group: &'a provider::GatewayGroup,
    providers: &'a [GatewayProvider],
    protocol: ApiProtocol,
    allow_translation: bool,
) -> Vec<(&'a GatewayProvider, &'a str, ApiProtocol)> {
    let mut seen = HashSet::new();
    let mut candidates = group
        .members
        .iter()
        .filter_map(|member| resolve_model(member, providers, protocol, allow_translation).ok())
        .filter(|(provider, model, _)| seen.insert((provider.id.as_str(), *model)))
        .collect::<Vec<_>>();
    if group.routing == "rotate" {
        rotate_group_candidates(&group.id, &mut candidates);
    }
    candidates
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
    key: Option<&str>,
    incoming: &HeaderMap,
) -> std::result::Result<HeaderMap, &'static str> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        let (name, raw_value) = match protocol {
            ApiProtocol::Anthropic => (HeaderName::from_static("x-api-key"), key.to_owned()),
            ApiProtocol::Chat | ApiProtocol::Responses => {
                (header::AUTHORIZATION, format!("Bearer {key}"))
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
            chat: "https://api.example/v1".to_owned(),
            responses: String::new(),
            anthropic: String::new(),
            fallback: Vec::new(),
            routing: String::new(),
            keys: Vec::new(),
            has_configured_keys: false,
            headers: BTreeMap::new(),
            models: models.iter().map(|model| (*model).to_owned()).collect(),
            model_keys: HashMap::new(),
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
    fn group_rotation_advances_the_first_member_per_request() {
        let providers = [
            provider("first", "First", &["model"]),
            provider("second", "Second", &["model"]),
            provider("third", "Third", &["model"]),
        ];
        let targets = || {
            providers
                .iter()
                .map(|provider| (provider, "model", ApiProtocol::Chat))
                .collect::<Vec<_>>()
        };
        let mut first = targets();
        rotate_group_candidates("rotation-test-group", &mut first);
        assert_eq!(first[0].0.id, "first");

        let mut second = targets();
        rotate_group_candidates("rotation-test-group", &mut second);
        assert_eq!(second[0].0.id, "second");
        assert_eq!(second[1].0.id, "third");
        assert_eq!(second[2].0.id, "first");
    }

    #[test]
    fn group_candidates_keep_configured_order_and_skip_unavailable_duplicates() {
        let providers = [
            provider("first", "First", &["model"]),
            provider("second", "Second", &["model"]),
        ];
        let group = provider::GatewayGroup {
            id: "ordered-test-group".to_owned(),
            name: "Ordered".to_owned(),
            members: vec![
                "second/model".to_owned(),
                "missing/model".to_owned(),
                "first/model".to_owned(),
                "second/model".to_owned(),
            ],
            routing: "order".to_owned(),
        };

        let candidates = group_candidates(&group, &providers, ApiProtocol::Chat, true);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].0.id, "second");
        assert_eq!(candidates[1].0.id, "first");
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
        provider
            .headers
            .insert("authorization".to_owned(), "Token custom-scheme".to_owned());

        let headers = upstream_headers(
            &provider,
            ApiProtocol::Chat,
            Some("stored-secret"),
            &HeaderMap::new(),
        )
            .expect("headers should be valid");
        assert_eq!(headers[header::AUTHORIZATION], "Token custom-scheme");
    }

    #[test]
    fn anthropic_auth_uses_its_protocol_headers() {
        let mut provider = provider("relay", "Relay", &[]);
        let mut incoming = HeaderMap::new();
        incoming.insert("anthropic-version", HeaderValue::from_static("2024-01-01"));

        let headers = upstream_headers(
            &provider,
            ApiProtocol::Anthropic,
            Some("secret"),
            &incoming,
        )
        .expect("headers should be valid");
        assert_eq!(headers["x-api-key"], "secret");
        assert_eq!(headers["anthropic-version"], "2024-01-01");
        assert!(!headers.contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn key_candidates_use_the_protocol_each_key_was_created_for() {
        let mut relay = provider("protocol-key-test", "Relay", &["claude-opus", "gpt-5.5"]);
        relay.anthropic = "https://api.example".to_owned();
        relay.has_configured_keys = true;
        relay.keys = vec![
            provider::GatewayKey {
                key: "openai-key".to_owned(),
                protocol: "chat".to_owned(),
                active: true,
            },
            provider::GatewayKey {
                key: "anthropic-key".to_owned(),
                protocol: "anthropic".to_owned(),
                active: true,
            },
        ];

        let claude = key_candidates(
            &relay,
            "claude-opus",
            ApiProtocol::Chat,
            ApiProtocol::Chat,
            true,
        );
        assert_eq!(claude[0].key, Some("anthropic-key"));
        assert_eq!(claude[0].upstream, ApiProtocol::Anthropic);
        assert_eq!(claude[1].key, Some("openai-key"));
        assert_eq!(claude[1].upstream, ApiProtocol::Chat);

        let gpt = key_candidates(
            &relay,
            "gpt-5.5",
            ApiProtocol::Anthropic,
            ApiProtocol::Anthropic,
            true,
        );
        assert_eq!(gpt[0].key, Some("openai-key"));
        assert_eq!(gpt[0].upstream, ApiProtocol::Chat);
    }

    #[test]
    fn off_keys_are_skipped_and_keyless_providers_remain_usable() {
        let mut relay = provider("off-key-test", "Relay", &["model"]);
        relay.has_configured_keys = true;
        relay.keys.push(provider::GatewayKey {
            key: "disabled".to_owned(),
            protocol: String::new(),
            active: false,
        });
        assert!(key_candidates(
            &relay,
            "model",
            ApiProtocol::Chat,
            ApiProtocol::Chat,
            true
        )
        .is_empty());

        relay.has_configured_keys = false;
        relay.keys.clear();
        let candidates = key_candidates(
            &relay,
            "model",
            ApiProtocol::Chat,
            ApiProtocol::Chat,
            true,
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].key, None);
    }

    #[test]
    fn key_rotation_advances_between_requests() {
        let mut relay = provider("key-rotation-test", "Relay", &["model"]);
        relay.routing = "rotate".to_owned();
        relay.has_configured_keys = true;
        relay.keys = ["first", "second", "third"]
            .into_iter()
            .map(|key| provider::GatewayKey {
                key: key.to_owned(),
                protocol: String::new(),
                active: true,
            })
            .collect();

        let targets = || {
            key_candidates(
                &relay,
                "model",
                ApiProtocol::Chat,
                ApiProtocol::Chat,
                true,
            )
        };
        assert_eq!(targets()[0].key, Some("first"));
        assert_eq!(targets()[0].key, Some("second"));
    }

    #[test]
    fn usage_routing_prefers_the_key_with_fewer_recent_requests() {
        let mut relay = provider("key-usage-test", "Relay", &["model"]);
        relay.routing = "usage".to_owned();
        relay.has_configured_keys = true;
        relay.keys = ["first", "second"]
            .into_iter()
            .map(|key| provider::GatewayKey {
                key: key.to_owned(),
                protocol: String::new(),
                active: true,
            })
            .collect();

        let first_pass = key_candidates(
            &relay,
            "model",
            ApiProtocol::Chat,
            ApiProtocol::Chat,
            true,
        );
        assert_eq!(first_pass[0].key, Some("first"));
        mark_route_used(first_pass[0]);

        let next_pass = key_candidates(
            &relay,
            "model",
            ApiProtocol::Chat,
            ApiProtocol::Chat,
            true,
        );
        assert_eq!(next_pass[0].key, Some("second"));
    }

    #[test]
    fn failed_routes_are_held_back_until_the_cooldown_expires() {
        let mut relay = provider("route-cooldown-test", "Relay", &["model"]);
        relay.has_configured_keys = true;
        relay.keys = ["limited", "available"]
            .into_iter()
            .map(|key| provider::GatewayKey {
                key: key.to_owned(),
                protocol: String::new(),
                active: true,
            })
            .collect();

        let mut candidates = key_candidates(
            &relay,
            "model",
            ApiProtocol::Chat,
            ApiProtocol::Chat,
            true,
        );
        rest_route(candidates[0], ROUTE_COOLDOWN);
        move_resting_routes_last(&mut candidates);
        assert_eq!(candidates[0].key, Some("available"));
        assert_eq!(candidates[1].key, Some("limited"));
    }
}
