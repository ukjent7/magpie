use std::{
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::future::join_all;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::Provider;
use super::zcode::millis_to_rfc3339;

// A plan bought with an API key — Zhipu's GLM Coding Plan (and Z.ai's), and
// OpenCode Go — has windows of allowance like a subscription's, which the
// vendor tells to the key: the Usage page shows them beside the
// subscriptions'.

const MAX_RESPONSE_BYTES: usize = 1 << 20;
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) struct PlanWindow {
    pub(crate) name: String,
    pub(crate) used: f64,
    pub(crate) resets_at: Option<String>,
    // how long the window runs, zero when not known
    pub(crate) span_secs: u64,
    // aside when using it up doesn't stop the models
    pub(crate) aside: bool,
}

#[derive(Clone)]
pub(crate) struct PlanSource {
    pub(crate) url: String,
    bearer: bool, // OpenCode Go takes Bearer; Zhipu the bare key
    // sure is set when the provider is a plan and not just the vendor: a
    // pay-as-you-go GLM key has no windows to tell, and saying so on a card
    // would only be noise
    sure: bool,
}

pub(crate) fn plan_quota_source_of(provider: &Provider) -> Option<PlanSource> {
    for base in [&provider.chat, &provider.responses, &provider.anthropic] {
        let coding = base.contains("/api/coding/");
        match super::host_of(base).as_str() {
            "open.bigmodel.cn" => {
                return Some(PlanSource {
                    url: "https://open.bigmodel.cn/api/monitor/usage/quota/limit".to_owned(),
                    bearer: false,
                    sure: coding,
                });
            }
            "api.z.ai" => {
                return Some(PlanSource {
                    url: "https://api.z.ai/api/monitor/usage/quota/limit".to_owned(),
                    bearer: false,
                    sure: coding,
                });
            }
            "opencode.ai" => {
                let base = base.trim_end_matches('/');
                if base.ends_with("/zen/go") || base.contains("/zen/go/") {
                    return Some(PlanSource {
                        url: "https://opencode.ai/zen/go/v1/usage".to_owned(),
                        bearer: true,
                        sure: true,
                    });
                }
            }
            _ => {}
        }
    }
    None
}

// read_zhipu_plan reads
//
//	{"success":true,"data":{"level":"pro","limits":[
//	  {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12,"nextResetTime":1758800000000},
//	  {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":40,"nextResetTime":…},
//	  {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":3,"nextResetTime":…}]}}
//
// unit 3 is hours and 6 weeks (number 1 or 7 have both been seen for the
// week); TIME_LIMIT is the month's MCP tool calls, which don't stop the
// models.
pub(crate) fn read_zhipu_plan(bytes: &[u8]) -> Result<(String, Vec<PlanWindow>)> {
    let value: Value = serde_json::from_slice(bytes).context("parse the plan reply")?;
    let success = value.get("success").and_then(Value::as_bool);
    if success == Some(false) || value.get("data").is_none_or(Value::is_null) {
        let message = value.get("msg").and_then(Value::as_str).unwrap_or_default();
        bail!(
            "{}",
            if message.is_empty() {
                "no plan in the reply"
            } else {
                message
            }
        );
    }
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let mut windows = Vec::new();
    for limit in data
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let limit_type = limit
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let unit = limit.get("unit").and_then(Value::as_i64).unwrap_or(0);
        let number = limit.get("number").and_then(Value::as_i64).unwrap_or(0);
        let used = limit
            .get("percentage")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let resets_at = millis_to_rfc3339(
            limit
                .get("nextResetTime")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        );
        let window = if limit_type.eq_ignore_ascii_case("TIME_LIMIT") {
            PlanWindow {
                name: "MCP · Month".to_owned(),
                used,
                resets_at,
                span_secs: 0,
                aside: true,
            }
        } else if unit == 3 {
            let hours = number.max(1) as u64;
            PlanWindow {
                name: format!("{hours} hours"),
                used,
                resets_at,
                span_secs: hours * 3600,
                aside: false,
            }
        } else if unit == 6 {
            PlanWindow {
                name: "7 days".to_owned(),
                used,
                resets_at,
                span_secs: 7 * 86_400,
                aside: false,
            }
        } else {
            PlanWindow {
                name: "Allowance".to_owned(),
                used,
                resets_at,
                span_secs: 0,
                aside: false,
            }
        };
        windows.push(window);
    }
    let plan = data
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok((plan, windows))
}

