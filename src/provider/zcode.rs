use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aes_gcm::{
    Aes256Gcm,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::{Url, form_urlencoded};

// A ZCode subscription is Z.ai's GLM Coding Plan, which ZCode (Zhipu's
// desktop app) signs in to. The plan is served on an Anthropic-compatible
// endpoint to a plain API key, `<id>.<secret>`, that ZCode mints for the
// account and names zcode-api-key; magpie uses that key as ZCode does.
//
// ZCode's own account is read, never changed, from its credential store,
// ~/.zcode/v2/credentials.json: each value is "enc:v1:" + iv.tag.ciphertext
// (base64url), AES-256-GCM under sha256 of $ZCODE_CREDENTIAL_SECRET, or
// else of "zcode-credential-fallback:<platform>:<home>:<user>". Further
// accounts are signed in by magpie with ZCode's own polling sign-in
// (zcode.z.ai/api/v1/oauth/cli/…), and their key is kept in logins.json.

pub(crate) const ZCODE_ZAI_BASE: &str = "https://api.z.ai/api/anthropic";
pub(crate) const ZCODE_BIGMODEL_BASE: &str = "https://open.bigmodel.cn/api/anthropic";
pub(crate) const ZCODE_API: &str = "https://zcode.z.ai";
pub(crate) const ZCODE_ZAI_API: &str = "https://api.z.ai";

const ZCODE_APP_VERSION: &str = "3.14.3";
const MAX_RESPONSE_BYTES: usize = 1 << 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

// zcodeKey is a coding plan's key and where it is served, as logins.json
// keeps it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ZcodeKey {
    #[serde(rename = "apiKey")]
    pub(crate) api_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) base: String,
}

impl ZcodeKey {
    pub(crate) fn endpoint(&self) -> &str {
        if self.base.is_empty() {
            ZCODE_ZAI_BASE
        } else {
            &self.base
        }
    }

    // root is the site a plan's endpoint is on: https://api.z.ai.
    pub(crate) fn root(&self) -> String {
        Url::parse(self.endpoint())
            .ok()
            .filter(|page| page.host_str().is_some_and(|host| !host.is_empty()))
            .map(|page| {
                format!(
                    "{}://{}",
                    page.scheme(),
                    page.host_str().unwrap_or_default()
                )
            })
            .unwrap_or_else(|| ZCODE_ZAI_API.to_owned())
    }
}

// The plan has no model list to ask; the models are ZCode's, as its
// built-in config lists them for the coding plan.
pub(crate) fn models() -> Vec<crate::catalog::Model> {
    [
        ("GLM-5.3", 1_000_000),
        ("GLM-5.3-Flash", 1_000_000),
        ("GLM-5.2", 1_000_000),
        ("GLM-5-Turbo", 200_000),
    ]
    .into_iter()
    .map(|(id, context)| crate::catalog::Model {
        id: id.to_owned(),
        name: id.to_owned(),
        context,
        ..crate::catalog::Model::default()
    })
    .collect()
}

// ---- ZCode's own account ------------------------------------------------------

pub(crate) struct OwnAccount {
    pub(crate) user: String,
    pub(crate) key: ZcodeKey,
}

// own is the account ZCode is signed in to and its coding plan's key, read
// from its credential store and never changed; None when it has none.
pub(crate) fn own() -> Option<OwnAccount> {
    let contents = fs::read(credentials_path()?).ok()?;
    let store: BTreeMap<String, String> = serde_json::from_slice(&contents).ok()?;
    let secret = credential_secret();
    let mut key = ZcodeKey::default();
    for (name, value) in &store {
        // account-provider:coding-plan:account:zai-individual-coding-plan:account:<uuid>:api-key
        if !name.contains(":coding-plan:") || !name.ends_with(":api-key") {
            continue;
        }
        let Some(api_key) = decrypt(&secret, value).filter(|key| key.contains('.')) else {
            continue;
        };
        let base = if name.contains(":bigmodel-") {
            ZCODE_BIGMODEL_BASE
        } else {
            ZCODE_ZAI_BASE
        };
        if key_wins(&key, base) {
            key = ZcodeKey {
                api_key,
                base: base.to_owned(),
            };
        }
    }
    if key.api_key.is_empty() {
        return None;
    }
    let (mut email, mut name, mut id) = (String::new(), String::new(), String::new());
    if let Some(value) = store.get("oauth:zai:user_info")
        && let Some(text) = decrypt(&secret, value)
        && let Ok(info) = serde_json::from_str::<Value>(&text)
    {
        email = info
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        name = info
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        id = info
            .get("user_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
    }
    Some(OwnAccount {
        user: who(&email, &name, &id),
        key,
    })
}

fn credentials_path() -> Option<PathBuf> {
    user_home().map(|home| home.join(".zcode").join("v2").join("credentials.json"))
}

fn user_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
}

