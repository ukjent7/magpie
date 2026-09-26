use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{
    Engine as _,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use reqwest::{Client, header};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CLIENT_VERSION: &str = "0.155.1";
const MAX_TOKEN_RESPONSE_BYTES: usize = 1 << 20;
const MAX_MODEL_RESPONSE_BYTES: usize = 4 << 20;

static REFRESH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone)]
pub(crate) struct Credentials {
    pub(crate) access_token: String,
    pub(crate) account_id: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CachedModels {
    client_version: String,
    models: Vec<CachedModel>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CachedModel {
    slug: String,
    visibility: String,
    supported_in_api: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModelResponse {
    models: Vec<RemoteModel>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RemoteModel {
    slug: String,
    display_name: String,
    visibility: String,
    priority: i64,
    input_modalities: Vec<String>,
    supported_reasoning_levels: Vec<ReasoningLevel>,
    context_window: usize,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ReasoningLevel {
    effort: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TokenResponse {
    access_token: String,
    id_token: String,
    refresh_token: String,
}

fn codex_directory() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home_directory().map(|home| home.join(".codex")))
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let variable = "USERPROFILE";
    #[cfg(not(windows))]
    let variable = "HOME";
    env::var_os(variable).map(PathBuf::from)
}

pub(crate) fn signed_in_auth_file() -> Option<PathBuf> {
    let path = codex_directory()?.join("auth.json");
    let contents = std::fs::read(&path).ok()?;
    let auth: Value = serde_json::from_slice(&contents).ok()?;
    let auth_mode = auth
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let access_token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    (auth_mode != "apikey" && !access_token.is_empty()).then_some(path)
}

pub(crate) fn signed_in_identity() -> Option<(String, String)> {
    let contents = std::fs::read(signed_in_auth_file()?).ok()?;
    let auth: Value = serde_json::from_slice(&contents).ok()?;
    let id_token = auth.pointer("/tokens/id_token").and_then(Value::as_str)?;
    let claims = jwt_claims(id_token);
    let account_claims = claims.get("https://api.openai.com/auth");
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let plan = account_claims
        .and_then(|claims| claims.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let user = match plan {
        "team" | "business" | "enterprise" | "edu" if !email.is_empty() => {
            let mut title = plan.to_owned();
            title[..1].make_ascii_uppercase();
            format!("{email} · {title}")
        }
        _ => email.to_owned(),
    };
    let user = if user.is_empty() {
        auth.pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        user
    };
    (!user.is_empty()).then(|| (user, plan.to_owned()))
}

pub(crate) fn cached_models() -> Vec<String> {
    let Some(path) = codex_directory().map(|directory| directory.join("models_cache.json")) else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(cache) = serde_json::from_slice::<CachedModels>(&contents) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    cache
        .models
        .into_iter()
        .filter(|model| {
            !model.slug.is_empty()
                && model.visibility != "hide"
                && model.supported_in_api != Some(false)
                && seen.insert(model.slug.clone())
        })
        .map(|model| model.slug)
        .collect()
}

pub(crate) async fn credentials(client: &Client, auth_file: &Path) -> Result<Credentials> {
    let _guard = REFRESH_LOCK.lock().await;
    let mut auth = read_auth(auth_file).await?;
    let mut access_token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("Codex is signed out; run codex login")?;
    let id_token = auth
        .pointer("/tokens/id_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let account_id = auth
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .or_else(|| account_id_from_id_token(&id_token));

    if token_needs_refresh(&access_token) {
        let refresh_token = auth
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .context("Codex is signed out; run codex login")?;
        access_token = refresh(client, auth_file, &mut auth, &refresh_token).await?;
    }

    Ok(Credentials {
        access_token,
        account_id: account_id.unwrap_or_default(),
    })
}

async fn read_auth(path: &Path) -> Result<Value> {
    let contents = tokio::fs::read(path)
        .await
        .with_context(|| format!("read Codex sign-in at {}", path.display()))?;
    let auth: Value = serde_json::from_slice(&contents).context("parse Codex sign-in")?;
    ensure!(
        auth.pointer("/tokens/access_token")
            .and_then(Value::as_str)
            .is_some_and(|token| !token.is_empty())
            && auth.get("auth_mode").and_then(Value::as_str) != Some("apikey"),
        "Codex is signed out; run codex login"
    );
    Ok(auth)
}

async fn refresh(
    client: &Client,
    auth_file: &Path,
    auth: &mut Value,
    refresh_token: &str,
) -> Result<String> {
    let body = serde_json::to_vec(&json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email"
    }))
    .context("serialize Codex token refresh")?;
    let mut response = client
        .post(TOKEN_URL)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .context("request Codex token refresh")?;
    if !response.status().is_success() {
        bail!("Codex is signed out (token refresh failed); run codex login");
    }
    let contents = read_limited(&mut response, MAX_TOKEN_RESPONSE_BYTES).await?;
    let fresh: TokenResponse =
        serde_json::from_slice(&contents).context("parse Codex token refresh response")?;
    ensure!(
        !fresh.access_token.is_empty(),
        "Codex is signed out (token refresh failed); run codex login"
    );

    let tokens = auth
        .as_object_mut()
        .context("Codex sign-in must be a JSON object")?
        .entry("tokens")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Codex sign-in tokens must be an object")?;
    tokens.insert("access_token".to_owned(), json!(fresh.access_token));
    if !fresh.id_token.is_empty() {
        tokens.insert("id_token".to_owned(), json!(fresh.id_token));
    }
    if !fresh.refresh_token.is_empty() {
        tokens.insert("refresh_token".to_owned(), json!(fresh.refresh_token));
    }
    if let Some(object) = auth.as_object_mut() {
        object.insert(
            "last_refresh".to_owned(),
            json!(
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            ),
        );
    }
    let mut bytes = serde_json::to_vec_pretty(auth).context("serialize refreshed Codex sign-in")?;
    bytes.push(b'\n');
    let path = auth_file.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::config::atomic_write_secret_for_settings(&path, &bytes)
    })
    .await
    .context("save refreshed Codex sign-in")??;
    Ok(fresh.access_token)
}

async fn read_limited(response: &mut reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut contents = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read Codex response")? {
        ensure!(
            contents.len().saturating_add(chunk.len()) <= limit,
            "Codex response exceeds the size limit"
        );
        contents.extend_from_slice(&chunk);
    }
    Ok(contents)
}

fn token_needs_refresh(token: &str) -> bool {
    let Some(expiration) = jwt_claims(token)
        .get("exp")
        .and_then(Value::as_f64)
        .filter(|expiration| *expiration > 0.0)
    else {
        return false;
    };
    expiration <= time::OffsetDateTime::now_utc().unix_timestamp() as f64 + 300.0
}

fn jwt_claims(token: &str) -> Value {
    let Some(payload) = token.split('.').nth(1) else {
        return Value::Null;
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload));
    decoded
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn account_id_from_id_token(token: &str) -> Option<String> {
    jwt_claims(token)
        .get("https://api.openai.com/auth")
        .and_then(|claims| claims.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

pub(crate) async fn refresh_models(auth_file: &Path) -> Result<usize> {
    let client = crate::netproxy::builder()
        .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(8))
        .build()
        .context("create Codex models client")?;
    let credentials = credentials(&client, auth_file).await?;
    let client_version = codex_client_version();
    let url = format!("{CODEX_BASE}/models?client_version={client_version}");
    let mut request = client
        .get(url)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", credentials.access_token),
        )
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "codex_cli_rs")
        .header("User-Agent", user_agent())
        .header(header::ACCEPT, "application/json");
    if !credentials.account_id.is_empty() {
        request = request.header("chatgpt-account-id", credentials.account_id);
    }
    let mut response = request.send().await.context("request Codex model list")?;
    ensure!(
        response.status().is_success(),
        "Codex model list request failed"
    );
    let contents = read_limited(&mut response, MAX_MODEL_RESPONSE_BYTES).await?;
    let list: ModelResponse =
        serde_json::from_slice(&contents).context("parse Codex model list")?;
    let mut remote_models = list.models;
    remote_models.sort_by_key(|model| model.priority);
    let models = remote_models
        .into_iter()
        .filter(|model| !model.slug.is_empty() && model.visibility != "hide")
        .map(|model| {
            let images = model
                .input_modalities
                .iter()
                .any(|modality| modality == "image");
            crate::catalog::Model {
                id: model.slug,
                name: model.display_name,
                provider: "openai".to_owned(),
                context: model.context_window,
                efforts: model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|level| level.effort)
                    .filter(|effort| !effort.is_empty())
                    .collect(),
                images,
                image_input: Some(images),
                ..crate::catalog::Model::default()
            }
        })
        .collect::<Vec<_>>();
    ensure!(!models.is_empty(), "Codex listed no available models");
    let prompt_contents = contents;
    tokio::task::spawn_blocking(move || save_model_prompts(&prompt_contents))
        .await
        .context("save Codex model prompts")??;
    let count = models.len();
    crate::catalog::save_live("codex", CODEX_BASE, models)?;
    Ok(count)
}

