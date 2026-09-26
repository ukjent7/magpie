use std::{collections::HashMap, time::Duration};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail, ensure};
use axum::{
    Router,
    extract::{Query, State},
    response::Html,
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use url::{Url, form_urlencoded};

use super::{
    SavedLogin, live_codex_login, nonempty, read_saved_logins, same_login, upsert_login,
    write_saved_logins,
};

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDR: &str = "127.0.0.1:1455";
const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const MAX_TOKEN_RESPONSE_BYTES: usize = 1 << 20;

#[derive(Default, Deserialize)]
#[serde(default)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    sender: mpsc::UnboundedSender<Result<String>>,
}

pub(super) async fn command(args: &[String]) -> Result<()> {
    let [action, agent] = args else {
        bail!("usage: magpie accounts add codex");
    };
    ensure!(action.eq_ignore_ascii_case("add"), "expected add");
    ensure!(
        agent.eq_ignore_ascii_case("codex"),
        "Rust account sign-in currently supports Codex only"
    );
    add_codex_account().await
}

async fn add_codex_account() -> Result<()> {
    let verifier = random_token(48)?;
    let state = random_token(24)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let listener = listen_for_callback().await?;
    let authorize_url = authorization_url(&challenge, &state)?;

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let app = Router::new()
        .route("/auth/callback", get(oauth_callback))
        .with_state(CallbackState {
            expected_state: state,
            sender,
        });
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_receiver.await;
            })
            .await
            .context("serve Codex OAuth callback")
    });

    println!("Finish signing in in your browser. If it didn't open, go to:");
    println!("{authorize_url}");
    let _ = open_browser(authorize_url.as_str());

    let callback = tokio::select! {
        result = receiver.recv() => result.context("Codex sign-in callback channel closed")?,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("sign-in canceled")),
        _ = tokio::time::sleep(Duration::from_secs(10 * 60)) => {
            Err(anyhow!("the sign-in timed out; start it again"))
        }
    };
    let _ = shutdown_sender.send(());
    server
        .await
        .context("wait for Codex OAuth callback server")??;
    let code = callback?;

    let client = crate::netproxy::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("create ChatGPT sign-in client")?;
    let token = exchange_code(&client, &code, &verifier).await?;
    save_login(token).await
}

fn random_token(byte_count: usize) -> Result<String> {
    let mut bytes = vec![0; byte_count];
    getrandom::fill(&mut bytes).context("generate OAuth security token")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn authorization_url(challenge: &str, state: &str) -> Result<Url> {
    let mut url = Url::parse(AUTHORIZE_URL).context("parse ChatGPT sign-in URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", crate::codex::CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "codex_cli_rs");
    Ok(url)
}

async fn listen_for_callback() -> Result<TcpListener> {
    for attempt in 0..10 {
        match TcpListener::bind(CALLBACK_ADDR).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                if attempt == 0 {
                    cancel_existing_login().await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => {
                return Err(error).context("bind the Codex sign-in callback on port 1455");
            }
        }
    }
    bail!(
        "port 1455, where ChatGPT sends the sign-in back, is busy; close any other Codex sign-in and try again"
    )
}

async fn cancel_existing_login() {
    let Ok(client) = crate::netproxy::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return;
    };
    let _ = client.get("http://127.0.0.1:1455/cancel").send().await;
}

async fn oauth_callback(
    State(callback): State<CallbackState>,
    Query(parameters): Query<HashMap<String, String>>,
) -> Html<String> {
    if parameters.get("state") != Some(&callback.expected_state) {
        return sign_in_page(
            false,
            "This link isn't from magpie's sign-in",
            "Start it again from magpie.",
        );
    }

    if let Some(error) = parameters.get("error") {
        let message = parameters
            .get("error_description")
            .filter(|description| !description.is_empty())
            .cloned()
            .unwrap_or_else(|| error.clone());
        let _ = callback.sender.send(Err(anyhow!(message.clone())));
        return sign_in_page(false, "Sign-in didn't finish", &message);
    }

    let Some(code) = parameters.get("code").filter(|code| !code.is_empty()) else {
        let message = "ChatGPT sent back no authorization code";
        let _ = callback.sender.send(Err(anyhow!(message)));
        return sign_in_page(false, "Sign-in didn't finish", message);
    };
    let _ = callback.sender.send(Ok(code.clone()));
    sign_in_page(
        true,
        "Finishing sign-in",
        "Authorization was received. Return to Magpie to finish signing in.",
    )
}

