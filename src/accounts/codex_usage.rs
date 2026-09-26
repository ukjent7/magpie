use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::Value;
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};

use super::{AccountRow, AccountWindow, read_saved_logins, write_saved_logins};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
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
    plan: Option<String>,
    windows: Vec<AccountWindow>,
    error: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RateLimit {
    primary_window: Option<QuotaWindow>,
    secondary_window: Option<QuotaWindow>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct QuotaWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_at: Option<i64>,
    reset_after_seconds: Option<i64>,
}

pub(super) async fn refresh(rows: &mut [AccountRow]) {
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for row in rows.iter_mut().filter(|row| row.agent == "codex") {
        let Some(auth) = row.auth.clone() else {
            row.error = Some("saved Codex sign-in has no credentials".to_owned());
            continue;
        };
        let key = account_key(&row.user, &auth);
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
            for row in rows.iter_mut().filter(|row| row.agent == "codex") {
                row.error = Some(format!("create Codex usage client: {error:#}"));
            }
            return;
        }
    };
    let futures = jobs.into_iter().map(|(key, active, auth)| {
        let client = &client;
        async move {
            let (quota, refreshed_auth) = cached_quota(client, &key, auth).await;
            (key, active, quota, refreshed_auth)
        }
    });

    let mut saved_updates = HashMap::new();
    for (key, active, quota, refreshed_auth) in join_all(futures).await {
        let persistence_error = if let Some(auth) = refreshed_auth.as_ref() {
            if active {
                persist_active_auth(auth)
                    .await
                    .err()
                    .map(|error| error.to_string())
            } else {
                saved_updates.insert(key.clone(), auth.clone());
                None
            }
        } else {
            None
        };
        for row in rows
            .iter_mut()
            .filter(|row| row.agent == "codex" && account_key_for_row(row) == key)
        {
            if let Some(auth) = saved_updates.get(&key) {
                row.auth = Some(auth.clone());
            } else if active && let Some(auth) = refreshed_auth.as_ref() {
                row.auth = Some(auth.clone());
            }
            if quota.plan.is_some() {
                row.plan = quota.plan.clone();
            }
            row.windows.clone_from(&quota.windows);
            row.error = quota.error.clone().or_else(|| persistence_error.clone());
        }
    }

    if !saved_updates.is_empty()
        && let Err(error) = persist_saved_auth(&saved_updates)
    {
        for row in rows.iter_mut().filter(|row| {
            row.agent == "codex" && saved_updates.contains_key(&account_key_for_row(row))
        }) {
            row.error = Some(error.to_string());
        }
    }
}

async fn cached_quota(
    client: &reqwest::Client,
    key: &str,
    auth: Value,
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

    let (fresh, refreshed_auth) = fetch_quota(client, auth).await;
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
    (quota, refreshed_auth)
}