fn codex_client_version() -> String {
    let cached = codex_directory()
        .map(|directory| directory.join("models_cache.json"))
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|contents| serde_json::from_slice::<CachedModels>(&contents).ok())
        .map(|cache| cache.client_version)
        .filter(|version| version_is_newer(version, CLIENT_VERSION));
    cached.unwrap_or_else(|| CLIENT_VERSION.to_owned())
}

fn version_is_newer(candidate: &str, baseline: &str) -> bool {
    let candidate = candidate
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>();
    let baseline = baseline
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>();
    let (Ok(candidate), Ok(baseline)) = (candidate, baseline) else {
        return false;
    };
    for (candidate, baseline) in candidate.iter().zip(&baseline) {
        match candidate.cmp(baseline) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    candidate.len() > baseline.len()
}

fn save_model_prompts(contents: &[u8]) -> Result<()> {
    let prompts_path = crate::settings::cache_dir().join("magpie/codex-prompts.json");
    let mut prompts = std::fs::read(&prompts_path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<HashMap<String, String>>(&contents).ok())
        .unwrap_or_default();
    let list: Value = serde_json::from_slice(contents).context("parse Codex model prompts")?;
    if let Some(models) = list.get("models").and_then(Value::as_array) {
        for model in models {
            let Some(slug) = model.get("slug").and_then(Value::as_str) else {
                continue;
            };
            let prompt = model
                .pointer("/model_messages/instructions_template")
                .and_then(Value::as_str)
                .filter(|prompt| !prompt.is_empty())
                .or_else(|| model.get("base_instructions").and_then(Value::as_str))
                .unwrap_or_default();
            if !prompt.is_empty() {
                prompts.insert(slug.to_owned(), prompt.to_owned());
            }
        }
    }
    let mut bytes = serde_json::to_vec(&prompts).context("serialize Codex model prompts")?;
    bytes.push(b'\n');
    crate::config::atomic_write_for_settings(&prompts_path, &bytes)
        .with_context(|| format!("save Codex prompts to {}", prompts_path.display()))
}

pub(crate) fn user_agent() -> String {
    let version = codex_client_version();
    let operating_system = match env::consts::OS {
        "macos" => "Mac OS",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => env::consts::OS,
    };
    let architecture = match env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        value => value,
    };
    let terminal = match env::consts::OS {
        "macos" => "Apple_Terminal/455",
        "windows" => "WindowsTerminal",
        _ => "xterm-256color",
    };
    format!("codex_cli_rs/{version} ({operating_system}; {architecture}) {terminal}")
}

