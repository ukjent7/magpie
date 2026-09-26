use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{AccountRow, AccountWindow, SavedLogin, read_saved_logins, write_saved_logins};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const MAX_RESPONSE_BYTES: usize = 1 << 20;
const CACHE_TTL: Duration = Duration::from_secs(60);

static QUOTA_CACHE: LazyLock<Mutex<HashMap<String, CachedQuota>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct CachedQuota {
    fetched_at: Instant,
    quota: AccountQuota,
}

#[derive(Clone, Default)]
struct AccountQuota {
    windows: Vec<AccountWindow>,
    error: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct UsageResponse {
    five_hour: Option<Snapshot>,
    seven_day: Option<Snapshot>,
    seven_day_opus: Option<Snapshot>,
    seven_day_sonnet: Option<Snapshot>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Snapshot {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RefreshResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

pub(super) async fn refresh(rows: &mut [AccountRow]) {
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for row in rows.iter_mut().filter(|row| row.agent == "claude") {
        let Some(auth) = row.auth.clone() else {
            row.error = Some("saved Claude Code sign-in has no credentials".to_owned());
            continue;
        };
        let key = account_key(&row.user, row.profile.as_ref());
        if seen.insert(key.clone()) {
            jobs.push((key, row.active, auth));
        }
    }
    if jobs.is_empty() {
        return;
    }

    let client = match crate::netproxy::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            for row in rows.iter_mut().filter(|row| row.agent == "claude") {
                row.error = Some(format!("create Claude usage client: {error:#}"));
            }
            return;
        }
    };
    let futures = jobs.into_iter().map(|(key, active, auth)| {
        let client = &client;
        async move {
            let (quota, refreshed_auth) = cached_quota(client, &key, auth, active).await;
            (key, active, quota, refreshed_auth)
        }
    });

    let mut saved_updates = HashMap::new();
    let mut refreshed_auths = HashMap::new();
    let mut persistence_errors = HashMap::new();
    for (key, active, quota, refreshed_auth) in join_all(futures).await {
        if let Some(auth) = refreshed_auth {
            refreshed_auths.insert(key.clone(), auth.clone());
            if active {
                if let Err(error) = super::claude_identity::install_login(&auth, None) {
                    persistence_errors.insert(key.clone(), error.to_string());
                }
            } else {
                saved_updates.insert(key.clone(), auth);
            }
        }
        for row in rows.iter_mut().filter(|row| {
            row.agent == "claude" && account_key(&row.user, row.profile.as_ref()) == key
        }) {
            if let Some(auth) = refreshed_auths.get(&key) {
                row.auth = Some(auth.clone());
            }
            row.windows.clone_from(&quota.windows);
            row.error = quota
                .error
                .clone()
                .or_else(|| persistence_errors.get(&key).cloned());
        }
    }

    if !saved_updates.is_empty()
        && let Err(error) = persist_saved_auth(&saved_updates)
    {
        for row in rows.iter_mut().filter(|row| {
            row.agent == "claude"
                && saved_updates.contains_key(&account_key(&row.user, row.profile.as_ref()))
        }) {
            row.error = Some(error.to_string());
        }
    }
}

async fn cached_quota(
    client: &reqwest::Client,
    key: &str,
    mut auth: Value,
    active: bool,
) -> (AccountQuota, Option<Value>) {
    let cached = {
        let cache = QUOTA_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key).cloned()
    };
    if let Some(cached) = &cached
        && cached.fetched_at.elapsed() < CACHE_TTL
    {
        return (cached.quota.clone(), None);
    }

    let (fresh, refreshed) = fetch_quota(client, &mut auth, active).await;
    let quota = if fresh.error.is_some() {
        cached.map_or(fresh, |cached| cached.quota)
    } else {
        fresh
    };
    QUOTA_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key.to_owned(),
            CachedQuota {
                fetched_at: Instant::now(),
                quota: quota.clone(),
            },
        );
    (quota, refreshed.then_some(auth))
}

