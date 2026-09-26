use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::time::Instant;
use url::{Url, form_urlencoded};

use super::{
    SavedLogin, copilot_active_user, copilot_live_user, nonempty, oauth::open_browser,
    read_saved_logins, write_saved_logins,
};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_URL: &str = "https://github.com/login/device/code";
const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const USER_URL: &str = "https://api.github.com/user";
const COPILOT_USER_URL: &str = "https://api.github.com/copilot_internal/user";
const DEVICE_RESPONSE_LIMIT: usize = 1 << 20;

#[derive(Default, Deserialize)]
#[serde(default)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: i64,
    interval: i64,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct DeviceToken {
    access_token: String,
    error: String,
    error_description: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GitHubUser {
    login: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotUser {
    copilot_plan: String,
}

pub(super) async fn command(args: &[String]) -> Result<()> {
    let [action, agent] = args else {
        bail!("usage: magpie accounts add copilot");
    };
    ensure!(action.eq_ignore_ascii_case("add"), "expected add");
    ensure!(
        agent.eq_ignore_ascii_case("copilot"),
        "usage: magpie accounts add copilot"
    );
    add_copilot_account().await
}

async fn add_copilot_account() -> Result<()> {
    let client = crate::netproxy::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("create GitHub sign-in client")?;
    let device = request_device_code(&client).await?;
    ensure!(!device.device_code.is_empty(), "GitHub gave no device code");
    ensure!(!device.user_code.is_empty(), "GitHub gave no user code");

    let verification_uri = verification_url(&device.verification_uri);
    println!(
        "Open {verification_uri} and enter this code: {}",
        device.user_code
    );
    let _ = open_browser(&verification_uri);

    let access_token = poll_for_access_token(&client, &device).await?;
    let (user, plan) = copilot_account(&client, &access_token).await?;
    save_login(&user, &plan, &access_token)?;
    Ok(())
}

async fn request_device_code(client: &reqwest::Client) -> Result<DeviceCode> {
    let mut form = form_urlencoded::Serializer::new(String::new());
    form.append_pair("client_id", CLIENT_ID)
        .append_pair("scope", "read:user");
    let mut response = client
        .post(DEVICE_URL)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ACCEPT, "application/json")
        .body(form.finish())
        .send()
        .await
        .context("request GitHub device code")?;
    let status = response.status();
    let body = read_limited(&mut response).await?;
    ensure!(
        status.is_success(),
        "GitHub: {}",
        response_error(status, &body)
    );
    serde_json::from_slice(&body).context("parse GitHub device code")
}

async fn poll_for_access_token(client: &reqwest::Client, device: &DeviceCode) -> Result<String> {
    let expires_in = u64::try_from(device.expires_in.max(1)).unwrap_or(900);
    let mut interval = Duration::from_secs(u64::try_from(device.interval.max(1)).unwrap_or(5));
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let cancellation = tokio::signal::ctrl_c();
    tokio::pin!(cancellation);

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            result = &mut cancellation => {
                result.context("wait for cancellation")?;
                bail!("sign-in canceled");
            }
        }
        ensure!(Instant::now() < deadline, "the code expired; start again");

        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("client_id", CLIENT_ID)
            .append_pair("device_code", &device.device_code)
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
        let response = client
            .post(TOKEN_URL)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .body(form.finish())
            .send()
            .await;
        let Ok(mut response) = response else {
            continue;
        };
        let status = response.status();
        let body = read_limited(&mut response).await?;
        if !status.is_success() {
            bail!("GitHub: {}", response_error(status, &body));
        }
        let token: DeviceToken =
            serde_json::from_slice(&body).context("parse GitHub device token response")?;
        match token.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => {
                interval = interval.saturating_add(Duration::from_secs(5));
            }
            "expired_token" => bail!("the code expired; start again"),
            "access_denied" => bail!("the sign-in was declined on GitHub"),
            "" if !token.access_token.is_empty() => return Ok(token.access_token),
            "" => bail!("GitHub sent back no access token"),
            _ => bail!(
                "GitHub: {}",
                [token.error.clone(), token.error_description.clone()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(": ")
            ),
        }
    }
}

