use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    convert::Infallible,
    env,
    future::Future,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle};
use url::Url;

use crate::{
    affinity::{self, Context as AffinityContext, UsageScanner},
    provider::{self, GatewayCatalog, GatewayProvider},
};

pub mod ir;
pub mod parse;
pub mod prompt;
pub mod render;
pub mod stream;

const DEFAULT_ADDR: &str = "127.0.0.1:3425";
pub const TOKEN: &str = "magpie";
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const RESPONSES_PATH: &str = "/v1/responses";
const MESSAGES_PATH: &str = "/v1/messages";
const MESSAGE_COUNT_PATH: &str = "/v1/messages/count_tokens";
const MODELS_PATH: &str = "/v1/models";
pub(crate) const API_ROUTES: [(&str, &str, &str); 7] = [
    ("OpenAI Chat Completions", "POST", CHAT_COMPLETIONS_PATH),
    ("OpenAI Responses", "POST", RESPONSES_PATH),
    ("Anthropic Messages", "POST", MESSAGES_PATH),
    ("Anthropic token count", "POST", MESSAGE_COUNT_PATH),
    ("Model catalog", "GET", MODELS_PATH),
    (
        "Gemini generateContent",
        "POST",
        "/v1beta/models/{model}:{method}",
    ),
    ("Gemini model catalog", "GET", "/v1beta/models"),
];
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct GatewayState {
    client: Client,
}

pub(crate) struct BackgroundGateway {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
    syncing: Option<JoinHandle<()>>,
}

impl BackgroundGateway {
    pub(crate) async fn shutdown(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.context("join background gateway task")??;
        }
        stop_syncing(&mut self.syncing);
        Ok(())
    }
}

impl Drop for BackgroundGateway {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        stop_syncing(&mut self.syncing);
    }
}

// while_serving is the work only the magpie serving the gateway does,
// besides answering: keeping this computer's setup the same as the others'.
fn while_serving() -> JoinHandle<()> {
    tokio::spawn(crate::davsync::run())
}

fn stop_syncing(syncing: &mut Option<JoinHandle<()>>) {
    if let Some(task) = syncing.take() {
        task.abort();
    }
}

pub async fn command(args: &[String]) -> Result<()> {
    let mut addr = addr();
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

pub fn addr() -> String {
    env::var("MAGPIE_ADDR")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_ADDR.to_owned())
}

pub fn url() -> String {
    format!("http://{}", addr())
}

pub fn v1_url() -> String {
    format!("{}/v1", url())
}

pub(crate) async fn start_background() -> Result<Option<BackgroundGateway>> {
    if is_running().await {
        return Ok(None);
    }

    let addr = addr();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind gateway to {addr}"))?;
    let app = router()?;
    let (shutdown, shutdown_receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        serve_listener(listener, app, async move {
            let _ = shutdown_receiver.await;
        })
        .await
    });

    Ok(Some(BackgroundGateway {
        shutdown: Some(shutdown),
        task: Some(task),
        syncing: Some(while_serving()),
    }))
}

async fn is_running() -> bool {
    let Ok(client) = Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(750))
        .build()
    else {
        return false;
    };
    let Ok(response) = client.get(url()).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    let Ok(body) = response.bytes().await else {
        return false;
    };
    serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|body| body.get("name").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|name| name == "magpie")
}

async fn serve(addr: &str) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind gateway to {addr}"))?;
    let address = listener.local_addr().context("read gateway address")?;
    println!("magpie gateway listening on http://{address}");

    let syncing = while_serving();
    let result = serve_listener(listener, router()?, shutdown_signal()).await;
    syncing.abort();
    result
}

fn router() -> Result<Router> {
    let state = GatewayState {
        client: crate::netproxy::builder()
            .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(UPSTREAM_TIMEOUT)
            .pool_max_idle_per_host(8)
            .build()
            .context("create gateway HTTP client")?,
    };
    Ok(Router::new()
        .route("/", get(info))
        .route(MODELS_PATH, get(models))
        .route("/models", get(models))
        .route("/v1beta/models", get(gemini_models))
        .route("/v1beta/models/{*call}", post(gemini))
        .route(CHAT_COMPLETIONS_PATH, post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route(RESPONSES_PATH, post(responses))
        .route("/responses", post(responses))
        .route(MESSAGES_PATH, post(messages))
        .route("/messages", post(messages))
        .route(MESSAGE_COUNT_PATH, post(count_tokens))
        .route("/v1/magpie/quotas", get(quotas))
        .fallback(not_found)
        .with_state(state))
}

async fn serve_listener(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
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
            "/v1/models",
            "/v1beta/models/{model}:{method}"
        ]
    })))
}

