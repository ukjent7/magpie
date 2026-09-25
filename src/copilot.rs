use std::{
    collections::{HashMap, HashSet},
    env,
    path::PathBuf,
    sync::{LazyLock, Mutex as StdMutex},
    time::{Duration, Instant},
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;

use anyhow::{Context, Result, ensure};
use jsonc_parser::{ParseOptions, cst::CstRootNode};
use reqwest::{Client, header};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const COPILOT_USER_URL: &str = "https://api.github.com/copilot_internal/user";
const DEFAULT_API: &str = "https://api.githubcopilot.com";
const MAX_TOKEN_RESPONSE_BYTES: usize = 1 << 20;
const MAX_MODEL_RESPONSE_BYTES: usize = 4 << 20;

static SESSION_CACHE: LazyLock<Mutex<HashMap<(String, bool), Session>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_TERMS: LazyLock<Mutex<HashMap<String, HashSet<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CLI_SECRET_CACHE: LazyLock<StdMutex<HashMap<String, CachedCliSecret>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

struct CachedCliSecret {
    fetched_at: Instant,
    token: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Account {
    pub(crate) github_token: String,
    pub(crate) user: String,
    direct: bool,
}

#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) token: String,
    pub(crate) api_endpoint: String,
    direct: bool,
    expires_at: i64,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredApp {
    user: String,
    oauth_token: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TokenResponse {
    token: String,
    expires_at: i64,
    endpoints: TokenEndpoints,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TokenEndpoints {
    api: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotUserResponse {
    endpoints: TokenEndpoints,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotCliUser {
    host: String,
    login: String,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotCliConfig {
    last_logged_in_user: Option<CopilotCliUser>,
    logged_in_users: Vec<CopilotCliUser>,
    copilot_tokens: HashMap<String, String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModelResponse {
    data: Vec<CopilotModel>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotModel {
    id: String,
    name: String,
    vendor: String,
    model_picker_enabled: bool,
    model_picker_category: String,
    supported_endpoints: Vec<String>,
    capabilities: ModelCapabilities,
    policy: Option<ModelPolicy>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModelCapabilities {
    #[serde(rename = "type")]
    kind: String,
    supports: ModelSupport,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModelSupport {
    reasoning_effort: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModelPolicy {
    state: String,
    terms: String,
}

pub(crate) fn signed_in_account() -> Option<Account> {
    if let Some(config_dir) = config_directory() {
        for filename in ["apps.json", "hosts.json"] {
            let Ok(contents) = std::fs::read(config_dir.join("github-copilot").join(filename))
            else {
                continue;
            };
            let Ok(apps) = serde_json::from_slice::<HashMap<String, StoredApp>>(&contents) else {
                continue;
            };
            let mut hosts = apps.into_iter().collect::<Vec<_>>();
            hosts.sort_by(|left, right| left.0.cmp(&right.0));
            for (host, app) in hosts {
                if host.starts_with("github.com") && !app.oauth_token.is_empty() {
                    return Some(Account {
                        github_token: app.oauth_token,
                        user: app.user,
                        direct: false,
                    });
                }
            }
        }
    }
    copilot_cli_account()
}

fn copilot_cli_account() -> Option<Account> {
    let home = env::var_os("COPILOT_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_directory().map(|home| home.join(".copilot")))?;
    let contents = std::fs::read_to_string(home.join("config.json")).ok()?;
    let root = CstRootNode::parse(&contents, &ParseOptions::default()).ok()?;
    let config: CopilotCliConfig = serde_json::from_value(root.value()?.to_serde_value()?).ok()?;
    let users = config
        .last_logged_in_user
        .into_iter()
        .chain(config.logged_in_users);
    for user in users {
        if user.login.is_empty() || (!user.host.is_empty() && user.host != "https://github.com") {
            continue;
        }
        let key = format!("https://github.com:{}", user.login);
        let token = config
            .copilot_tokens
            .get(&key)
            .filter(|token| !token.is_empty())
            .cloned()
            .or_else(|| copilot_cli_secret(&key));
        let Some(token) = token else {
            continue;
        };
        return Some(Account {
            github_token: token,
            user: user.login,
            direct: true,
        });
    }
    None
}

fn copilot_cli_secret(account: &str) -> Option<String> {
    let mut cache = CLI_SECRET_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if let Some(secret) = cache.get(account) {
        let ttl = if secret.token.is_some() {
            Duration::from_secs(60 * 60)
        } else {
            Duration::from_secs(10 * 60)
        };
        if now.saturating_duration_since(secret.fetched_at) < ttl {
            return secret.token.clone();
        }
    }

    #[cfg(target_os = "macos")]
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "copilot-cli",
            "-a",
            account,
            "-w",
        ])
        .output();
    #[cfg(target_os = "linux")]
    let output = Command::new("secret-tool")
        .args(["lookup", "service", "copilot-cli", "account", account])
        .output();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let output: std::io::Result<std::process::Output> = Err(std::io::ErrorKind::Unsupported.into());

    let token = output
        .ok()
        .filter(|result| result.status.success())
        .and_then(|result| {
            let token = String::from_utf8_lossy(&result.stdout).trim().to_owned();
            (!token.is_empty()).then_some(token)
        });
    cache.insert(
        account.to_owned(),
        CachedCliSecret {
            fetched_at: now,
            token: token.clone(),
        },
    );
    token
}

fn config_directory() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_directory().map(|home| home.join(".config")))
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let variable = "USERPROFILE";
    #[cfg(not(windows))]
    let variable = "HOME";
    env::var_os(variable).map(PathBuf::from)
}

pub(crate) async fn session(client: &Client, account: &Account) -> Result<Session> {
    let github_token = &account.github_token;
    let mut sessions = SESSION_CACHE.lock().await;
    let cache_key = (github_token.clone(), account.direct);
    if let Some(session) = sessions.get(&cache_key).filter(|session| {
        session.expires_at > time::OffsetDateTime::now_utc().unix_timestamp() + 120
    }) {
        return Ok(session.clone());
    }

    if account.direct {
        let mut response = client
            .get(COPILOT_USER_URL)
            .header(header::AUTHORIZATION, format!("token {github_token}"))
            .header(header::ACCEPT, "application/json")
            .header("Editor-Version", "copilot/1.0.88")
            .header("Copilot-Integration-Id", "copilot-developer-cli")
            .header("X-GitHub-Api-Version", "2026-07-01")
            .header("User-Agent", "copilot/1.0.88")
            .send()
            .await
            .context("request Copilot CLI account endpoint")?;
        ensure!(
            response.status().is_success(),
            "Copilot CLI sign-in was refused; run `copilot login` again"
        );
        let contents = read_limited(&mut response, MAX_TOKEN_RESPONSE_BYTES).await?;
        let user: CopilotUserResponse =
            serde_json::from_slice(&contents).context("parse Copilot CLI account endpoint")?;
        let session = Session {
            token: github_token.clone(),
            api_endpoint: user.endpoints.api,
            direct: true,
            expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 30 * 60,
        };
        sessions.insert(cache_key, session.clone());
        return Ok(session);
    }

    let mut response = client
        .get(TOKEN_URL)
        .header(header::AUTHORIZATION, format!("token {github_token}"))
        .header(header::ACCEPT, "application/json")
        .header("Editor-Version", "vscode/1.104.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.31.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("User-Agent", "GitHubCopilotChat/0.31.0")
        .send()
        .await
        .context("request Copilot session token")?;
    ensure!(
        response.status().is_success(),
        "Copilot sign-in was refused; sign in to Copilot again"
    );
    let contents = read_limited(&mut response, MAX_TOKEN_RESPONSE_BYTES).await?;
    let token: TokenResponse =
        serde_json::from_slice(&contents).context("parse Copilot session token")?;
    ensure!(
        !token.token.is_empty(),
        "Copilot did not return a session token"
    );
    let session = Session {
        token: token.token,
        api_endpoint: token.endpoints.api,
        direct: false,
        expires_at: token.expires_at,
    };
    sessions.insert(cache_key, session.clone());
    Ok(session)
}

pub(crate) fn upstream_headers(
    session: &Session,
    body: &Value,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", session.token))
            .context("Copilot session token cannot be used as an HTTP header")?,
    );
    if session.direct {
        headers.insert(
            "Editor-Version",
            reqwest::header::HeaderValue::from_static("copilot/1.0.88"),
        );
        headers.insert(
            "Copilot-Integration-Id",
            reqwest::header::HeaderValue::from_static("copilot-developer-cli"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            reqwest::header::HeaderValue::from_static("2026-07-01"),
        );
        headers.insert(
            "User-Agent",
            reqwest::header::HeaderValue::from_static("copilot/1.0.88"),
        );
    } else {
        headers.insert(
            "Editor-Version",
            reqwest::header::HeaderValue::from_static("vscode/1.104.0"),
        );
        headers.insert(
            "Editor-Plugin-Version",
            reqwest::header::HeaderValue::from_static("copilot-chat/0.31.0"),
        );
        headers.insert(
            "Copilot-Integration-Id",
            reqwest::header::HeaderValue::from_static("vscode-chat"),
        );
        headers.insert(
            "User-Agent",
            reqwest::header::HeaderValue::from_static("GitHubCopilotChat/0.31.0"),
        );
    }
    headers.insert(
        "Openai-Intent",
        reqwest::header::HeaderValue::from_static("conversation-panel"),
    );
    let initiator = initiator(body);
    headers.insert(
        "X-Initiator",
        reqwest::header::HeaderValue::from_static(initiator),
    );
    if let Ok(body) = serde_json::to_string(body)
        && ["image_url", "input_image", "\"type\":\"image\""]
            .iter()
            .any(|needle| body.contains(needle))
    {
        headers.insert(
            "Copilot-Vision-Request",
            reqwest::header::HeaderValue::from_static("true"),
        );
    }
    Ok(headers)
}

fn initiator(body: &Value) -> &'static str {
    if let Some(messages) = body.get("messages").and_then(Value::as_array)
        && let Some(last) = messages.last()
    {
        let role = last.get("role").and_then(Value::as_str).unwrap_or_default();
        if role == "user"
            && let Some(content) = last.get("content").and_then(Value::as_array)
            && !content.is_empty()
            && content
                .iter()
                .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            return "agent";
        }
        return if role == "user" { "user" } else { "agent" };
    }
    if let Some(input) = body.get("input") {
        if input.is_string() {
            return "user";
        }
        if let Some(items) = input.as_array()
            && let Some(role) = items
                .last()
                .and_then(|item| item.get("role"))
                .and_then(Value::as_str)
        {
            return if role == "user" { "user" } else { "agent" };
        }
    }
    "agent"
}

pub(crate) async fn refresh_models(account: &Account) -> Result<usize> {
    let client = crate::netproxy::builder()
        .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(8))
        .build()
        .context("create Copilot models client")?;
    let session = session(&client, account).await?;
    let base = if session.api_endpoint.is_empty() {
        DEFAULT_API
    } else {
        session.api_endpoint.trim_end_matches('/')
    };
    let mut response = client
        .get(format!("{base}/models"))
        .headers(upstream_headers(&session, &Value::Null)?)
        .send()
        .await
        .context("request Copilot model list")?;
    ensure!(
        response.status().is_success(),
        "Copilot model list request failed"
    );
    let contents = read_limited(&mut response, MAX_MODEL_RESPONSE_BYTES).await?;
    let list: ModelResponse =
        serde_json::from_slice(&contents).context("parse Copilot model list")?;
    let mut pending = HashSet::new();
    let models = list
        .data
        .into_iter()
        .filter_map(parse_model)
        .map(|(model, needs_acceptance)| {
            if needs_acceptance {
                pending.insert(model.id.clone());
            }
            model
        })
        .collect::<Vec<_>>();
    ensure!(
        !models.is_empty(),
        "Copilot lists no available chat models for this account"
    );
    let count = models.len();
    PENDING_TERMS
        .lock()
        .await
        .insert(account.github_token.clone(), pending);
    crate::catalog::save_live("copilot", DEFAULT_API, models)?;
    Ok(count)
}

pub(crate) async fn accept_model(
    client: &Client,
    account: &Account,
    session: &Session,
    model: &str,
) {
    let known = PENDING_TERMS
        .lock()
        .await
        .contains_key(&account.github_token);
    if !known {
        let _ = refresh_models(account).await;
    }
    let pending = PENDING_TERMS
        .lock()
        .await
        .get(&account.github_token)
        .is_some_and(|models| models.contains(model));
    if !pending {
        return;
    }

    let base = if session.api_endpoint.is_empty() {
        DEFAULT_API
    } else {
        session.api_endpoint.trim_end_matches('/')
    };
    let Ok(mut url) = url::Url::parse(base) else {
        return;
    };
    let Ok(mut path) = url.path_segments_mut() else {
        return;
    };
    path.pop_if_empty()
        .push("models")
        .push(model)
        .push("policy");
    drop(path);
    let Ok(headers) = upstream_headers(session, &Value::Null) else {
        return;
    };
    let Ok(response) = client
        .post(url)
        .headers(headers)
        .body(r#"{"state":"enabled"}"#)
        .send()
        .await
    else {
        return;
    };
    if response.status().is_success()
        && let Some(models) = PENDING_TERMS.lock().await.get_mut(&account.github_token)
    {
        models.remove(model);
    }
}

fn parse_model(model: CopilotModel) -> Option<(crate::catalog::Model, bool)> {
    if model.id.is_empty()
        || model.capabilities.kind != "chat"
        || internal_model(&model.id)
        || model.vendor == "Experimental"
        || (!model.model_picker_enabled && model.model_picker_category.is_empty())
    {
        return None;
    }
    let needs_acceptance = match model.policy.as_ref() {
        Some(policy) if policy.state == "enabled" => false,
        None if model.model_picker_enabled => false,
        Some(policy) if !policy.terms.is_empty() => true,
        _ => return None,
    };
    let mut apis = model
        .supported_endpoints
        .into_iter()
        .filter_map(|endpoint| match endpoint.as_str() {
            "/chat/completions" => Some("chat".to_owned()),
            "/responses" => Some("responses".to_owned()),
            "/v1/messages" => Some("anthropic".to_owned()),
            _ => None,
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if apis.is_empty() {
        return None;
    }
    apis.sort();
    Some((
        crate::catalog::Model {
            id: model.id,
            name: model.name,
            provider: model.vendor,
            efforts: model.capabilities.supports.reasoning_effort,
            apis,
            ..crate::catalog::Model::default()
        },
        needs_acceptance,
    ))
}

fn internal_model(id: &str) -> bool {
    ["copilot-search", "exec-agent", "trajectory"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
        || ["-secondary", "-tertiary", "-4th", "-free-auto"]
            .iter()
            .any(|suffix| id.ends_with(suffix))
}

async fn read_limited(response: &mut reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut contents = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read Copilot response")? {
        ensure!(
            contents.len().saturating_add(chunk.len()) <= limit,
            "Copilot response exceeds the size limit"
        );
        contents.extend_from_slice(&chunk);
    }
    Ok(contents)
}
