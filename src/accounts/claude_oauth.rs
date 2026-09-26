use std::{collections::HashMap, time::Duration};

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
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use url::Url;

use super::{
    SavedLogin, claude_identity, nonempty, oauth::open_browser, oauth::random_token,
    oauth::sign_in_page, read_saved_logins, same_login, upsert_login, write_saved_logins,
};

const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins";
const MAX_RESPONSE_BYTES: usize = 1 << 20;

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    sender: mpsc::UnboundedSender<Result<String>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    scope: String,
    account: TokenAccount,
    organization: TokenOrganization,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TokenAccount {
    uuid: String,
    email_address: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TokenOrganization {
    uuid: String,
    name: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ProfileResponse {
    account: ProfileAccount,
    organization: ProfileOrganization,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ProfileAccount {
    email: String,
    display_name: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ProfileOrganization {
    organization_type: String,
    rate_limit_tier: String,
    billing_type: String,
    has_extra_usage_enabled: bool,
}

pub(super) async fn command(args: &[String]) -> Result<()> {
    let [action, agent] = args else {
        bail!("usage: magpie accounts add claude");
    };
    ensure!(action.eq_ignore_ascii_case("add"), "expected add");
    ensure!(
        agent.eq_ignore_ascii_case("claude"),
        "usage: magpie accounts add claude"
    );
    add_claude_account().await
}

async fn add_claude_account() -> Result<()> {
    let verifier = random_token(48)?;
    let state = random_token(24)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind the Claude sign-in callback")?;
    let port = listener
        .local_addr()
        .context("read the Claude sign-in callback address")?
        .port();
    let redirect_uri = format!("http://localhost:{port}/callback");
    let authorize_url = authorization_url(&challenge, &state, &redirect_uri)?;

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let app = Router::new()
        .route("/callback", get(oauth_callback))
        .with_state(CallbackState {
            expected_state: state.clone(),
            sender,
        });
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_receiver.await;
            })
            .await
            .context("serve Claude OAuth callback")
    });

    println!("Finish signing in in your browser. If it didn't open, go to:");
    println!("{authorize_url}");
    let _ = open_browser(authorize_url.as_str());

    let callback = tokio::select! {
        result = receiver.recv() => result.context("Claude sign-in callback channel closed")?,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("sign-in canceled")),
        _ = tokio::time::sleep(Duration::from_secs(10 * 60)) => {
            Err(anyhow!("the sign-in timed out; start it again"))
        }
    };
    let _ = shutdown_sender.send(());
    server
        .await
        .context("wait for Claude OAuth callback server")??;
    let code = callback?;

    let client = crate::netproxy::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("create Claude sign-in client")?;
    let login = exchange_code(&client, &code, &verifier, &state, &redirect_uri).await?;
    save_login(login).await
}

fn authorization_url(challenge: &str, state: &str, redirect_uri: &str) -> Result<Url> {
    let mut url = Url::parse(AUTHORIZE_URL).context("parse Claude sign-in URL")?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url)
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
        let message = "Claude sent back no authorization code";
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