// credential_secret is the key ZCode encrypts its credentials with.
fn credential_secret() -> [u8; 32] {
    let seed = match env::var("ZCODE_CREDENTIAL_SECRET") {
        Ok(seed) if !seed.is_empty() => seed,
        _ => {
            let home = user_home()
                .map(|home| home.to_string_lossy().to_string())
                .unwrap_or_default();
            // node's userInfo() has the user alone, without the domain
            let name = env::var("USERNAME")
                .or_else(|_| env::var("USER"))
                .unwrap_or_default();
            let name = name.rsplit('\\').next().unwrap_or_default().to_owned();
            let platform = match env::consts::OS {
                "windows" => "win32",
                other => other,
            };
            format!("zcode-credential-fallback:{platform}:{home}:{name}")
        }
    };
    let sum = Sha256::digest(seed.as_bytes());
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&sum);
    secret
}

// key_wins says a found key replaces the one held: Z.ai's first, as ZCode
// lists it.
fn key_wins(current: &ZcodeKey, base: &str) -> bool {
    current.api_key.is_empty() || (base == ZCODE_ZAI_BASE && current.base != ZCODE_ZAI_BASE)
}

// decrypt opens one of ZCode's values: "enc:v1:" + iv.tag.ciphertext
// (base64url), AES-256-GCM with the tag after the text.
fn decrypt(secret: &[u8; 32], value: &str) -> Option<String> {
    let rest = value.strip_prefix("enc:v1:")?;
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let iv = decode_part(parts[0])?;
    let tag = decode_part(parts[1])?;
    let ciphertext = decode_part(parts[2])?;
    // ZCode writes 12-byte nonces
    if iv.len() != 12 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(secret.as_slice()).ok()?;
    let sealed = [ciphertext.as_slice(), tag.as_slice()].concat();
    let plain = cipher.decrypt(&aead_nonce(&iv), sealed.as_slice()).ok()?;
    String::from_utf8(plain).ok()
}

fn decode_part(part: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()
}

fn aead_nonce(iv: &[u8]) -> aes_gcm::aead::Nonce<Aes256Gcm> {
    aes_gcm::aead::Nonce::<Aes256Gcm>::try_from(iv).expect("nonce")
}

// who names a Z.ai account: its email, or the phone number of one signed
// in by phone (its email is then <phone>@phone.local).
pub(crate) fn who(email: &str, name: &str, id: &str) -> String {
    let email = email.strip_suffix("@phone.local").unwrap_or(email);
    [email, name, id]
        .into_iter()
        .map(str::trim)
        .find(|part| !part.is_empty())
        .unwrap_or("ZCode")
        .to_owned()
}

// ---- Z.ai's API ---------------------------------------------------------------

#[derive(Debug)]
struct StatusError {
    status: u16,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            reqwest::StatusCode::from_u16(self.status)
                .ok()
                .and_then(|status| status.canonical_reason())
                .unwrap_or("request failed"),
        )
    }
}

impl std::error::Error for StatusError {}

// call asks one of Z.ai's JSON endpoints, which wrap what they say in
// {code, msg, data}: code 0 or 200 is a success.
async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    auth: &str,
    body: Option<Value>,
) -> Result<Value> {
    let mut request = client
        .request(method, url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            format!("ZCode/{ZCODE_APP_VERSION}"),
        );
    if !auth.is_empty() {
        request = request.header(reqwest::header::AUTHORIZATION, auth);
    }
    if let Some(body) = &body {
        request = request.json(body);
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| anyhow!("request Z.ai: {}", error.without_url()))?;
    let status = response.status();
    let bytes = crate::codex::read_limited(&mut response, MAX_RESPONSE_BYTES)
        .await
        .map_err(|error| anyhow!("read Z.ai reply: {error:#}"))?;
    if !status.is_success() {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(message) = value
                .get("msg")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
        {
            bail!("{message} ({})", status.as_u16());
        }
        return Err(anyhow::Error::new(StatusError {
            status: status.as_u16(),
        }));
    }
    let reply: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse the Z.ai reply from {url}"))?;
    let code = match reply.get("code") {
        None => String::new(),
        Some(Value::Null) => "null".to_owned(),
        Some(Value::String(text)) => text.trim_matches('"').to_owned(),
        Some(other) => plain_number(other),
    };
    if !matches!(code.as_str(), "" | "null" | "0" | "200") {
        let message = reply
            .get("msg")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty());
        bail!(
            "{}",
            message.map_or_else(|| format!("error {code}"), str::to_owned)
        );
    }
    Ok(reply.get("data").cloned().unwrap_or(Value::Null))
}