// quotas is what is left of every subscription and key balance, for an
// agent choosing where to send its work. It names the accounts, so it
// answers only on this machine, when the gateway listens beyond it too.
async fn quotas(request: Request<Body>) -> Response {
    let loopback = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .is_some_and(|info| info.0.ip().is_loopback());
    if !loopback {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": {
                "message": "magpie's quotas are only told to this machine",
                "type": "forbidden",
            }})),
        )
            .into_response();
    }
    let data = crate::quota::report().await;
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn models(request: Request<Body>) -> std::result::Result<Json<Value>, ApiError> {
    let catalog = configured_catalog().await?;
    // catalogFor is the catalog as the agent asking is shown it
    let agent_id = crate::usage::agent_of(
        request
            .headers()
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    );
    let visibility = crate::provider::visible_to(&agent_id);
    let mut data = Vec::new();
    for provider in catalog
        .providers
        .iter()
        .filter(|provider| !provider.hidden && !provider.unlisted && has_endpoint(provider))
    {
        if let Some(names) = &visibility
            && !crate::provider::shows(names, &provider.family, &provider.id, None)
        {
            continue;
        }
        let metadata = crate::catalog::available_models(&provider.id, &provider.catalog_id)
            .into_iter()
            .map(|model| (model.id.clone(), model))
            .collect::<HashMap<_, _>>();
        for model in &provider.models {
            let known = metadata.get(model.as_str());
            data.push(model_object(
                format!("{}/{}", provider.id, model),
                model,
                &provider.id,
                known,
                &provider.contexts,
            ));
        }
    }
    for group in catalog.groups.iter() {
        if let Some(names) = &visibility
            && !crate::provider::shows(names, &group.family, &group.id, Some(&group.id))
        {
            continue;
        }
        data.push(json!({
            "id": format!("group/{}", group.id),
            "object": "model",
            "type": "model",
            "created": 0,
            "created_at": "2025-01-01T00:00:00Z",
            "owned_by": "magpie",
            "display_name": group.name
        }));
    }
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

// model_object says whether a model reasons and at which levels, and how
// long a request and a reply it takes, as the names clients read it by.
fn model_object(
    id: String,
    model: &str,
    owned_by: &str,
    known: Option<&crate::catalog::Model>,
    contexts: &BTreeMap<String, u64>,
) -> Value {
    let efforts = known.map_or(Vec::new(), |model| model.efforts.clone());
    let levels = efforts
        .iter()
        .map(|effort| json!({"effort": effort}))
        .collect::<Vec<_>>();
    let mut context = known.map_or(0, |model| model.context);
    if let Some(set) = contexts.get(model).or_else(|| contexts.get("*")) {
        context = *set as usize;
    }
    let output = known.map_or(0, |model| model.output);
    let mut object = json!({
        "id": id,
        "object": "model",
        "type": "model",
        "created": 0,
        "created_at": "2025-01-01T00:00:00Z",
        "owned_by": owned_by,
        "display_name": known.map_or(model.to_owned(), |model| model.name.clone()),
        "reasoning": !levels.is_empty(),
        "supported_reasoning_levels": levels,
    });
    if context > 0 {
        object["context_window"] = json!(context);
        object["context_length"] = json!(context);
        object["max_input_tokens"] = json!(context);
    }
    if output > 0 {
        object["max_output_tokens"] = json!(output);
    }
    object
}

async fn gemini_models() -> Response {
    let catalog = match configured_catalog().await {
        Ok(catalog) => catalog,
        Err(error) => return gemini_error(error.status, error.message),
    };
    let models = exposed_models(&catalog.providers)
        .map(|(provider, model)| gemini_model(&format!("{}/{}", provider.id, model), model))
        .chain(
            catalog
                .groups
                .iter()
                .map(|group| gemini_model(&format!("group/{}", group.id), &group.name)),
        )
        .collect::<Vec<_>>();
    Json(json!({"models":models})).into_response()
}

fn gemini_model(id: &str, name: &str) -> Value {
    json!({
        "name":format!("models/{id}"),
        "displayName":name,
        "description":format!("{name} via magpie"),
        "supportedGenerationMethods":["generateContent","streamGenerateContent","countTokens"]
    })
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
        Some(MESSAGE_COUNT_PATH),
    )
    .await
}

async fn gemini(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    let (mut parts, body) = request.into_parts();
    let Some(call) = parts.uri.path().strip_prefix("/v1beta/models/") else {
        return gemini_error(StatusCode::NOT_FOUND, "unknown Gemini API route");
    };
    let Some((model, method)) = call.rsplit_once(':') else {
        return gemini_error(
            StatusCode::NOT_FOUND,
            "expected /v1beta/models/{model}:generateContent",
        );
    };
    let model = model.strip_prefix("models/").unwrap_or(model).to_owned();
    if model.is_empty() {
        return gemini_error(StatusCode::BAD_REQUEST, "model name cannot be empty");
    }

    let bytes = match to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let status = if error.to_string().contains("length limit") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return gemini_error(status, "could not read request body");
        }
    };
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(body) => body,
        Err(_) => return gemini_error(StatusCode::BAD_REQUEST, "request body must be valid JSON"),
    };

    if method == "countTokens" {
        let request = body.get("generateContentRequest").unwrap_or(&body);
        return Json(json!({"totalTokens":crate::gemini::estimate_tokens(request)}))
            .into_response();
    }
    let streaming = match method {
        "generateContent" => false,
        "streamGenerateContent" => true,
        _ => return gemini_error(StatusCode::NOT_FOUND, &format!("unknown method {method}")),
    };

    let chat_request = crate::gemini::to_chat_request(&body, &model, streaming);
    let chat_body = match serde_json::to_vec(&chat_request) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("magpie: encode Gemini request: {error}");
            return gemini_error(StatusCode::BAD_REQUEST, "could not translate request");
        }
    };
    parts.uri = Uri::from_static("/v1/chat/completions");
    let response = forward(
        &state,
        Request::from_parts(parts, Body::from(chat_body)),
        ApiProtocol::Chat,
        None,
    )
    .await;
    gemini_response(response, &model, streaming).await
}

async fn gemini_response(response: Response, model: &str, streaming: bool) -> Response {
    let status = response.status();
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim() == "text/event-stream")
        });
    if status.is_success() && streaming && is_sse {
        return gemini_stream_response(response, model.to_owned());
    }

    let bytes = match to_bytes(response.into_body(), MAX_RESPONSE_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("magpie: read Gemini gateway response: {error}");
            return gemini_error(StatusCode::BAD_GATEWAY, "provider response failed");
        }
    };
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("magpie: parse Gemini gateway response: {error}");
            return gemini_error(StatusCode::BAD_GATEWAY, "provider returned invalid JSON");
        }
    };
    if !status.is_success() {
        let message = body
            .pointer("/error/message")
            .or_else(|| body.pointer("/message"))
            .and_then(Value::as_str)
            .unwrap_or("provider request failed");
        return gemini_error(status, message);
    }

    let body = crate::gemini::from_chat_response(&body, model);
    if streaming {
        let mut response = Response::new(Body::from(crate::gemini::single_event(&body)));
        *response.status_mut() = status;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        );
        response
    } else {
        (status, Json(body)).into_response()
    }
}

