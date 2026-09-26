use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use futures_util::future::join_all;
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{AccountRow, AccountWindow};

const COPILOT_USER_URL: &str = "https://api.github.com/copilot_internal/user";
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
    copilot_plan: Option<String>,
    quota_snapshots: HashMap<String, Snapshot>,
    quota_reset_date_utc: Option<String>,
    quota_reset_date: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Snapshot {
    has_quota: Option<bool>,
    entitlement: Option<f64>,
    quota_remaining: Option<f64>,
}

pub(super) async fn refresh(rows: &mut [AccountRow]) {
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for row in rows.iter_mut().filter(|row| row.agent == "copilot") {
        let Some(token) = row.copilot_token.clone() else {
            row.error = Some("Copilot account has no available credentials".to_owned());
            continue;
        };
        let key = row.user.to_ascii_lowercase();
        if seen.insert(key.clone()) {
            jobs.push((key, token));
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
            for row in rows.iter_mut().filter(|row| row.agent == "copilot") {
                row.error = Some(format!("create Copilot usage client: {error:#}"));
            }
            return;
        }
    };
    let futures = jobs.into_iter().map(|(key, token)| {
        let client = &client;
        async move { (key.clone(), cached_quota(client, &key, &token).await) }
    });
    for (key, quota) in join_all(futures).await {
        for row in rows
            .iter_mut()
            .filter(|row| row.agent == "copilot" && row.user.eq_ignore_ascii_case(&key))
        {
            if quota.plan.is_some() {
                row.plan = quota.plan.clone();
            }
            row.windows.clone_from(&quota.windows);
            row.error = quota.error.clone();
        }
    }
}

async fn cached_quota(client: &reqwest::Client, key: &str, token: &str) -> AccountQuota {
    let cached = {
        let cache = QUOTA_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key).cloned()
    };
    if let Some(cached) = &cached
        && cached.fetched_at.elapsed() < CACHE_TTL
    {
        return cached.quota.clone();
    }
    let fresh = fetch_quota(client, token).await;
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
    quota
}

async fn fetch_quota(client: &reqwest::Client, token: &str) -> AccountQuota {
    let mut response = match client
        .get(COPILOT_USER_URL)
        .header(reqwest::header::AUTHORIZATION, format!("token {token}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .header("Editor-Version", "vscode/1.104.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.31.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("User-Agent", "GitHubCopilotChat/0.31.0")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return AccountQuota {
                error: Some(format!("request Copilot usage: {error}")),
                ..AccountQuota::default()
            };
        }
    };
    let status = response.status();
    let mut body = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                return AccountQuota {
                    error: Some(format!("read Copilot response: {error}")),
                    ..AccountQuota::default()
                };
            }
        };
        if body.len().saturating_add(chunk.len()) > 1 << 20 {
            return AccountQuota {
                error: Some("Copilot response exceeds the size limit".to_owned()),
                ..AccountQuota::default()
            };
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return AccountQuota {
            error: Some(
                status
                    .canonical_reason()
                    .unwrap_or("Copilot usage request failed")
                    .to_owned(),
            ),
            ..AccountQuota::default()
        };
    }
    let data = match serde_json::from_slice::<UsageResponse>(&body) {
        Ok(data) => data,
        Err(error) => {
            return AccountQuota {
                error: Some(format!("parse Copilot usage response: {error}")),
                ..AccountQuota::default()
            };
        }
    };
    let resets_at = parse_reset_date(
        data.quota_reset_date_utc.as_deref(),
        data.quota_reset_date.as_deref(),
    );
    let metrics = [
        ("chat", "Chat requests"),
        ("completions", "Completions"),
        ("premium_interactions", "Premium requests"),
    ];
    let windows = metrics
        .into_iter()
        .filter_map(|(id, name)| {
            let snapshot = data.quota_snapshots.get(id)?;
            if snapshot.has_quota != Some(true) {
                return None;
            }
            let entitlement = snapshot.entitlement.filter(|value| *value > 0.0)?;
            let remaining = snapshot.quota_remaining.unwrap_or_default();
            let used = entitlement - remaining;
            let used_percent = 100.0 * used / entitlement;
            Some(AccountWindow {
                name: name.to_owned(),
                used: used_percent,
                remaining: (100.0 - used_percent).max(0.0),
                resets_at: resets_at.clone(),
                display: format!("{} / {}", compact_number(used), compact_number(entitlement)),
            })
        })
        .collect();
    AccountQuota {
        plan: data
            .copilot_plan
            .filter(|plan| !plan.is_empty())
            .map(|plan| super::copilot_oauth::copilot_plan_label(&plan)),
        windows,
        error: None,
    }
}

fn parse_reset_date(date_time: Option<&str>, date: Option<&str>) -> Option<String> {
    date_time
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .or_else(|| {
            date.and_then(|value| {
                OffsetDateTime::parse(&format!("{value}T00:00:00Z"), &Rfc3339).ok()
            })
        })
        .and_then(|value| value.format(&Rfc3339).ok())
}

fn compact_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}