// plain_number says a JSON number as Z.ai's Go server would print it: 2.0
// is "2".
fn plain_number(value: &Value) -> String {
    match value.as_f64() {
        Some(number) if number.fract() == 0.0 && number.abs() < 1e15 => {
            format!("{number}")
        }
        _ => value.to_string(),
    }
}

pub(crate) async fn plan_for(client: &reqwest::Client, key: &ZcodeKey) -> Result<String> {
    let data = call(
        client,
        reqwest::Method::GET,
        &format!("{}/api/biz/subscription/list", key.root()),
        &key.api_key,
        None,
    )
    .await?;
    for entry in data.as_array().into_iter().flatten() {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if status.eq_ignore_ascii_case("VALID") {
            return Ok(entry
                .get("productName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned());
        }
    }
    Ok(String::new())
}

// ---- allowance ----------------------------------------------------------------

pub(crate) struct QuotaWindow {
    pub(crate) name: String,
    pub(crate) used: f64,
    pub(crate) resets_at: Option<String>,
    pub(crate) display: String,
}

// quota is a coding plan's allowance: credits per five hours and per week,
// as ZCode shows them.
pub(crate) async fn quota(
    client: &reqwest::Client,
    key: &ZcodeKey,
) -> Result<(String, Vec<QuotaWindow>)> {
    let data = call(
        client,
        reqwest::Method::GET,
        &format!("{}/api/monitor/usage/quota/limit", key.root()),
        &key.api_key,
        None,
    )
    .await?;
    Ok(parse_quota(&data))
}

pub(crate) fn parse_quota(data: &Value) -> (String, Vec<QuotaWindow>) {
    let level = data
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let plan = if level.is_empty() {
        String::new()
    } else {
        let mut characters = level.chars();
        format!(
            "GLM Coding {}{}",
            characters
                .next()
                .map(|first| first.to_ascii_uppercase())
                .unwrap_or_default(),
            characters.as_str()
        )
    };
    let mut windows = Vec::new();
    for limit in data
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let unit = limit.get("unit").and_then(Value::as_i64).unwrap_or(0);
        let number = limit.get("number").and_then(Value::as_i64).unwrap_or(0);
        let usage = limit.get("usage").and_then(Value::as_f64).unwrap_or(0.0);
        let remaining = limit
            .get("remaining")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let percent = limit
            .get("percentage")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let reset = limit
            .get("nextResetTime")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let span = span_secs(unit, number);
        let mut window = QuotaWindow {
            name: window_name(span),
            used: percent,
            resets_at: millis_to_rfc3339(reset),
            display: String::new(),
        };
        if usage > 0.0 {
            let used = usage - remaining;
            window.used = 100.0 * used / usage;
            window.display = format!("{} / {}", compact_number(used), compact_number(usage));
        }
        windows.push(window);
    }
    (plan, windows)
}

// span_secs reads a limit's window: unit 3 counts hours, 6 weeks (and 4, 5
// days and months by the same count); 0 when the unit is unknown.
fn span_secs(unit: i64, number: i64) -> u64 {
    let number = number.max(1) as u64;
    match unit {
        1 => number * 60,
        3 => number * 3600,
        4 => number * 86_400,
        5 => number * 30 * 86_400,
        6 => number * 7 * 86_400,
        _ => 0,
    }
}

fn window_name(span: u64) -> String {
    const DAY: u64 = 86_400;
    const WEEK: u64 = 7 * DAY;
    match span {
        0 => "Credits".to_owned(),
        span if span < DAY => format!("{} hours", span / 3600),
        WEEK => "Weekly".to_owned(),
        span if span >= 28 * DAY => "Monthly".to_owned(),
        span => format!("{} days", span / DAY),
    }
}

pub(super) fn millis_to_rfc3339(millis: i64) -> Option<String> {
    if millis <= 0 {
        return None;
    }
    let stamp = OffsetDateTime::from_unix_timestamp(millis.div_euclid(1000)).ok()?;
    stamp
        .replace_nanosecond(millis.rem_euclid(1000) as u32)
        .ok()?
        .format(&Rfc3339)
        .ok()
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

// ---- signing in ---------------------------------------------------------------

// sign_in is ZCode's own polling sign-in: zcode.z.ai opens a flow, the
// browser signs in to Z.ai, and the flow is asked until it is ready. The
// account's coding plan key is then found or made, as ZCode does it. The
// sign-in page is handed to on_url once it is known.
pub(crate) async fn sign_in(on_url: impl FnOnce(&str)) -> Result<(String, String, ZcodeKey)> {
    let client = crate::netproxy::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("create ZCode sign-in client")?;
    let mut poll_bytes = [0u8; 32];
    getrandom::fill(&mut poll_bytes).context("generate ZCode poll token")?;
    let poll = format!(
        "Bearer {}",
        poll_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let flow = call(
        &client,
        reqwest::Method::POST,
        &format!("{ZCODE_API}/api/v1/oauth/cli/init"),
        &poll,
        Some(json!({ "provider": "zai" })),
    )
    .await
    .map_err(|error| anyhow!("ZCode sign-in: {error:#}"))?;
    let flow_id = flow
        .get("flow_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let authorize_url = flow
        .get("authorize_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expires_at = flow
        .get("expires_at")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let interval = Duration::from_secs_f64(
        flow.get("poll_interval_sec")
            .and_then(Value::as_f64)
            .unwrap_or_default()
            .max(1.0),
    );
    let mut page = match Url::parse(&authorize_url) {
        Ok(page) if page.scheme() == "https" => page,
        _ => bail!("ZCode gave no sign-in page"),
    };
    ensure!(!flow_id.is_empty(), "ZCode gave no sign-in page");
    // where Z.ai sends the browser back, as ZCode sets it
    let mut back = Url::parse(&format!("{ZCODE_API}/app/oauth/login"))
        .context("parse the ZCode sign-in address")?;
    back.query_pairs_mut()
        .append_pair("redirect", "zcode://oauth/callback")
        .append_pair("app_version", ZCODE_APP_VERSION);
    page.query_pairs_mut()
        .append_pair("redirect_uri", back.as_str());
    let page_url = page.to_string();
    on_url(&page_url);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or_default();
    let deadline = tokio::time::Instant::now()
        + if expires_at > 0.0 {
            Duration::from_secs_f64((expires_at - now).max(1.0))
        } else {
            Duration::from_secs(5 * 60)
        };
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            bail!("the sign-in expired; start again");
        }
        let answer = call(
            &client,
            reqwest::Method::GET,
            &format!(
                "{ZCODE_API}/api/v1/oauth/cli/poll/{}",
                path_escape(&flow_id)
            ),
            &poll,
            None,
        )
        .await;
        match answer {
            Err(error) => {
                if let Some(status) = error.downcast_ref::<StatusError>()
                    && (400..500).contains(&status.status)
                    && status.status != 408
                    && status.status != 429
                {
                    bail!("ZCode sign-in: {error}");
                }
                // a hiccup: ask again
            }
            Ok(data) => {
                let status = data
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match status {
                    "pending" | "" => {}
                    "failed" => bail!("the sign-in was declined on Z.ai"),
                    "ready" => {
                        let zai_token = data
                            .pointer("/zai/access_token")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        ensure!(
                            !zai_token.is_empty(),
                            "ZCode sign-in: unexpected answer {status}"
                        );
                        let user_id = data
                            .pointer("/user/user_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let email = data
                            .pointer("/user/email")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let name = data
                            .pointer("/user/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let (key, plan) = mint_key(&client, &zai_token).await?;
                        return Ok((who(email, name, user_id), plan, key));
                    }
                    other => bail!("ZCode sign-in: unexpected answer {other}"),
                }
            }
        }
    }
}

// mint_key turns a Z.ai sign-in into its coding plan key: Z.ai's business
// token, then the key named zcode-api-key in the account's default project,
// made if it isn't there, with its secret.
async fn mint_key(client: &reqwest::Client, zai_token: &str) -> Result<(ZcodeKey, String)> {
    let biz = call(
        client,
        reqwest::Method::POST,
        &format!("{ZCODE_ZAI_API}/api/auth/z/login"),
        "",
        Some(json!({ "token": zai_token })),
    )
    .await
    .map_err(|error| anyhow!("Z.ai sign-in: {error:#}"))?;
    let biz_token = biz
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure!(!biz_token.is_empty(), "Z.ai sign-in: no token");
    let bearer = format!("Bearer {biz_token}");

    let info = call(
        client,
        reqwest::Method::GET,
        &format!("{ZCODE_ZAI_API}/api/biz/customer/getCustomerInfo"),
        &bearer,
        None,
    )
    .await
    .map_err(|error| anyhow!("Z.ai account: {error:#}"))?;

    // the default organization and project, as ZCode picks them
    let mut org = String::new();
    let mut project = String::new();
    for entry in info
        .get("organizations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let org_id = entry
            .get("organizationId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let org_name = entry
            .get("organizationName")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut projects: Vec<&str> = Vec::new();
        let mut default_project = String::new();
        for candidate in entry
            .get("projects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let project_id = candidate
                .get("projectId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if project_id.is_empty() || is_project_type_two(candidate.get("projectType")) {
                continue;
            }
            projects.push(project_id);
            if default_project.is_empty()
                && candidate
                    .get("projectName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .contains("默认项目")
            {
                default_project = project_id.to_owned();
            }
        }
        if org_id.is_empty() || projects.is_empty() {
            continue;
        }
        if default_project.is_empty() {
            default_project = projects[0].to_owned();
        }
        if org.is_empty() || org_name.contains("默认机构") {
            org = org_id.to_owned();
            project = default_project;
            if org_name.contains("默认机构") {
                break;
            }
        }
    }
    ensure!(
        !org.is_empty(),
        "this Z.ai account has no project for an API key"
    );

    let keys_url = format!(
        "{ZCODE_ZAI_API}/api/biz/v1/organization/{}/projects/{}/api_keys",
        path_escape(&org),
        path_escape(&project)
    );
    let list = call(client, reqwest::Method::GET, &keys_url, &bearer, None)
        .await
        .map_err(|error| anyhow!("Z.ai API keys: {error:#}"))?;
    let mut id = String::new();
    for key in list.as_array().into_iter().flatten() {
        if key.get("name").and_then(Value::as_str) == Some("zcode-api-key") {
            id = key
                .get("apiKey")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
        }
    }
    if id.is_empty() {
        let made = call(
            client,
            reqwest::Method::POST,
            &keys_url,
            &bearer,
            Some(json!({ "name": "zcode-api-key" })),
        )
        .await
        .map_err(|error| anyhow!("Z.ai API key: {error:#}"))?;
        id = made
            .get("apiKey")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
    }
    let mut secret = String::new();
    if !id.is_empty() {
        let copied = call(
            client,
            reqwest::Method::GET,
            &format!("{keys_url}/copy/{}", path_escape(&id)),
            &bearer,
            None,
        )
        .await
        .map_err(|error| anyhow!("Z.ai API key: {error:#}"))?;
        secret = copied
            .get("secretKey")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
    }
    ensure!(!id.is_empty() && !secret.is_empty(), "Z.ai gave no API key");

    let key = ZcodeKey {
        api_key: format!("{id}.{secret}"),
        base: ZCODE_ZAI_BASE.to_owned(),
    };
    let plan = plan_for(client, &key)
        .await
        .map_err(|error| anyhow!("GLM Coding Plan: {error:#}"))?;
    ensure!(
        !plan.is_empty(),
        "this Z.ai account has no GLM Coding Plan — subscribe at z.ai/subscribe, then add it again"
    );
    Ok((key, plan))
}

fn is_project_type_two(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(number)) => number.as_f64() == Some(2.0),
        Some(Value::String(text)) => text.trim() == "2",
        _ => false,
    }
}

fn path_escape(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // zcode_encrypt stores a value as ZCode's credential store does.
    fn zcode_encrypt(secret: &[u8; 32], value: &str) -> String {
        let cipher = Aes256Gcm::new_from_slice(secret.as_slice()).unwrap();
        let iv = b"0123456789ab";
        let sealed = cipher
            .encrypt(&aead_nonce(iv.as_slice()), value.as_bytes())
            .unwrap();
        let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);
        format!(
            "enc:v1:{}.{}.{}",
            URL_SAFE_NO_PAD.encode(iv),
            URL_SAFE_NO_PAD.encode(tag),
            URL_SAFE_NO_PAD.encode(ciphertext)
        )
    }

    #[test]
    fn decrypts_a_credential_store_value() {
        let secret = [7u8; 32];
        let value = zcode_encrypt(&secret, "own.secret");
        assert_eq!(decrypt(&secret, &value).as_deref(), Some("own.secret"));
        // padded base64 is accepted, as ZCode writes it either way
        assert_eq!(
            decrypt(&secret, &format!("{value}=")).as_deref(),
            Some("own.secret")
        );
        assert!(decrypt(&secret, "own.secret").is_none());
        assert!(decrypt(&secret, "enc:v1:abc").is_none());
        assert!(decrypt(&secret, "enc:v1:a.b.c.d").is_none());
        assert!(decrypt(&[8u8; 32], &value).is_none());
    }

    #[test]
    fn prefers_zai_over_bigmodel() {
        assert!(key_wins(&ZcodeKey::default(), ZCODE_ZAI_BASE));
        assert!(key_wins(&ZcodeKey::default(), ZCODE_BIGMODEL_BASE));
        assert!(!key_wins(
            &ZcodeKey {
                api_key: "a.b".to_owned(),
                base: ZCODE_ZAI_BASE.to_owned()
            },
            ZCODE_BIGMODEL_BASE
        ));
        assert!(key_wins(
            &ZcodeKey {
                api_key: "a.b".to_owned(),
                base: ZCODE_BIGMODEL_BASE.to_owned()
            },
            ZCODE_ZAI_BASE
        ));
    }

    #[test]
    fn names_accounts() {
        assert_eq!(
            who("13800000000@phone.local", "旅行者0000", ""),
            "13800000000"
        );
        assert_eq!(who("", " 旅行者 ", "u1"), "旅行者");
        assert_eq!(who("", "", "u1"), "u1");
        assert_eq!(who("", "", ""), "ZCode");
    }

    #[test]
    fn names_windows() {
        assert_eq!(window_name(0), "Credits");
        assert_eq!(window_name(5 * 3600), "5 hours");
        assert_eq!(window_name(7 * 86_400), "Weekly");
        assert_eq!(window_name(30 * 86_400), "Monthly");
        assert_eq!(window_name(2 * 86_400), "2 days");
        assert_eq!(span_secs(3, 5), 5 * 3600);
        assert_eq!(span_secs(6, 1), 7 * 86_400);
        assert_eq!(span_secs(9, 5), 0);
        assert_eq!(span_secs(3, 0), 3600);
    }

    #[test]
    fn reads_a_quota_reply() {
        let (plan, windows) = parse_quota(&json!({
            "level": "pro",
            "limits": [
                {"type": "CREDIT_LIMIT", "unit": 3, "number": 5, "usage": 2000, "remaining": 1500,
                 "percentage": 25, "nextResetTime": 1790000000000i64},
                {"type": "CREDIT_LIMIT", "unit": 6, "number": 1, "usage": 10000, "remaining": 9000,
                 "percentage": 10},
            ],
        }));
        assert_eq!(plan, "GLM Coding Pro");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name, "5 hours");
        assert_eq!(windows[0].used, 25.0);
        assert_eq!(windows[0].display, "500 / 2000");
        assert_eq!(
            windows[0].resets_at.as_deref(),
            millis_to_rfc3339(1790000000000).as_deref()
        );
        assert!(windows[0].resets_at.is_some());
        assert_eq!(windows[1].name, "Weekly");
        assert_eq!(windows[1].used, 10.0);
        assert_eq!(windows[1].display, "1000 / 10000");
        assert_eq!(windows[1].resets_at, None);
    }

    #[test]
    fn reads_a_quota_reply_without_credits() {
        let (plan, windows) = parse_quota(&json!({
            "limits": [
                {"type": "CREDIT_LIMIT", "unit": 3, "number": 5, "percentage": 12},
            ],
        }));
        assert_eq!(plan, "");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].name, "5 hours");
        assert_eq!(windows[0].used, 12.0);
        assert_eq!(windows[0].display, "");
    }
}