fn gemini_stream_response(response: Response, model: String) -> Response {
    let status = response.status();
    let headers = response.headers().clone();
    let stream = stream::unfold(
        (
            response.into_body().into_data_stream(),
            crate::gemini::StreamTranslator::new(model),
            VecDeque::<Vec<u8>>::new(),
            false,
        ),
        |(mut input, mut translator, mut pending, mut ended)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((
                        Ok::<_, Infallible>(event),
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
                        pending.push_back(translator.fail(&error.to_string()));
                        ended = true;
                    }
                    None => {
                        pending.extend(translator.finish());
                        ended = true;
                    }
                }
            }
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    copy_translated_headers(&mut response, &headers);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
}

fn gemini_error(status: StatusCode, message: &str) -> Response {
    let state = match status {
        StatusCode::BAD_REQUEST => "INVALID_ARGUMENT",
        StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
        StatusCode::FORBIDDEN => "PERMISSION_DENIED",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
        StatusCode::PAYLOAD_TOO_LARGE => "RESOURCE_EXHAUSTED",
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE => "UNAVAILABLE",
        _ => "INTERNAL",
    };
    (
        status,
        Json(json!({"error":{"code":status.as_u16(),"message":message,"status":state}})),
    )
        .into_response()
}

async fn forward(
    state: &GatewayState,
    request: Request<Body>,
    protocol: ApiProtocol,
    path_override: Option<&str>,
) -> Response {
    let request_started = Instant::now();
    let (parts, body) = request.into_parts();
    if path_override.is_none() {
        crate::usage::saw(&crate::usage::agent_of(
            parts
                .headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default(),
        ));
    }
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
    let (bytes, redacted) = redact_request(&bytes);
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
    let Some(requested_model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return api_error_for(
            protocol,
            StatusCode::BAD_REQUEST,
            "request must include a model",
        );
    };
    // Claude Code's mark for a model with a 1M window; it drops the mark
    // before asking, but a value in its settings still has it
    let model = requested_model
        .trim()
        .strip_suffix("[1m]")
        .unwrap_or(requested_model.trim())
        .to_owned();
    let catalog = match configured_catalog().await {
        Ok(catalog) => catalog,
        Err(error) => return api_error_for(protocol, error.status, error.message),
    };
    let providers = &catalog.providers;
    // a model's id without a provider in it that names a routing group is the
    // group's, as "group/<id>" is, rather than one provider's that serves it
    let asked = provider::group_for(&model, &catalog.groups).unwrap_or(model.clone());
    let group = asked
        .strip_prefix("group/")
        .and_then(|id| catalog.groups.iter().find(|group| group.id == id));
    let mut candidates = if let Some(group) = group {
        group_candidates(group, providers, protocol, path_override.is_none())
    } else if asked.starts_with("group/") {
        Vec::new()
    } else {
        let target = match resolve_model(&asked, providers, protocol, path_override.is_none()) {
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
    let (affinity_scope, affinity_mode, rotate_affinity) = if let Some(group) = group {
        (
            format!("group/{}", group.id),
            group.affinity.clone(),
            group.routing == "rotate",
        )
    } else {
        let (provider, resolved_model, _) = candidates[0];
        (
            format!("{}/{}", provider.id, resolved_model),
            provider.affinity.clone(),
            provider.routing == "rotate",
        )
    };
    let affinity_context = affinity::context(
        &affinity_scope,
        &affinity_mode,
        &parts.headers,
        protocol,
        &body,
    );
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
            key_candidates(provider, model, protocol, upstream, path_override.is_none())
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
        candidates
            .sort_by_key(|candidate| (!candidate.model_listed, key_fit(*candidate, protocol)));
    }
    move_resting_routes_last(&mut candidates);
    apply_affinity(&mut candidates, &affinity_context, rotate_affinity);

    // a group's rules: as a turn begins, the first rule it matches sends
    // the turn to its member, put ahead of the others for the failover
    if let Some(group) = group
        && let Some(saved) = crate::provider::groups()
            .ok()
            .and_then(|groups| groups.into_iter().find(|saved| saved.id == group.id))
    {
        let rules = crate::grouprule::rules_of(&saved);
        if !rules.is_empty() {
            let request_view = crate::grouprule::parse(protocol, &body);
            let contexts = crate::grouprule::member_contexts(&group.members);
            let agent_id = crate::usage::agent_of(
                parts
                    .headers
                    .get(header::USER_AGENT)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default(),
            );
            let rule_key = crate::grouprule::rule_key(&group.id, &parts.headers, &request_view);
            let hit = crate::grouprule::rule_for(
                &rule_key,
                &rules,
                &crate::grouprule::classifier_of(&saved),
                &request_view,
                &agent_id,
                &contexts,
                Some(&crate::grouprule::classify::GatewayClassifier),
            )
            .await;
            if let Some(hit) = hit
                && hit.n > 0
                && !hit.held
                && !hit.waits
                && !hit.unready
                && let Some((member_provider, member_model)) = hit.use_.split_once('/')
                && let Some(position) = candidates.iter().position(|candidate| {
                    candidate.provider.id == member_provider && candidate.model == member_model
                })
            {
                let member = candidates.remove(position);
                candidates.insert(0, member);
            }
        }
    }

    let mut selected = None;
    let mut body = body;
    let mut floored = false;
    let mut unthinking = false;
    let mut index = 0usize;
    while index < candidates.len() {
        let candidate = &candidates[index];
        let is_last = index + 1 == candidates.len();
        let mut response = match send_upstream(
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
                index += 1;
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
        if !floored && response.status() == StatusCode::BAD_REQUEST {
            // asked for fewer tokens than this provider answers with (#64):
            // the same one again with the reply's length raised
            let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
            let mut bytes = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            let error_text = String::from_utf8_lossy(&bytes).to_string();
            if let Some(floor) = token_floor(&error_text)
                && let Some(raised) = with_token_floor(&body, floor)
            {
                floored = true;
                body = raised;
                eprintln!(
                    "magpie: {} takes a reply of at least {floor} tokens; asking again",
                    candidate.label()
                );
                continue;
            }
            // a model that always thinks (GLM-5.3 answers 1210 to thinking
            // turned off) is asked again with thinking left to it
            if !unthinking && always_thinks(&error_text) {
                let raised = body.as_object().and_then(|object| {
                    let disabled = object
                        .get("thinking")
                        .and_then(|thinking| thinking.get("type"))
                        .and_then(Value::as_str)
                        == Some("disabled");
                    if !disabled {
                        return None;
                    }
                    let mut without = object.clone();
                    without.remove("thinking");
                    Some(Value::Object(without))
                });
                if let Some(raised) = raised {
                    unthinking = true;
                    body = raised;
                    eprintln!(
                        "magpie: {} always thinks; asking again with thinking left to it",
                        candidate.label()
                    );
                    continue;
                }
            }
            let usage_request = path_override.is_none().then(|| {
                crate::usage::Request::new(
                    parts
                        .headers
                        .get(header::USER_AGENT)
                        .and_then(|value| value.to_str().ok()),
                    &candidate.provider.id,
                    &candidate.provider.where_(),
                    candidate.model,
                    request_started,
                )
            });
            let error_body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if let Some(request) = usage_request {
                crate::usage::record(
                    request,
                    400,
                    affinity::usage_from_value(candidate.upstream, &error_body),
                );
            }
            let message = format!(
                "{}: {}",
                candidate.provider.name,
                upstream_error_message(&error_body, &bytes)
            );
            if too_long(StatusCode::BAD_REQUEST, &message) {
                let mut message = message;
                if protocol == ApiProtocol::Anthropic
                    && !message.to_ascii_lowercase().contains("prompt is too long")
                {
                    message = format!("prompt is too long: {message}");
                }
                return api_error_for(protocol, StatusCode::BAD_REQUEST, &message);
            }
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            if let Some(content_type) = content_type {
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, content_type);
            }
            return response;
        }
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
            index += 1;
            continue;
        }
        selected = Some((response, *candidate));
        break;
    }
    let Some((response, candidate)) = selected else {
        return api_error_for(protocol, StatusCode::BAD_GATEWAY, "provider request failed");
    };
    let usage_request = path_override.is_none().then(|| {
        crate::usage::Request::new(
            parts
                .headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            &candidate.provider.id,
            &candidate.provider.where_(),
            candidate.model,
            request_started,
        )
    });
    let affinity_record = response
        .status()
        .is_success()
        .then(|| (affinity_context, candidate.id()));
    if response.status().is_success() {
        mark_route_used(candidate);
    }
    let provider = candidate.provider;
    let upstream_protocol = candidate.upstream;
    if matches!(
        provider.account.as_ref(),
        Some(provider::ProviderAccount::Codex { .. })
    ) && !streaming
        && response.status().is_success()
    {
        return finish_redact(
            codex_non_stream_response(response, affinity_record, usage_request).await,
            redacted,
        );
    }
    let translated = upstream_protocol != protocol;
    if response.status().is_success() {
        if !translated {
            return finish_redact(
                relay(response, upstream_protocol, affinity_record, usage_request),
                redacted,
            );
        }
    } else {
        let provider_name = provider.name.clone();
        return upstream_error_response(
            response,
            upstream_protocol,
            protocol,
            &provider_name,
            affinity_record,
            usage_request,
        )
        .await;
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
        return finish_redact(
            translated_stream_response(
                response,
                upstream_protocol,
                protocol,
                &model,
                affinity_record,
                usage_request,
            ),
            redacted,
        );
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
    if let Some((context, route)) = affinity_record {
        affinity::record(
            &context,
            route,
            affinity::cache_read_from_value(upstream_protocol, &upstream_body),
        );
    }
    if let Some(request) = usage_request {
        crate::usage::record(
            request,
            status.as_u16(),
            affinity::usage_from_value(upstream_protocol, &upstream_body),
        );
    }
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
    finish_redact(
        translated_response(status, &upstream_headers, bytes, streaming),
        redacted,
    )
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
        if upstream_protocol == ApiProtocol::Anthropic && !object.contains_key("thinking") {
            // thinking off unless asked: Anthropic's API assumes that, but
            // DeepSeek's and other vendors' Anthropic endpoints think by
            // default, so the title requests spent their tokens thinking
            object.insert("thinking".to_owned(), json!({"type":"disabled"}));
        }
        body
    };
    if matches!(
        provider.account.as_ref(),
        Some(provider::ProviderAccount::Codex { .. })
    ) {
        body = crate::codex::request_body(&body).await;
    }
    if translated && streaming {
        body["stream"] = json!(true);
    }
    let body_value = body;
    let body = serde_json::to_vec(&body_value).context("serialize upstream request")?;
    let (base, headers, accept) = match provider.account.as_ref() {
        Some(provider::ProviderAccount::Codex { auth_file }) => {
            let credentials = crate::codex::credentials(&state.client, auth_file).await?;
            (
                upstream_protocol.base(provider).to_owned(),
                codex_upstream_headers(&credentials, &body_value)?,
                None,
            )
        }
        Some(provider::ProviderAccount::Zcode { key, .. }) => {
            // ZCode's GLM Coding Plan speaks Anthropic at its own endpoint;
            // its key authenticates either way Anthropic keys do
            let mut headers = upstream_headers(
                provider,
                upstream_protocol,
                Some(key.api_key.as_str()),
                incoming_headers,
            )
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("build headers for provider {}", provider.id))?;
            headers.insert(
                header::HeaderName::from_static("authorization"),
                header::HeaderValue::from_str(&format!("Bearer {}", key.api_key))
                    .context("build ZCode authorization header")?,
            );
            (
                key.endpoint().to_owned(),
                headers,
                Some("application/json, text/event-stream"),
            )
        }
        Some(provider::ProviderAccount::Copilot { account }) => {
            let session = crate::copilot::session(&state.client, account).await?;
            crate::copilot::accept_model(&state.client, account, &session, model).await;
            let base = if session.api_endpoint.is_empty() {
                upstream_protocol.base(provider)
            } else {
                &session.api_endpoint
            };
            (
                base.to_owned(),
                crate::copilot::upstream_headers(&session, &body_value)?,
                Some("application/json, text/event-stream"),
            )
        }
        None => (
            upstream_protocol.base(provider).to_owned(),
            upstream_headers(provider, upstream_protocol, key, incoming_headers)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("build headers for provider {}", provider.id))?,
            Some("application/json, text/event-stream"),
        ),
    };
    let url = upstream_url(
        &base,
        path_override.unwrap_or_else(|| upstream_protocol.path()),
        query,
    )
    .with_context(|| format!("build URL for provider {}", provider.id))?;
    let request = state.client.post(url).headers(headers);
    let request = match accept {
        Some(value) => request.header(header::ACCEPT, value),
        None => request,
    };
    request
        .body(body)
        .send()
        .await
        .with_context(|| format!("send request to provider {}", provider.id))
}