pub(crate) async fn request_body(body: &Value) -> Value {
    let mut body = body.clone();
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    for key in [
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "previous_response_id",
        "user",
        "safety_identifier",
        "service_tier",
    ] {
        object.remove(key);
    }

    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(prompt) = object
        .get("input")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        object.insert(
            "input".to_owned(),
            json!([{"type":"message","role":"user","content":[{"type":"input_text","text":prompt}]}]),
        );
    }
    let mut input = object
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    input = codex_input(input);

    let own_instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if object
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(key) = conversation_key(&own_instructions, &input)
    {
        object.insert("prompt_cache_key".to_owned(), json!(key));
    }
    let mut instructions = model_instructions(&model).await;
    if !own_instructions.trim().is_empty() {
        if first_line(&own_instructions) == first_line(&instructions) {
            instructions = own_instructions.clone();
        } else {
            input.insert(
                0,
                json!({
                    "type":"message",
                    "role":"developer",
                    "content":[{"type":"input_text","text":own_instructions.clone()}]
                }),
            );
        }
    }
    object.insert("input".to_owned(), json!(input));
    if !instructions.is_empty() {
        object.insert("instructions".to_owned(), json!(instructions));
    }
    object.insert("store".to_owned(), json!(false));
    object.insert("stream".to_owned(), json!(true));
    object.insert("tool_choice".to_owned(), json!("auto"));
    object
        .entry("parallel_tool_calls".to_owned())
        .or_insert(json!(true));
    if !object.contains_key("text") {
        object.insert("text".to_owned(), json!({"verbosity":"medium"}));
    }

    let effort = object
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if effort == "ultra"
        && let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut)
    {
        reasoning.insert("effort".to_owned(), json!("max"));
    }
    if effort == "none" {
        object.remove("include");
    } else {
        if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
            reasoning
                .entry("summary".to_owned())
                .or_insert(json!("auto"));
        }
        let include = object
            .entry("include".to_owned())
            .or_insert_with(|| json!([]));
        if let Some(include) = include.as_array_mut()
            && !include
                .iter()
                .any(|value| value == "reasoning.encrypted_content")
        {
            include.push(json!("reasoning.encrypted_content"));
        }
    }
    body
}