// read_opencode_go reads
//
//	{"usage":{"rolling":{"status":"ok","percent":37,"resetsAt":"2026-08-26T14:12:03.000Z"},
//	          "weekly":{…},"monthly":{"status":"rate-limited","percent":100,…}}}
//
// A window at 0% gives now plus its length as resetsAt, a time nothing
// happens at, so it is left out.
pub(crate) fn read_opencode_go(bytes: &[u8]) -> Result<(String, Vec<PlanWindow>)> {
    let value: Value = serde_json::from_slice(bytes).context("parse the usage reply")?;
    let usage = value
        .get("usage")
        .and_then(Value::as_object)
        .context("no usage in the reply")?;
    let mut windows = Vec::new();
    for (key, name, span_secs) in [
        ("rolling", "5 hours", 5 * 3600_u64),
        ("weekly", "7 days", 7 * 86_400),
        ("monthly", "Month", 0),
    ] {
        let Some(window) = usage.get(key) else {
            continue;
        };
        let Some(used) = window.get("percent").and_then(Value::as_f64) else {
            continue;
        };
        let resets_at = window
            .get("resetsAt")
            .and_then(Value::as_str)
            .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
            .filter(|_| used > 0.0)
            .and_then(|at| at.format(&Rfc3339).ok());
        windows.push(PlanWindow {
            name: name.to_owned(),
            used,
            resets_at,
            span_secs,
            aside: false,
        });
    }
    ensure!(!windows.is_empty(), "no usage in the reply");
    Ok((String::new(), windows))
}