fn sign_in_page(success: bool, title: &str, message: &str) -> Html<String> {
    let color = if success { "#16875d" } else { "#b42318" };
    Html(format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><body style=\"margin:0;background:#101114;color:#f5f5f5;font:16px system-ui;display:grid;min-height:100vh;place-items:center\"><main style=\"max-width:32rem;padding:2rem\"><h1 style=\"color:{color}\">{}</h1><p>{}</p></main></body></html>",
        escape_html(title),
        escape_html(title),
        escape_html(message)
    ))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let mut form = form_urlencoded::Serializer::new(String::new());
    form.append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("client_id", crate::codex::CLIENT_ID)
        .append_pair("code_verifier", verifier);
    let body = form.finish();
    let mut response = client
        .post(crate::codex::TOKEN_URL)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await
        .context("request ChatGPT sign-in token")?;
    let status = response.status();
    let contents = read_limited(&mut response).await?;
    if !status.is_success() {
        bail!("{}", token_error(status, &contents));
    }
    let token: TokenResponse =
        serde_json::from_slice(&contents).context("parse ChatGPT token response")?;
    ensure!(
        !token.access_token.is_empty()
            && !token.refresh_token.is_empty()
            && !token.id_token.is_empty(),
        "ChatGPT sent back no token"
    );
    Ok(token)
}

async fn read_limited(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    let mut contents = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read ChatGPT response")? {
        ensure!(
            contents.len().saturating_add(chunk.len()) <= MAX_TOKEN_RESPONSE_BYTES,
            "ChatGPT token response exceeds the size limit"
        );
        contents.extend_from_slice(&chunk);
    }
    Ok(contents)
}

fn token_error(status: StatusCode, body: &[u8]) -> String {
    let response = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let message = response
        .pointer("/error_description")
        .or(response.pointer("/detail"))
        .or(response.pointer("/message"))
        .and_then(Value::as_str)
        .or(response.pointer("/error/message").and_then(Value::as_str))
        .or(response.get("error").and_then(Value::as_str))
        .filter(|message| !message.is_empty())
        .unwrap_or(status.canonical_reason().unwrap_or("token endpoint error"));
    format!("the sign-in was refused ({}): {message}", status.as_u16())
}

async fn save_login(token: TokenResponse) -> Result<()> {
    let account_id = crate::codex::account_id_from_id_token(&token.id_token).unwrap_or_default();
    let last_refresh = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format token refresh time")?;
    let auth = json!({
        "OPENAI_API_KEY": Value::Null,
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": token.id_token,
            "access_token": token.access_token,
            "refresh_token": token.refresh_token,
            "account_id": account_id,
        },
        "last_refresh": last_refresh,
    });
    let (user, plan) = crate::codex::identity_from_auth(&auth)
        .context("ChatGPT didn't say which account signed in")?;
    let seen = Value::String(
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format account timestamp")?,
    );
    let mut login = SavedLogin {
        agent: "codex".to_owned(),
        user: user.clone(),
        plan: nonempty(plan),
        seen: Some(seen.clone()),
        on: false,
        auth: Some(auth.clone()),
        ..SavedLogin::default()
    };

    let current = live_codex_login()?;
    let using = current
        .as_ref()
        .is_none_or(|current| same_login(current, &login));
    let mut logins = read_saved_logins()?;
    if !using && let Some(mut current) = current {
        current.seen = Some(seen);
        upsert_login(&mut logins, current);
    }
    login.on = !using;
    upsert_login(&mut logins, login);
    write_saved_logins(&mut logins)?;

    if using {
        let auth_file = crate::codex::auth_file_path().context("cannot locate Codex auth.json")?;
        let mut contents = serde_json::to_vec_pretty(&auth).context("serialize Codex sign-in")?;
        contents.push(b'\n');
        crate::config::atomic_write_secret_for_settings(&auth_file, &contents)
            .with_context(|| format!("install Codex sign-in at {}", auth_file.display()))?;
        println!("✓ codex is signed in as {user}");
    } else {
        println!("✓ added {user} · use it with: magpie accounts switch codex {user}");
    }
    Ok(())
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = url;
        false
    }
}