fn codex_input(input: Vec<Value>) -> Vec<Value> {
    let mut calls = HashMap::<String, &'static str>::new();
    for item in &input {
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(id) = object.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        let kind = match object.get("type").and_then(Value::as_str) {
            Some("function_call") => Some("function_call_output"),
            Some("local_shell_call") => Some("local_shell_call_output"),
            Some("custom_tool_call") => Some("custom_tool_call_output"),
            _ => None,
        };
        if let Some(kind) = kind {
            calls.insert(id.trim().to_owned(), kind);
        }
    }

    input
        .into_iter()
        .filter_map(|mut item| {
            let Some(object) = item.as_object_mut() else {
                return Some(item);
            };
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if kind == "item_reference" {
                return None;
            }
            object.remove("id");
            if matches!(
                kind.as_str(),
                "function_call_output" | "custom_tool_call_output" | "local_shell_call_output"
            ) {
                let id = object
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let expected = calls.get(&id).copied();
                let valid = expected == Some(kind.as_str())
                    || (kind == "function_call_output"
                        && expected == Some("local_shell_call_output"));
                if !valid {
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .unwrap_or("tool");
                    let call_id = if id.is_empty() {
                        "unknown"
                    } else {
                        id.as_str()
                    };
                    let output = object.get("output").map_or_else(
                        || "null".to_owned(),
                        |value| {
                            value.as_str().map_or_else(
                                || serde_json::to_string(value).unwrap_or_default(),
                                str::to_owned,
                            )
                        },
                    );
                    let mut output = output.chars().take(16_000).collect::<String>();
                    if output.chars().count() >= 16_000 {
                        output.push_str("\n...[truncated]");
                    }
                    return Some(json!({
                        "type":"message",
                        "role":"assistant",
                        "content":format!("[Previous {name} result; call_id={call_id}]: {output}")
                    }));
                }
            }
            if object.get("role").and_then(Value::as_str) == Some("system")
                && object.get("type").is_none_or(|kind| kind == "message")
            {
                object.insert("role".to_owned(), json!("developer"));
            }
            Some(item)
        })
        .collect()
}

async fn model_instructions(model: &str) -> String {
    let bare_model = model.strip_prefix("codex/").unwrap_or(model);
    let prompt_cache = crate::settings::cache_dir().join("magpie/codex-prompts.json");
    if let Ok(contents) = tokio::fs::read(prompt_cache).await
        && let Ok(prompts) = serde_json::from_slice::<HashMap<String, String>>(&contents)
        && let Some(prompt) = prompts.get(bare_model).filter(|prompt| !prompt.is_empty())
    {
        return prompt.clone();
    }
    if let Some(path) = codex_directory().map(|directory| directory.join("models_cache.json"))
        && let Ok(contents) = tokio::fs::read(path).await
        && let Ok(cache) = serde_json::from_slice::<Value>(&contents)
        && let Some(models) = cache.get("models").and_then(Value::as_array)
        && let Some(model) = models
            .iter()
            .find(|model| model.get("slug").and_then(Value::as_str) == Some(bare_model))
    {
        let prompt = model
            .pointer("/model_messages/instructions_template")
            .and_then(Value::as_str)
            .or_else(|| model.get("base_instructions").and_then(Value::as_str))
            .unwrap_or_default();
        if !prompt.is_empty() {
            return prompt.to_owned();
        }
    }
    include_str!("../internal/codexcat/codex_prompt.md").to_owned()
}

fn conversation_key(instructions: &str, input: &[Value]) -> Option<String> {
    let mut digest = Sha256::new();
    digest.update(instructions.as_bytes());
    let mut has_user_message = false;
    for item in input {
        digest.update(serde_json::to_vec(item).ok()?);
        if item.get("role").and_then(Value::as_str) == Some("user") {
            has_user_message = true;
            break;
        }
    }
    if !has_user_message {
        return None;
    }
    let bytes = digest.finalize();
    let mut hex = String::with_capacity(64);
    use std::fmt::Write as _;
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn first_line(value: &str) -> &str {
    value.trim().lines().next().unwrap_or_default().trim()
}

pub(crate) fn cache_key(body: &Value) -> Option<&str> {
    body.get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
}