async fn fetch_quota(
    client: &reqwest::Client,
    auth: &mut Value,
    active: bool,
) -> (AccountQuota, bool) {
    if !auth
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty())
    {
        return (auth_error("saved Claude Code sign-in is unreadable"), false);
    }

    let mut refreshed = false;
    if auth
        .get("claudeAiOauth")
        .is_none_or(|oauth| !token_is_fresh(oauth))
    {
        if active && let Some(latest) = super::claude_identity::latest_auth() {
            *auth = latest;
        }
        if auth
            .get("claudeAiOauth")
            .is_none_or(|oauth| !token_is_fresh(oauth))
        {
            match refresh_token(client, auth).await {
                Ok(()) => refreshed = true,
                Err(error) => return (auth_error(&format!("{error:#}")), false),
            }
        }
    }
    let token = auth
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if token.is_empty() {
        return (
            auth_error("Claude Code is signed out; run claude auth login"),
            refreshed,
        );
    }

    let response = client
        .get(CLAUDE_USAGE_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(reqwest::header::USER_AGENT, "magpie")
        .send()
        .await;
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            return (
                AccountQuota {
                    error: Some(format!("request Claude usage: {error}")),
                    ..AccountQuota::default()
                },
                refreshed,
            );
        }
    };
    let status = response.status();
    let body = match crate::codex::read_limited(&mut response, MAX_RESPONSE_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            return (
                AccountQuota {
                    error: Some(format!("read Claude response: {error:#}")),
                    ..AccountQuota::default()
                },
                refreshed,
            );
        }
    };
    if !status.is_success() {
        return (
            AccountQuota {
                error: Some(
                    status
                        .canonical_reason()
                        .unwrap_or("Claude usage request failed")
                        .to_owned(),
                ),
                ..AccountQuota::default()
            },
            refreshed,
        );
    }
    let data = match serde_json::from_slice::<UsageResponse>(&body) {
        Ok(data) => data,
        Err(error) => {
            return (
                AccountQuota {
                    error: Some(format!("parse Claude usage response: {error}")),
                    ..AccountQuota::default()
                },
                refreshed,
            );
        }
    };
    let windows = [
        ("5 hours", data.five_hour),
        ("7 days", data.seven_day),
        ("7 days · Opus", data.seven_day_opus),
        ("7 days · Sonnet", data.seven_day_sonnet),
    ]
    .into_iter()
    .filter_map(|(name, snapshot)| snapshot.map(|snapshot| snapshot.into_window(name)))
    .collect();
    (
        AccountQuota {
            windows,
            error: None,
        },
        refreshed,
    )
}

fn token_is_fresh(oauth: &Value) -> bool {
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if token.is_empty() {
        return false;
    }
    let expires_at = oauth
        .get("expiresAt")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    if expires_at == 0 {
        return true;
    }
    let expires_at_ms = if expires_at < 1_000_000_000_000 {
        expires_at.saturating_mul(1_000)
    } else {
        expires_at
    };
    let now_ms = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    expires_at_ms.saturating_sub(now_ms) > 5 * 60 * 1_000
}

async fn refresh_token(client: &reqwest::Client, auth: &mut Value) -> Result<()> {
    let oauth = auth
        .get("claudeAiOauth")
        .context("Claude credentials have no OAuth data")?;
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("Claude Code OAuth token expired; run claude auth login")?;
    let mut response = client
        .post(CLAUDE_TOKEN_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_CLIENT_ID,
        }))
        .send()
        .await
        .context("request Claude token refresh")?;
    let status = response.status();
    let body = crate::codex::read_limited(&mut response, MAX_RESPONSE_BYTES)
        .await
        .context("read Claude token refresh response")?;
    let fresh = match serde_json::from_slice::<RefreshResponse>(&body) {
        Ok(fresh) if status.is_success() && !fresh.access_token.is_empty() => fresh,
        _ => {
            anyhow::bail!("Claude Code is signed out (token refresh failed); run claude auth login")
        }
    };
    let oauth = auth
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .context("Claude credentials have no OAuth data")?;
    oauth.insert("accessToken".to_owned(), Value::String(fresh.access_token));
    if !fresh.refresh_token.is_empty() {
        oauth.insert(
            "refreshToken".to_owned(),
            Value::String(fresh.refresh_token),
        );
    }
    if fresh.expires_in > 0 {
        let now_ms = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        oauth.insert(
            "expiresAt".to_owned(),
            json!(now_ms.saturating_add(fresh.expires_in.saturating_mul(1_000))),
        );
    }
    Ok(())
}

fn auth_error(message: &str) -> AccountQuota {
    AccountQuota {
        error: Some(message.to_owned()),
        ..AccountQuota::default()
    }
}

impl Snapshot {
    fn into_window(self, name: &str) -> AccountWindow {
        let used = self.utilization.unwrap_or_default();
        let resets_at = self
            .resets_at
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
            .and_then(|value| value.format(&Rfc3339).ok());
        AccountWindow {
            name: name.to_owned(),
            used,
            remaining: (100.0 - used).max(0.0),
            resets_at,
            display: String::new(),
        }
    }
}

fn account_key(user: &str, profile: Option<&Value>) -> String {
    if let Some(profile) = profile {
        let email = profile
            .get("emailAddress")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let organization = profile
            .get("organizationUuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !email.is_empty() && !organization.is_empty() {
            return format!("claude/{}/{organization}", email.to_lowercase());
        }
    }
    format!("claude/{}", user.to_lowercase())
}

fn persist_saved_auth(updates: &HashMap<String, Value>) -> Result<()> {
    let mut logins: Vec<SavedLogin> = read_saved_logins()?;
    let mut changed = false;
    for login in &mut logins {
        if login.agent != "claude" {
            continue;
        }
        let key = account_key(&login.user, login.profile.as_ref());
        if let Some(auth) = updates.get(&key) {
            login.auth = Some(auth.clone());
            changed = true;
        }
    }
    if changed {
        write_saved_logins(&mut logins)?;
    }
    Ok(())
}