fn codex_upstream_headers(
    credentials: &crate::codex::Credentials,
    body: &Value,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", credentials.access_token))
            .context("Codex token cannot be used as an HTTP header")?,
    );
    if !credentials.account_id.is_empty() {
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_str(&credentials.account_id)
                .context("Codex account id cannot be used as an HTTP header")?,
        );
    }
    for (name, value) in [
        ("OpenAI-Beta", "responses=experimental".to_owned()),
        ("originator", "codex_cli_rs".to_owned()),
        ("User-Agent", crate::codex::user_agent()),
    ] {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).context("invalid Codex header name")?,
            HeaderValue::from_str(&value).context("invalid Codex request header")?,
        );
    }
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    if let Some(key) = crate::codex::cache_key(body) {
        let key =
            HeaderValue::from_str(key).context("Codex prompt cache key is not a valid header")?;
        headers.insert(HeaderName::from_static("session_id"), key.clone());
        headers.insert(HeaderName::from_static("conversation_id"), key);
    }
    Ok(headers)
}

async fn codex_non_stream_response(
    mut upstream: reqwest::Response,
    affinity_record: Option<(AffinityContext, String)>,
    usage_request: Option<crate::usage::Request>,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = match upstream.chunk().await {
        Ok(chunk) => chunk,
        Err(error) => {
            eprintln!("magpie: read Codex response: {error}");
            return api_error_for(
                ApiProtocol::Responses,
                StatusCode::BAD_GATEWAY,
                "provider response failed",
            );
        }
    } {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return api_error_for(
                ApiProtocol::Responses,
                StatusCode::BAD_GATEWAY,
                "provider response exceeds the gateway size limit",
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    let completed = match codex_completed_response(&bytes) {
        Some(response) => response,
        None => {
            eprintln!("magpie: Codex returned no completed Responses event");
            return api_error_for(
                ApiProtocol::Responses,
                StatusCode::BAD_GATEWAY,
                "provider response could not be read",
            );
        }
    };
    if let Some((context, route)) = affinity_record {
        affinity::record(
            &context,
            route,
            affinity::cache_read_from_value(ApiProtocol::Responses, &completed),
        );
    }
    if let Some(request) = usage_request {
        crate::usage::record(
            request,
            status.as_u16(),
            affinity::usage_from_value(ApiProtocol::Responses, &completed),
        );
    }
    match serde_json::to_vec(&completed) {
        Ok(bytes) => translated_response(status, &headers, bytes, false),
        Err(error) => {
            eprintln!("magpie: serialize Codex response: {error}");
            api_error_for(
                ApiProtocol::Responses,
                StatusCode::BAD_GATEWAY,
                "provider response could not be read",
            )
        }
    }
}

fn codex_completed_response(bytes: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes)
        && value.get("object").and_then(Value::as_str) == Some("response")
    {
        return Some(value);
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut data = String::new();
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty()
                && data != "[DONE]"
                && let Ok(event) = serde_json::from_str::<Value>(&data)
                && matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("response.completed" | "response.incomplete" | "response.failed")
                )
            {
                return event.get("response").cloned();
            }
            data.clear();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    None
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
        let (listed, mut others): (Vec<_>, Vec<_>) =
            candidates.into_iter().partition(|candidate| {
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
            | (ApiProtocol::Chat, ApiProtocol::Responses)
            | (ApiProtocol::Responses, ApiProtocol::Chat)
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

fn apply_affinity(candidates: &mut [RouteCandidate<'_>], context: &AffinityContext, rotate: bool) {
    let Some(previous) = affinity::previous(context) else {
        return;
    };
    let Some(last) = candidates
        .iter()
        .position(|candidate| candidate.id() == previous.route)
    else {
        return;
    };
    let last_is_resting = ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .resting
        .get(&previous.route)
        .is_some_and(|until| *until > Instant::now());
    if last_is_resting {
        return;
    }

    if affinity::should_keep(context, &previous, rotate) {
        candidates.rotate_left(last);
    } else if affinity::should_advance(context, rotate) {
        for distance in 1..candidates.len() {
            let next = (last + distance) % candidates.len();
            if !candidates[next].model_listed || route_is_resting(candidates[next]) {
                continue;
            }
            candidates.rotate_left(next);
            break;
        }
    }
}

fn route_is_resting(candidate: RouteCandidate<'_>) -> bool {
    ROUTING_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .resting
        .get(&candidate.id())
        .is_some_and(|until| *until > Instant::now())
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
pub enum ApiProtocol {
    Chat,
    Responses,
    Anthropic,
}

impl ApiProtocol {
    const fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
        }
    }

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
                endpoint_for(protocol, provider, model, allow_translation)
                    .map(|upstream| (provider, model, upstream))
            })
            .ok_or(ResolveError::Unknown);
    }

    let mut matches = providers.iter().filter_map(|provider| {
        if provider.hidden || !provider.models.iter().any(|model| model == requested) {
            return None;
        }
        endpoint_for(protocol, provider, requested, allow_translation)
            .map(|upstream| (provider, upstream))
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
    model: &str,
    allow_translation: bool,
) -> Option<ApiProtocol> {
    let supports = |api: ApiProtocol| {
        provider.model_apis.get(model).map_or_else(
            || {
                !matches!(
                    provider.account.as_ref(),
                    Some(provider::ProviderAccount::Copilot { .. })
                ) || api == ApiProtocol::Chat
            },
            |apis| apis.contains(api.name()),
        )
    };
    if supports(protocol) && !protocol.base(provider).is_empty() {
        return Some(protocol);
    }
    if !allow_translation {
        return None;
    }
    let alternatives: &[ApiProtocol] = match protocol {
        ApiProtocol::Chat => &[ApiProtocol::Anthropic, ApiProtocol::Responses],
        ApiProtocol::Anthropic | ApiProtocol::Responses => &[ApiProtocol::Chat],
    };
    alternatives.iter().copied().find(|alternative| {
        translation_supported(protocol, *alternative)
            && supports(*alternative)
            && !alternative.base(provider).is_empty()
    })
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

fn relay(
    upstream: reqwest::Response,
    protocol: ApiProtocol,
    affinity_record: Option<(AffinityContext, String)>,
    usage_request: Option<crate::usage::Request>,
) -> Response {
    let status = upstream.status();
    let record_status = status.as_u16();
    let headers = upstream.headers().clone();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let scanner = Some(UsageScanner::new(protocol, content_type));
    let body_stream = stream::unfold(
        (
            upstream.bytes_stream(),
            scanner,
            affinity_record,
            usage_request,
        ),
        move |(mut input, mut scanner, mut affinity_record, mut usage_request)| async move {
            match input.next().await {
                Some(Ok(chunk)) => {
                    if let Some(scanner) = scanner.as_mut() {
                        scanner.push(&chunk);
                    }
                    Some((
                        Ok::<_, reqwest::Error>(chunk),
                        (input, scanner, affinity_record, usage_request),
                    ))
                }
                Some(Err(error)) => {
                    let usage = finish_scanner(&mut scanner);
                    if let Some(request) = usage_request.take() {
                        crate::usage::record(request, record_status, usage);
                    }
                    Some((Err(error), (input, None, None, None)))
                }
                None => {
                    let usage = finish_scanner(&mut scanner);
                    if let Some(request) = usage_request.take() {
                        crate::usage::record(request, record_status, usage);
                    }
                    if let Some((context, route)) = affinity_record.take() {
                        affinity::record(&context, route, usage.cache_read);
                    }
                    None
                }
            }
        },
    );
    let mut response = Response::new(Body::from_stream(body_stream));
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
    affinity_record: Option<(AffinityContext, String)>,
    usage_request: Option<crate::usage::Request>,
) -> Response {
    let status = upstream.status();
    let record_status = status.as_u16();
    let upstream_headers = upstream.headers().clone();
    let Some(translator) = crate::translation::SseTranslator::new(from, to, model) else {
        return api_error_for(
            to,
            StatusCode::BAD_GATEWAY,
            "streaming translation is not supported for this protocol pair",
        );
    };
    let scanner = Some({
        UsageScanner::new(
            from,
            upstream_headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
        )
    });
    let body_stream = stream::unfold(
        (
            upstream.bytes_stream(),
            translator,
            VecDeque::new(),
            false,
            scanner,
            affinity_record,
            usage_request,
        ),
        move |(
            mut input,
            mut translator,
            mut pending,
            mut ended,
            mut scanner,
            mut affinity_record,
            mut usage_request,
        )| async move {
            loop {
                if let Some(frame) = pending.pop_front() {
                    return Some((
                        Ok::<_, reqwest::Error>(frame),
                        (
                            input,
                            translator,
                            pending,
                            ended,
                            scanner,
                            affinity_record,
                            usage_request,
                        ),
                    ));
                }
                if ended {
                    let usage = finish_scanner(&mut scanner);
                    if let Some((context, route)) = affinity_record.take() {
                        affinity::record(&context, route, usage.cache_read);
                    }
                    if let Some(request) = usage_request.take() {
                        crate::usage::record(request, record_status, usage);
                    }
                    return None;
                }
                match input.next().await {
                    Some(Ok(chunk)) => {
                        if let Some(scanner) = scanner.as_mut() {
                            scanner.push(&chunk);
                        }
                        pending.extend(translator.push(&chunk));
                        ended = translator.is_ended();
                    }
                    Some(Err(error)) => {
                        let usage = finish_scanner(&mut scanner);
                        if let Some(request) = usage_request.take() {
                            crate::usage::record(request, record_status, usage);
                        }
                        ended = true;
                        return Some((
                            Err(error),
                            (input, translator, pending, ended, None, None, None),
                        ));
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

fn finish_scanner(scanner: &mut Option<UsageScanner>) -> crate::usage::TokenUsage {
    scanner
        .take()
        .map_or_else(crate::usage::TokenUsage::default, |mut scanner| {
            scanner.finish_usage()
        })
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

// token_floor is the least reply length a provider said it takes, from the
// 400 it sends when a request asks for less ("max_tokens must be greater
// than 2"). Apps checking a model is up ask for a token or two, which most
// providers answer and some turn away. None when the error isn't about that.
fn token_floor(message: &str) -> Option<u64> {
    let message = message.to_ascii_lowercase();
    for key in ["max_completion_tokens", "max_output_tokens", "max_tokens"] {
        let Some(at) = message.find(key) else {
            continue;
        };
        let window = &message[(at + key.len()).min(message.len())..];
        let window = &window[..window.len().min(60)];
        const TRIGGERS: &[(&str, u64)] = &[
            ("greater than", 1),
            ("more than", 1),
            ("larger than", 1),
            ("at least", 0),
            (">=", 0),
            (">", 1),
        ];
        for (trigger, add) in TRIGGERS {
            if let Some(where_) = window.find(trigger) {
                let digits: String = window[where_ + trigger.len()..]
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                return digits
                    .parse::<u64>()
                    .ok()
                    .filter(|floor| *floor <= 1024)
                    .map(|floor| floor + add);
            }
        }
    }
    None
}

// always_thinks is a vendor refusing to turn a model's thinking off:
// Z.ai's GLM-5.3 answers 1210, "…always engages in thinking…".
fn always_thinks(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("always engages in thinking")
        || message.contains("cannot be disabled")
        || message.contains("can not be disabled")
        || message.contains("can't be disabled")
        || message.contains("cannot be turned off")
        || message.contains("can't be turned off")
}

// with_token_floor raises the reply's length the request asks for to floor,
// wherever the request's protocol keeps it; None when it asked for that much
// already, so the error was about something else.
fn with_token_floor(body: &Value, floor: u64) -> Option<Value> {
    if floor == 0 {
        return None;
    }
    let mut object = body.as_object()?.clone();
    let mut raised = false;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if let Some(value) = object.get(key).and_then(Value::as_u64)
            && value < floor
        {
            object.insert(key.to_owned(), json!(floor));
            raised = true;
        }
    }
    if let Some(config) = object
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
        && let Some(value) = config.get("maxOutputTokens").and_then(Value::as_u64)
        && value < floor
    {
        config.insert("maxOutputTokens".to_owned(), json!(floor));
        raised = true;
    }
    raised.then_some(Value::Object(object))
}

// upstream_error_response passes a vendor's failure on to the agent, but a
// vendor saying the conversation is too long is said the client's way, so
// the agent compacts and retries rather than stopping.
async fn upstream_error_response(
    mut upstream: reqwest::Response,
    upstream_protocol: ApiProtocol,
    client_protocol: ApiProtocol,
    provider_name: &str,
    affinity_record: Option<(AffinityContext, String)>,
    usage_request: Option<crate::usage::Request>,
) -> Response {
    let status = upstream.status();
    let record_status = status.as_u16();
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = upstream.chunk().await {
        if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if let Some(request) = usage_request {
        crate::usage::record(
            request,
            record_status,
            affinity::usage_from_value(upstream_protocol, &body),
        );
    }
    if let Some((context, route)) = affinity_record {
        affinity::record(
            &context,
            route,
            affinity::cache_read_from_value(upstream_protocol, &body),
        );
    }
    let message = format!("{provider_name}: {}", upstream_error_message(&body, &bytes));
    if too_long(status, &message) {
        let mut message = message;
        if client_protocol == ApiProtocol::Anthropic
            && !message.to_ascii_lowercase().contains("prompt is too long")
        {
            message = format!("prompt is too long: {message}");
        }
        return api_error_for(client_protocol, StatusCode::BAD_REQUEST, &message);
    }
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    if let Some(content_type) = upstream.headers().get(header::CONTENT_TYPE) {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.clone());
    }
    response
}

// redact_request masks a request's secrets, as the settings say, before it
// goes to a vendor; finish_redact wraps the response so what the vendor
// answers has them back. Nothing masked, nothing wrapped.
fn redact_request(bytes: &[u8]) -> (Vec<u8>, bool) {
    let settings = crate::settings::load();
    if !settings.redact && !settings.redact_personal && settings.redact_words.is_empty() {
        return (bytes.to_vec(), false);
    }
    if !crate::redact::known() {
        crate::redact::set_key_path(
            crate::settings::path()
                .with_file_name("redact.key")
                .display()
                .to_string(),
        );
    }
    let options = crate::redact::Options {
        secrets: settings.redact,
        personal: settings.redact_personal,
        words: settings.redact_words.clone(),
    };
    let (masked, count) = crate::redact::mask_json(bytes, &options);
    (masked, count > 0)
}

fn finish_redact(mut response: Response, redacted: bool) -> Response {
    if !redacted {
        return response;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_encoding = response
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    response.headers_mut().remove(header::CONTENT_LENGTH);
    let status = response.status();
    let headers = response.headers().clone();
    let writer = std::sync::Arc::new(std::sync::Mutex::new(crate::redact::Writer::new(
        content_type.as_deref(),
        content_encoding.as_deref(),
    )));
    let finish_writer = writer.clone();
    let stream = response
        .into_body()
        .into_data_stream()
        .map(move |chunk| match chunk {
            Ok(bytes) => {
                let out = writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .write(&bytes);
                Ok(axum::body::Bytes::from(out))
            }
            Err(error) => Err(error),
        })
        .chain(stream::once(async move {
            let out = finish_writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .finish();
            Ok::<_, axum::Error>(axum::body::Bytes::from(out))
        }));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn upstream_error_message(body: &Value, raw: &[u8]) -> String {
    if let Some(message) = body.pointer("/error/message").and_then(Value::as_str) {
        return message.to_owned();
    }
    if let Some(message) = body.pointer("/error").and_then(Value::as_str) {
        return message.to_owned();
    }
    String::from_utf8_lossy(raw).chars().take(300).collect()
}

// too_long is whether a vendor's error says the conversation no longer
// fits: OpenAI's context_length_exceeded, Anthropic's "prompt is too
// long", Volcengine's "Input exceeds the context limit", "maximum context
// length", "context window"… One about max_tokens is left alone: the
// reply's allowance, not the conversation, is what is too big there, and
// compacting won't help.
fn too_long(status: StatusCode, message: &str) -> bool {
    let code = status.as_u16();
    if !(400..500).contains(&code) {
        return false;
    }
    let message = message.to_ascii_lowercase();
    if ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .iter()
        .any(|token| message.contains(token))
    {
        return false;
    }
    message.contains("context_length_exceeded")
        || message.contains("prompt is too long")
        || message.contains("input is too long")
        || (message.contains("exceed") && message.contains("context"))
        || (message.contains("beyond") && message.contains("context"))
        || message.contains("maximum context")
        || message.contains("too many input tokens")
        || message.contains("too many prompt tokens")
        || message.contains("too many tokens")
        || (message.contains("上下文") && (message.contains("超") || message.contains("过长")))
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
            affinity: String::new(),
            keys: Vec::new(),
            has_configured_keys: false,
            headers: BTreeMap::new(),
            models: models.iter().map(|model| (*model).to_owned()).collect(),
            model_keys: HashMap::new(),
            model_apis: HashMap::new(),
            account: None,
            hidden: false,
            unlisted: false,
            contexts: BTreeMap::new(),
            family: String::new(),
            catalog_id: String::new(),
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
            affinity: String::new(),
            family: String::new(),
        };

        let candidates = group_candidates(&group, &providers, ApiProtocol::Chat, true);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].0.id, "second");
        assert_eq!(candidates[1].0.id, "first");
    }

    #[test]
    fn resolves_a_responses_endpoint_for_chat_when_translation_is_enabled() {
        let mut provider = provider("relay", "Relay", &["model"]);
        provider.chat.clear();
        provider.responses = "https://api.example/v1".to_owned();
        let providers = [provider];

        let (_, _, upstream) = resolve_model("relay/model", &providers, ApiProtocol::Chat, true)
            .expect("Chat requests can be translated to Responses");
        assert_eq!(upstream, ApiProtocol::Responses);
        assert!(matches!(
            resolve_model("relay/model", &providers, ApiProtocol::Chat, false),
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
        let provider = provider("relay", "Relay", &[]);
        let mut incoming = HeaderMap::new();
        incoming.insert("anthropic-version", HeaderValue::from_static("2024-01-01"));

        let headers =
            upstream_headers(&provider, ApiProtocol::Anthropic, Some("secret"), &incoming)
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
        assert!(
            key_candidates(&relay, "model", ApiProtocol::Chat, ApiProtocol::Chat, true).is_empty()
        );

        relay.has_configured_keys = false;
        relay.keys.clear();
        let candidates =
            key_candidates(&relay, "model", ApiProtocol::Chat, ApiProtocol::Chat, true);
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

        let targets =
            || key_candidates(&relay, "model", ApiProtocol::Chat, ApiProtocol::Chat, true);
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

        let first_pass =
            key_candidates(&relay, "model", ApiProtocol::Chat, ApiProtocol::Chat, true);
        assert_eq!(first_pass[0].key, Some("first"));
        mark_route_used(first_pass[0]);

        let next_pass = key_candidates(&relay, "model", ApiProtocol::Chat, ApiProtocol::Chat, true);
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

        let mut candidates =
            key_candidates(&relay, "model", ApiProtocol::Chat, ApiProtocol::Chat, true);
        rest_route(candidates[0], ROUTE_COOLDOWN);
        move_resting_routes_last(&mut candidates);
        assert_eq!(candidates[0].key, Some("available"));
        assert_eq!(candidates[1].key, Some("limited"));
    }

    #[test]
    fn session_affinity_restores_the_key_that_answered_the_previous_request() {
        let providers = [
            provider("affinity-session-first", "First", &["model"]),
            provider("affinity-session-second", "Second", &["model"]),
        ];
        let mut candidates = providers
            .iter()
            .flat_map(|provider| {
                key_candidates(
                    provider,
                    "model",
                    ApiProtocol::Chat,
                    ApiProtocol::Chat,
                    true,
                )
            })
            .collect::<Vec<_>>();
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("session-test"));
        let body = json!({"messages":[{"role":"user","content":"hello"}]});
        let previous = affinity::context(
            "affinity-session-route-test",
            "session",
            &headers,
            ApiProtocol::Chat,
            &body,
        );
        let last_route = candidates[1].id();
        affinity::record(&previous, last_route.clone(), 0);

        let next = affinity::context(
            "affinity-session-route-test",
            "session",
            &headers,
            ApiProtocol::Chat,
            &body,
        );
        apply_affinity(&mut candidates, &next, false);
        assert_eq!(candidates[0].id(), last_route);
    }

    #[test]
    fn rotating_affinity_advances_after_the_last_route_on_a_new_turn() {
        let providers = [
            provider("affinity-rotate-first", "First", &["model"]),
            provider("affinity-rotate-second", "Second", &["model"]),
            provider("affinity-rotate-third", "Third", &["model"]),
        ];
        let mut candidates = providers
            .iter()
            .flat_map(|provider| {
                key_candidates(
                    provider,
                    "model",
                    ApiProtocol::Chat,
                    ApiProtocol::Chat,
                    true,
                )
            })
            .collect::<Vec<_>>();
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("session-test"));
        let previous_body = json!({"messages":[{"role":"user","content":"first"}]});
        let previous = affinity::context(
            "affinity-rotate-route-test",
            "auto",
            &headers,
            ApiProtocol::Chat,
            &previous_body,
        );
        let last_route = candidates[1].id();
        affinity::record(&previous, last_route, 2048);

        let next_body = json!({"messages":[
            {"role":"user","content":"first"},
            {"role":"assistant","content":"answer"},
            {"role":"user","content":"second"}
        ]});
        let next = affinity::context(
            "affinity-rotate-route-test",
            "auto",
            &headers,
            ApiProtocol::Chat,
            &next_body,
        );
        apply_affinity(&mut candidates, &next, true);
        assert_eq!(candidates[0].provider.id, "affinity-rotate-third");
    }
}