// plan_windows asks the vendor for the plan key is on and its windows.
pub(crate) async fn plan_windows(
    client: &reqwest::Client,
    source: &PlanSource,
    key: &str,
) -> Result<(String, Vec<PlanWindow>)> {
    let mut request = client
        .get(&source.url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("Accept-Language", "en-US,en");
    request = if source.bearer {
        request.bearer_auth(key)
    } else {
        request.header(reqwest::header::AUTHORIZATION, key)
    };
    let mut response = request.send().await.context("ask the plan's usage")?;
    let status = response.status();
    let bytes = crate::codex::read_limited(&mut response, MAX_RESPONSE_BYTES)
        .await
        .context("read the plan's usage")?;
    if status.as_u16() == 403 && source.bearer {
        bail!("this key has no OpenCode Go subscription");
    }
    if !status.is_success() {
        bail!("{status}");
    }
    if source.bearer {
        read_opencode_go(&bytes)
    } else {
        read_zhipu_plan(&bytes)
    }
}

#[derive(Clone, Debug)]
struct PlanQuota {
    provider: String,
    name: String,
    user: String,
    plan: String,
    windows: Vec<PlanWindow>,
    error: Option<String>,
}

static CACHE: LazyLock<Mutex<Option<(Instant, Vec<PlanQuota>)>>> =
    LazyLock::new(|| Mutex::new(None));

// plan_quotas is the windows of every plan magpie has a key for, each key
// on a card of its own when a provider has several. What was asked less
// than a minute ago is not asked again.
pub(crate) async fn plan_quotas() -> Vec<crate::quota::Quota> {
    {
        let cache = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, cards)) = cache.as_ref()
            && at.elapsed() < CACHE_TTL
        {
            return cards.iter().map(quota_of).collect();
        }
    }
    struct Job {
        id: String,
        name: String,
        source: PlanSource,
        key: String,
        user: String,
    }
    let providers = match super::load() {
        Ok(file) => file.providers,
        Err(_) => return Vec::new(),
    };
    let mut jobs: Vec<Job> = Vec::new();
    for provider in providers.into_iter().filter(|provider| {
        !provider.hidden && provider.account.is_none() && !provider.key.is_empty()
    }) {
        let Some(source) = plan_quota_source_of(&provider) else {
            continue;
        };
        let mut keys = vec![(provider.key.clone(), provider.key_name.clone())];
        for key in &provider.keys {
            if !key.off && !key.key.is_empty() && key.key != provider.key {
                keys.push((key.key.clone(), key.name.clone()));
            }
        }
        for (key, name) in &keys {
            let user = if keys.len() > 1 {
                let name = name.clone();
                if name.is_empty() {
                    super::mask(key)
                } else {
                    name
                }
            } else {
                String::new()
            };
            jobs.push(Job {
                id: provider.id.clone(),
                name: provider.name.clone(),
                source: source.clone(),
                key: key.clone(),
                user,
            });
        }
    }
    let shared = crate::netproxy::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok();
    let cards: Vec<PlanQuota> = join_all(jobs.iter().map(|job| {
        let client = shared.clone();
        async move {
            let Some(client) = client else {
                return Some(PlanQuota {
                    provider: job.id.clone(),
                    name: job.name.clone(),
                    user: job.user.clone(),
                    plan: String::new(),
                    windows: Vec::new(),
                    error: Some("create plan usage client".to_owned()),
                });
            };
            match plan_windows(&client, &job.source, &job.key).await {
                Ok((plan, windows)) if windows.is_empty() => None, // a key with no plan
                Ok((plan, windows)) => Some(PlanQuota {
                    provider: job.id.clone(),
                    name: job.name.clone(),
                    user: job.user.clone(),
                    plan,
                    windows,
                    error: None,
                }),
                Err(error) if !job.source.sure => None, // a key with no plan
                Err(error) => Some(PlanQuota {
                    provider: job.id.clone(),
                    name: job.name.clone(),
                    user: job.user.clone(),
                    plan: String::new(),
                    windows: Vec::new(),
                    error: Some(format!("{error:#}")),
                }),
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();
    {
        let mut cache = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cache = Some((Instant::now(), cards.clone()));
    }
    cards.iter().map(quota_of).collect()
}

fn quota_of(card: &PlanQuota) -> crate::quota::Quota {
    crate::quota::Quota::subscription(
        card.provider.clone(),
        card.name.clone(),
        card.plan.clone(),
        card.user.clone(),
        card.windows
            .iter()
            .map(|window| {
                crate::quota::QuotaSpan::new(
                    window.name.clone(),
                    window.used,
                    (100.0 - window.used).max(0.0),
                    window.resets_at.clone(),
                    String::new(),
                )
            })
            .collect(),
        card.error.clone().unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(chat: &str, anthropic: &str) -> Provider {
        Provider {
            chat: chat.to_owned(),
            anthropic: anthropic.to_owned(),
            ..Provider::default()
        }
    }

    #[test]
    fn finds_plan_sources() {
        for (provider, url, sure) in [
            (
                provider(
                    "https://open.bigmodel.cn/api/coding/paas/v4",
                    "https://open.bigmodel.cn/api/anthropic",
                ),
                "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
                true,
            ),
            (
                provider(
                    "https://open.bigmodel.cn/api/paas/v4",
                    "https://open.bigmodel.cn/api/anthropic",
                ),
                "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
                false,
            ),
            (
                provider("", "https://api.z.ai/api/anthropic"),
                "https://api.z.ai/api/monitor/usage/quota/limit",
                false,
            ),
            (
                provider(
                    "https://opencode.ai/zen/go/v1",
                    "https://opencode.ai/zen/go",
                ),
                "https://opencode.ai/zen/go/v1/usage",
                true,
            ),
            (
                provider("", "https://opencode.ai/zen/go"),
                "https://opencode.ai/zen/go/v1/usage",
                true,
            ),
            // Zen is pay as you go
            (provider("https://opencode.ai/zen/v1", ""), "", false),
            (provider("https://api.deepseek.com", ""), "", false),
        ] {
            let source = plan_quota_source_of(&provider);
            assert_eq!(
                source.as_ref().map(|source| source.url.as_str()),
                if url.is_empty() { None } else { Some(url) }
            );
            assert_eq!(source.is_some_and(|source| source.sure), sure);
        }
    }

    #[test]
    fn reads_a_zhipu_plan() {
        let (plan, windows) = read_zhipu_plan(
            br#"{"code":200,"success":true,"data":{"level":"pro","limits":[
                {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12,"nextResetTime":1790000000000},
                {"type":"TOKENS_LIMIT","unit":6,"number":7,"percentage":40,"nextResetTime":1790500000000},
                {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":3,"usageDetails":[{"modelCode":"search-prime","usage":2}]}]}}"#,
        )
        .unwrap();
        assert_eq!(plan, "pro");
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].name, "5 hours");
        assert_eq!(windows[0].used, 12.0);
        assert_eq!(windows[0].span_secs, 5 * 3600);
        assert_eq!(
            windows[0].resets_at.as_deref(),
            millis_to_rfc3339(1790000000000).as_deref()
        );
        assert!(windows[0].resets_at.is_some());
        assert_eq!(windows[1].name, "7 days");
        assert_eq!(windows[1].used, 40.0);
        assert_eq!(windows[1].span_secs, 7 * 86_400);
        assert_eq!(windows[2].name, "MCP · Month");
        assert!(windows[2].aside);
        assert_eq!(windows[2].resets_at, None);
    }

    #[test]
    fn a_zhipu_refusal_reads_as_its_message() {
        let error = read_zhipu_plan(
            r#"{"code":401,"msg":"令牌已过期或验证不正确","success":false}"#.as_bytes(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "令牌已过期或验证不正确");
    }

    #[test]
    fn a_payg_zhipu_key_has_no_windows() {
        let (plan, windows) = read_zhipu_plan(br#"{"success":true,"data":{"limits":[]}}"#).unwrap();
        assert_eq!(plan, "");
        assert!(windows.is_empty());
    }

    #[test]
    fn reads_opencode_go_usage() {
        let (_, windows) = read_opencode_go(
            br#"{"usage":{
                "rolling":{"status":"ok","percent":0,"resetsAt":"2026-09-25T20:00:00.000Z"},
                "weekly":{"status":"ok","percent":62,"resetsAt":"2026-09-28T00:00:00.000Z"},
                "monthly":{"status":"rate-limited","percent":100,"resetsAt":"2026-10-11T00:00:00.000Z"}}}"#,
        )
        .unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].name, "5 hours");
        assert_eq!(windows[0].used, 0.0);
        // an idle window keeps its placeholder reset
        assert_eq!(windows[0].resets_at, None);
        assert_eq!(windows[1].name, "7 days");
        assert_eq!(windows[1].used, 62.0);
        assert_eq!(
            windows[1].resets_at.as_deref(),
            Some("2026-09-28T00:00:00Z")
        );
        assert_eq!(windows[2].name, "Month");
        assert_eq!(windows[2].used, 100.0);
    }

    #[test]
    fn a_reply_of_another_shape_reads_as_no_windows() {
        assert!(read_opencode_go(br#"{"rollingUsage":{}}"#).is_err());
    }
}
