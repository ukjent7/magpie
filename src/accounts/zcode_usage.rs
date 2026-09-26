use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use futures_util::future::join_all;
use serde_json::Value;

use super::{AccountRow, AccountWindow};
use crate::provider::zcode::{self, ZcodeKey};

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

pub(super) async fn refresh(rows: &mut [AccountRow]) {
    let mut seen = HashSet::new();
    let mut jobs: Vec<(String, ZcodeKey)> = Vec::new();
    for row in rows.iter_mut().filter(|row| row.agent == "zcode") {
        let key = if row.auth.is_none() {
            // ZCode's own sign-in: its key comes from the credential store
            match zcode::own() {
                Some(own) => own.key,
                None => {
                    row.error = Some("no ZCode account in its credential store".to_owned());
                    continue;
                }
            }
        } else {
            match saved_key(row.auth.as_ref()) {
                Some(key) => key,
                None => {
                    row.error = Some("the saved ZCode sign-in has no API key".to_owned());
                    continue;
                }
            }
        };
        let identity = row.user.to_ascii_lowercase();
        if seen.insert(identity.clone()) {
            jobs.push((identity, key));
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
            for row in rows.iter_mut().filter(|row| row.agent == "zcode") {
                row.error = Some(format!("create ZCode usage client: {error:#}"));
            }
            return;
        }
    };
    let futures = jobs.into_iter().map(|(identity, key)| {
        let client = &client;
        async move {
            (
                identity.clone(),
                cached_quota(client, &identity, &key).await,
            )
        }
    });
    for (identity, quota) in join_all(futures).await {
        for row in rows
            .iter_mut()
            .filter(|row| row.agent == "zcode" && row.user.eq_ignore_ascii_case(&identity))
        {
            if let Some(plan) = &quota.plan {
                row.plan = Some(plan.clone());
            }
            row.windows.clone_from(&quota.windows);
            row.error = quota.error.clone();
        }
    }
}

async fn cached_quota(client: &reqwest::Client, identity: &str, key: &ZcodeKey) -> AccountQuota {
    let cached = {
        let cache = QUOTA_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(identity).cloned()
    };
    if let Some(cached) = &cached
        && cached.fetched_at.elapsed() < CACHE_TTL
    {
        return cached.quota.clone();
    }
    let fresh = fetch_quota(client, key).await;
    let quota = if fresh.error.is_some() {
        cached.map_or(fresh, |cached| cached.quota)
    } else {
        fresh
    };
    QUOTA_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            identity.to_owned(),
            CachedQuota {
                fetched_at: Instant::now(),
                quota: quota.clone(),
            },
        );
    quota
}

async fn fetch_quota(client: &reqwest::Client, key: &ZcodeKey) -> AccountQuota {
    match zcode::quota(client, key).await {
        Ok((plan, windows)) => AccountQuota {
            plan: (!plan.is_empty()).then_some(plan),
            windows: windows
                .into_iter()
                .map(|window| AccountWindow {
                    name: window.name,
                    used: window.used,
                    remaining: (100.0 - window.used).max(0.0),
                    resets_at: window.resets_at,
                    display: window.display,
                })
                .collect(),
            error: None,
        },
        Err(error) => AccountQuota {
            error: Some(format!("{error:#}")),
            ..AccountQuota::default()
        },
    }
}

// saved_key reads the coding plan key of one of magpie's saved accounts,
// as logins.json keeps it: {"apiKey": "<id>.<secret>", "base": …}.
fn saved_key(auth: Option<&Value>) -> Option<ZcodeKey> {
    let auth = auth?;
    let api_key = auth
        .get("apiKey")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if api_key.is_empty() {
        return None;
    }
    Some(ZcodeKey {
        api_key: api_key.to_owned(),
        base: auth
            .get("base")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_saved_key() {
        let key = saved_key(Some(
            &json!({"apiKey": "id.secret", "base": "https://x/api/anthropic"}),
        ))
        .unwrap();
        assert_eq!(key.api_key, "id.secret");
        assert_eq!(key.base, "https://x/api/anthropic");
        let key = saved_key(Some(&json!({"apiKey": "id.secret"}))).unwrap();
        assert_eq!(key.base, "");
        assert!(saved_key(Some(&json!({"apiKey": ""}))).is_none());
        assert!(saved_key(Some(&json!({}))).is_none());
        assert!(saved_key(None).is_none());
    }
}