async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<SavedLogin> {
    let mut response = client
        .post(TOKEN_URL)
        .header(header::ACCEPT, "application/json")
        .json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
            "client_id": CLIENT_ID,
            "code_verifier": verifier,
            "state": state,
        }))
        .send()
        .await
        .context("request Claude sign-in token")?;
    let status = response.status();
    let body = read_limited(&mut response).await?;
    if !status.is_success() {
        bail!("{}", token_error(status, &body));
    }
    let token =
        serde_json::from_slice::<TokenResponse>(&body).context("parse Claude token response")?;
    ensure!(
        !token.access_token.is_empty() && !token.refresh_token.is_empty(),
        "Claude sent back no token"
    );

    let profile = fetch_profile(client, &token.access_token).await.ok();
    let email = profile
        .as_ref()
        .map(|profile| profile.account.email.as_str())
        .filter(|email| !email.is_empty())
        .unwrap_or(&token.account.email_address)
        .to_owned();
    ensure!(
        !email.is_empty(),
        "Claude didn't say which account signed in"
    );

    let plan = profile
        .as_ref()
        .map(|profile| plan_name(&profile.organization.organization_type))
        .unwrap_or_default();
    let mut account = Map::new();
    account.insert("accountUuid".to_owned(), json!(token.account.uuid));
    account.insert("emailAddress".to_owned(), json!(email.clone()));
    account.insert(
        "organizationUuid".to_owned(),
        json!(token.organization.uuid),
    );
    if !token.organization.name.is_empty() {
        account.insert(
            "organizationName".to_owned(),
            json!(token.organization.name),
        );
    }
    let mut oauth = Map::new();
    oauth.insert("accessToken".to_owned(), json!(token.access_token));
    oauth.insert("refreshToken".to_owned(), json!(token.refresh_token));
    oauth.insert("expiresAt".to_owned(), json!(expires_at(token.expires_in)));
    oauth.insert(
        "scopes".to_owned(),
        json!(token.scope.split_whitespace().collect::<Vec<_>>()),
    );
    if !plan.is_empty() {
        oauth.insert("subscriptionType".to_owned(), json!(plan.clone()));
    }
    if let Some(profile) = profile {
        if !profile.organization.rate_limit_tier.is_empty() {
            oauth.insert(
                "rateLimitTier".to_owned(),
                json!(profile.organization.rate_limit_tier),
            );
        }
        if !profile.account.display_name.is_empty() {
            account.insert(
                "displayName".to_owned(),
                json!(profile.account.display_name),
            );
        }
        if !profile.organization.billing_type.is_empty() {
            account.insert(
                "billingType".to_owned(),
                json!(profile.organization.billing_type),
            );
        }
        account.insert(
            "hasExtraUsageEnabled".to_owned(),
            json!(profile.organization.has_extra_usage_enabled),
        );
    }
    let profile = Value::Object(account);
    let user = claude_identity::claude_user(&email, &plan, Some(&profile));
    let auth = json!({"claudeAiOauth": oauth});
    Ok(SavedLogin {
        agent: "claude".to_owned(),
        user,
        plan: nonempty(plan),
        profile: Some(profile),
        auth: Some(auth),
        ..SavedLogin::default()
    })
}

async fn fetch_profile(client: &reqwest::Client, token: &str) -> Result<ProfileResponse> {
    let mut response = client
        .get(PROFILE_URL)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .context("request Claude profile")?;
    ensure!(
        response.status() == StatusCode::OK,
        "Claude profile request failed"
    );
    let body = read_limited(&mut response).await?;
    serde_json::from_slice(&body).context("parse Claude profile")
}

fn plan_name(organization_type: &str) -> String {
    match organization_type {
        "claude_max" => "max",
        "claude_pro" => "pro",
        "claude_enterprise" => "enterprise",
        "claude_team" => "team",
        _ => "",
    }
    .to_owned()
}

fn expires_at(expires_in: i64) -> i64 {
    let now_ms = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    now_ms.saturating_add(expires_in.saturating_mul(1_000))
}

async fn read_limited(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read Claude response")? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Claude response exceeds the size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

async fn save_login(mut login: SavedLogin) -> Result<()> {
    let seen = Value::String(
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format account timestamp")?,
    );
    login.seen = Some(seen.clone());
    let current = claude_identity::live_login().await;
    let using = current
        .as_ref()
        .is_none_or(|current| same_login(current, &login));
    let mut logins = read_saved_logins()?;
    if !using && let Some(mut current) = current {
        current.seen = Some(seen);
        upsert_login(&mut logins, current);
    }
    login.on = !using;
    let user = login.user.clone();
    let auth = login
        .auth
        .clone()
        .context("Claude sign-in returned no credentials")?;
    let profile = login.profile.clone();
    upsert_login(&mut logins, login);
    write_saved_logins(&mut logins)?;

    if using {
        claude_identity::install_login(&auth, profile.as_ref())?;
        println!("✓ claude is signed in as {user}");
    } else {
        println!("✓ added {user} · use it with: magpie accounts switch claude {user}");
    }
    Ok(())
}