async fn fetch_quota(client: &reqwest::Client, mut auth: Value) -> (AccountQuota, Option<Value>) {
    let (credentials, refreshed) =
        match crate::codex::credentials_from_auth(client, &mut auth).await {
            Ok(result) => result,
            Err(error) => {
                return (
                    AccountQuota {
                        error: Some(format!("{error:#}")),
                        ..AccountQuota::default()
                    },
                    None,
                );
            }
        };
    let refreshed_auth = refreshed.then_some(auth.clone());
    let mut request = client
        .get(USAGE_URL)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", credentials.access_token),
        )
        .header(reqwest::header::ACCEPT, "application/json");
    if !credentials.account_id.is_empty() {
        request = request.header("chatgpt-account-id", credentials.account_id);
    }
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return (
                AccountQuota {
                    error: Some(format!("request Codex usage: {error}")),
                    ..AccountQuota::default()
                },
                refreshed_auth,
            );
        }
    };
    let status = response.status();
    let body = match crate::codex::read_limited(&mut response, 1 << 20).await {
        Ok(body) => body,
        Err(error) => {
            return (
                AccountQuota {
                    error: Some(format!("{error:#}")),
                    ..AccountQuota::default()
                },
                refreshed_auth,
            );
        }
    };
    if !status.is_success() {
        return (
            AccountQuota {
                error: Some(
                    status
                        .canonical_reason()
                        .unwrap_or("Codex usage request failed")
                        .to_owned(),
                ),
                ..AccountQuota::default()
            },
            refreshed_auth,
        );
    }

    let data = match serde_json::from_slice::<UsageResponse>(&body) {
        Ok(data) => data,
        Err(error) => {
            return (
                AccountQuota {
                    error: Some(format!("parse Codex usage response: {error}")),
                    ..AccountQuota::default()
                },
                refreshed_auth,
            );
        }
    };
    let now = OffsetDateTime::now_utc();
    let plan = data.plan_type.filter(|plan| !plan.is_empty());
    let rate_limit = data.rate_limit.unwrap_or_default();
    let windows = rate_limit
        .primary_window
        .into_iter()
        .chain(rate_limit.secondary_window)
        .map(|window| window.into_account_window(now))
        .collect();
    (
        AccountQuota {
            plan,
            windows,
            error: None,
        },
        refreshed_auth,
    )
}

impl QuotaWindow {
    fn into_account_window(self, now: OffsetDateTime) -> AccountWindow {
        let limit_window_seconds = self.limit_window_seconds.unwrap_or_default();
        let used = self.used_percent.unwrap_or_default();
        let name = match limit_window_seconds {
            seconds if seconds > 0 && seconds % 86_400 == 0 => {
                format!("{} days", seconds / 86_400)
            }
            seconds if seconds > 0 && seconds % 3_600 == 0 => {
                format!("{} hours", seconds / 3_600)
            }
            _ => "Allowance".to_owned(),
        };
        let reset_at = self.reset_at.unwrap_or_default();
        let reset_after_seconds = self.reset_after_seconds.unwrap_or_default();
        let resets_at = if reset_at > 0 {
            OffsetDateTime::from_unix_timestamp(reset_at).ok()
        } else if reset_after_seconds > 0 {
            now.checked_add(TimeDuration::seconds(reset_after_seconds))
        } else {
            None
        }
        .and_then(|reset| reset.format(&Rfc3339).ok());
        AccountWindow {
            name,
            used,
            remaining: (100.0 - used).max(0.0),
            resets_at,
            display: String::new(),
        }
    }
}

async fn persist_active_auth(auth: &Value) -> Result<()> {
    let path = crate::codex::auth_file_path().context("cannot locate Codex auth.json")?;
    let mut bytes = serde_json::to_vec_pretty(auth).context("serialize refreshed Codex sign-in")?;
    bytes.push(b'\n');
    tokio::task::spawn_blocking(move || {
        crate::config::atomic_write_secret_for_settings(&path, &bytes)
    })
    .await
    .context("save refreshed Codex sign-in")??;
    Ok(())
}

fn persist_saved_auth(updates: &HashMap<String, Value>) -> Result<()> {
    let mut logins = read_saved_logins()?;
    let mut changed = false;
    for login in &mut logins {
        if login.agent != "codex" {
            continue;
        }
        let Some(auth) = login.auth.as_ref() else {
            continue;
        };
        if let Some(updated) = updates.get(&account_key(&login.user, auth)) {
            login.auth = Some(updated.clone());
            changed = true;
        }
    }
    if changed {
        write_saved_logins(&mut logins)?;
    }
    Ok(())
}

fn account_key_for_row(row: &AccountRow) -> String {
    row.auth
        .as_ref()
        .map(|auth| account_key(&row.user, auth))
        .unwrap_or_else(|| format!("codex/{}", row.user.to_lowercase()))
}

fn account_key(user: &str, auth: &Value) -> String {
    crate::codex::account_id_from_auth(auth)
        .map(|id| format!("codex/{id}"))
        .unwrap_or_else(|| format!("codex/{}", user.to_lowercase()))
}