async fn copilot_account(client: &reqwest::Client, token: &str) -> Result<(String, String)> {
    let user: GitHubUser = get_json(client, USER_URL, token)
        .await
        .context("read GitHub account")?;
    ensure!(
        !user.login.is_empty(),
        "GitHub didn't say whose account this is"
    );

    let copilot = copilot_plan_for_user(client, token, &user.login).await?;
    Ok((user.login, copilot_plan_label(&copilot.copilot_plan)))
}

async fn copilot_plan_for_user(
    client: &reqwest::Client,
    token: &str,
    user: &str,
) -> Result<CopilotUser> {
    let mut response = client
        .get(COPILOT_USER_URL)
        .header(header::AUTHORIZATION, format!("token {token}"))
        .header(header::ACCEPT, "application/json")
        .header("Editor-Version", "vscode/1.104.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.31.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .header(header::USER_AGENT, "GitHubCopilotChat/0.31.0")
        .send()
        .await
        .context("request Copilot account")?;
    let status = response.status();
    let body = read_limited(&mut response).await?;
    if matches!(status.as_u16(), 401 | 403 | 404) {
        bail!("{user} has no Copilot subscription");
    }
    ensure!(
        status.is_success(),
        "Copilot: {}",
        response_error(status, &body)
    );
    serde_json::from_slice(&body).context("parse Copilot account")
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<T> {
    let mut response = client
        .get(url)
        .header(header::AUTHORIZATION, format!("token {token}"))
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "magpie")
        .send()
        .await
        .context("send GitHub request")?;
    let status = response.status();
    let body = read_limited(&mut response).await?;
    ensure!(
        status.is_success(),
        "GitHub HTTP {}: {}",
        status.as_u16(),
        response_error(status, &body)
    );
    serde_json::from_slice(&body).context("parse GitHub response")
}

async fn read_limited(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read GitHub response")? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= DEVICE_RESPONSE_LIMIT,
            "GitHub response exceeds the size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn response_error(status: StatusCode, body: &[u8]) -> String {
    let response = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    response
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| response.get("error_description").and_then(Value::as_str))
        .or_else(|| response.get("error").and_then(Value::as_str))
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "HTTP {} ({})",
                status.as_u16(),
                status.canonical_reason().unwrap_or("request failed")
            )
        })
}

fn verification_url(value: &str) -> String {
    Url::parse(value)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host_str() == Some("github.com"))
        .map(|url| url.to_string())
        .unwrap_or_else(|| "https://github.com/login/device".to_owned())
}

pub(super) fn copilot_plan_label(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "free" => "Free".to_owned(),
        "individual" => "Pro".to_owned(),
        "individual_pro" => "Pro+".to_owned(),
        "business" => "Business".to_owned(),
        "enterprise" => "Enterprise".to_owned(),
        _ => {
            let mut characters = value.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(characters).collect()
            })
        }
    }
}

fn save_login(user: &str, plan: &str, token: &str) -> Result<()> {
    let own_user = copilot_live_user();
    if own_user
        .as_deref()
        .is_some_and(|account| account.eq_ignore_ascii_case(user))
    {
        println!("✓ Copilot is already signed in as {user}");
        return Ok(());
    }

    let mut logins = read_saved_logins()?;
    let seen = Value::String(
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format account timestamp")?,
    );
    let active = copilot_active_user(&logins, own_user.as_deref());
    let existing = logins
        .iter()
        .position(|login| login.agent == "copilot" && login.user.eq_ignore_ascii_case(user));
    let mut login = SavedLogin {
        agent: "copilot".to_owned(),
        user: user.to_owned(),
        plan: nonempty(plan.to_owned()),
        seen: Some(seen),
        on: existing.is_none(),
        first: active.is_none(),
        auth: Some(json!({"oauth_token": token})),
        ..SavedLogin::default()
    };
    if let Some(index) = existing {
        login.on = logins[index].on;
        login.first = logins[index].first;
        login.extra = std::mem::take(&mut logins[index].extra);
        logins[index] = login;
    } else {
        logins.push(login);
    }
    write_saved_logins(&mut logins)?;
    println!("✓ added {user} · use it with: magpie accounts switch copilot {user}");
    Ok(())
}
