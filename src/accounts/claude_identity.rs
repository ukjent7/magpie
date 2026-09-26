use std::{
    env, fs,
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use serde::Deserialize;
use serde_json::{Map, Value};

use anyhow::{Context, Result, anyhow, bail};

use super::{SavedLogin, claude_oauth};

const CLAUDE_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default, Deserialize)]
#[serde(default)]
struct AuthStatus {
    #[serde(rename = "loggedIn")]
    logged_in: Option<bool>,
    email: String,
}

enum CredentialLocation {
    File(PathBuf),
    #[cfg(target_os = "macos")]
    Keychain {
        account: String,
    },
}

pub(super) async fn live_login() -> Option<SavedLogin> {
    let (auth, _) = read_credentials()?;
    let oauth = auth.get("claudeAiOauth")?;
    let access_token = oauth.get("accessToken")?.as_str()?;
    if access_token.is_empty() {
        return None;
    }

    let status = auth_status().await;
    if status
        .as_ref()
        .is_some_and(|status| status.logged_in == Some(false))
    {
        return None;
    }

    let profile = read_profile().and_then(|profile| profile.get("oauthAccount").cloned());
    let email = profile
        .as_ref()
        .and_then(|profile| profile.get("emailAddress"))
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .or_else(|| status.as_ref().map(|status| status.email.as_str()))?;
    let plan = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let user = claude_user(email, plan, profile.as_ref());
    if user.is_empty() {
        return None;
    }

    Some(SavedLogin {
        agent: "claude".to_owned(),
        user,
        plan: (!plan.is_empty()).then(|| plan.to_owned()),
        profile,
        auth: Some(auth),
        ..SavedLogin::default()
    })
}

pub(super) fn usable_auth(auth: &Value) -> bool {
    auth.pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty())
}

pub(super) fn latest_auth() -> Option<Value> {
    read_credentials().map(|(auth, _)| auth)
}

pub(super) fn install_login(auth: &Value, profile: Option<&Value>) -> Result<()> {
    if !usable_auth(auth) {
        bail!("the saved Claude Code sign-in is unreadable");
    }

    let location = match read_credentials() {
        Some((_, location)) => location,
        None => default_credential_location()?,
    };
    let mut contents =
        serde_json::to_vec_pretty(auth).context("serialize saved Claude credentials")?;
    contents.push(b'\n');
    match location {
        CredentialLocation::File(path) => {
            crate::config::atomic_write_secret_for_settings(&path, &contents)
                .with_context(|| format!("write Claude credentials at {}", path.display()))?
        }
        #[cfg(target_os = "macos")]
        CredentialLocation::Keychain { account } => {
            let secret = String::from_utf8(contents).context("serialize Claude credentials")?;
            let mut command = crate::proc::command("security");
            command.args([
                "add-generic-password",
                "-U",
                "-s",
                "Claude Code-credentials",
            ]);
            if !account.is_empty() {
                command.arg("-a").arg(&account);
            }
            let output = command
                .arg("-w")
                .arg(secret)
                .output()
                .context("save Claude Code credentials to Keychain")?;
            if !output.status.success() {
                bail!("save Claude Code credentials to Keychain failed");
            }
        }
    }

    if let Some(profile) = profile {
        write_profile(profile)?;
    }
    Ok(())
}

pub(super) fn same_account(left: &Value, right: &Value) -> Option<bool> {
    let (left_email, left_org) = profile_identity(left)?;
    let (right_email, right_org) = profile_identity(right)?;
    Some(left_email.eq_ignore_ascii_case(right_email) && left_org == right_org)
}

fn profile_identity(profile: &Value) -> Option<(&str, &str)> {
    let email = profile.get("emailAddress")?.as_str()?;
    let organization = profile.get("organizationUuid")?.as_str()?;
    (!email.is_empty() && !organization.is_empty()).then_some((email, organization))
}

pub(super) fn claude_user(email: &str, plan: &str, profile: Option<&Value>) -> String {
    if email.is_empty() || !matches!(plan, "team" | "enterprise") {
        return email.to_owned();
    }
    let organization = profile
        .and_then(|profile| profile.get("organizationName"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let organization = if organization.is_empty() || organization.contains(email) {
        title_case(plan)
    } else {
        organization.to_owned()
    };
    format!("{email} · {organization}")
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect::<String>()
    })
}

async fn auth_status() -> Option<AuthStatus> {
    let output = tokio::time::timeout(
        CLAUDE_STATUS_TIMEOUT,
        crate::proc::async_command("claude")
            .args(["auth", "status", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    serde_json::from_slice(&output.stdout).ok()
}

fn read_credentials() -> Option<(Value, CredentialLocation)> {
    let path = config_dir()?.join(".credentials.json");
    if let Ok(contents) = fs::read(&path)
        && let Ok(auth) = serde_json::from_slice::<Value>(&contents)
        && usable_auth(&auth)
    {
        return Some((auth, CredentialLocation::File(path)));
    }

    #[cfg(target_os = "macos")]
    {
        let output = crate::proc::command("security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
            .ok()?;
        if output.status.success() {
            let auth = serde_json::from_slice(output.stdout.trim_ascii()).ok()?;
            if usable_auth(&auth) {
                return Some((
                    auth,
                    CredentialLocation::Keychain {
                        account: env::var("USER").unwrap_or_default(),
                    },
                ));
            }
        }
    }
    None
}

fn read_profile() -> Option<Value> {
    let path = profile_path()?;
    let contents = fs::read(path).ok()?;
    serde_json::from_slice(&contents).ok()
}

fn write_profile(profile: &Value) -> Result<()> {
    let path = profile_path().context("cannot locate Claude profile")?;
    let mut document = match fs::read(&path) {
        Ok(contents) => serde_json::from_slice::<Value>(&contents)
            .with_context(|| format!("parse Claude profile at {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Value::Object(Default::default())
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read Claude profile at {}", path.display()));
        }
    };
    document
        .as_object_mut()
        .context("Claude profile must be a JSON object")?
        .insert("oauthAccount".to_owned(), profile.clone());
    let mut contents = serde_json::to_vec_pretty(&document).context("serialize Claude profile")?;
    contents.push(b'\n');
    crate::config::atomic_write_secret_for_settings(&path, &contents)
        .with_context(|| format!("write Claude profile at {}", path.display()))
}

fn profile_path() -> Option<PathBuf> {
    env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .or_else(home_dir)
        .map(|directory| directory.join(".claude.json"))
}

fn config_dir() -> Option<PathBuf> {
    env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".claude")))
}

#[cfg(target_os = "macos")]
fn default_credential_location() -> Result<CredentialLocation> {
    Ok(CredentialLocation::Keychain {
        account: env::var("USER").unwrap_or_default(),
    })
}

#[cfg(not(target_os = "macos"))]
fn default_credential_location() -> Result<CredentialLocation> {
    Ok(CredentialLocation::File(
        config_dir()
            .context("cannot locate Claude configuration directory")?
            .join(".credentials.json"),
    ))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
}

// access_token is a token of Claude Code's own sign-in that answers now: one
// that is nearly out is traded for the next, and the new pair goes back where
// Claude Code looks for it.
pub(crate) async fn access_token() -> Result<String> {
    // The pair rotates with every trade, so two requests at once would each
    // write a pair of their own and leave the other refresh token refused.
    static RENEWING: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));
    let _renewing = RENEWING.lock().await;

    let (mut auth, _) = read_credentials()
        .ok_or_else(|| anyhow!("Claude Code is signed out; run claude auth login"))?;
    if !fresh(&auth) {
        let traded = word(&auth, "refreshToken");
        if traded.is_empty() {
            bail!("Claude Code OAuth token expired; run claude auth login");
        }
        let renewed = claude_oauth::renew(&traded).await?;
        let oauth = signed_in_mut(&mut auth).context("Claude Code credentials hold no sign-in")?;
        oauth.insert("accessToken".to_owned(), Value::String(renewed.access));
        if let Some(refresh) = renewed.refresh {
            oauth.insert("refreshToken".to_owned(), Value::String(refresh));
        }
        if let Some(expires_at) = renewed.expires_at {
            oauth.insert("expiresAt".to_owned(), Value::from(expires_at));
        }
        install_login(&auth, None)?;
    }
    Ok(word(&auth, "accessToken"))
}

fn signed_in(auth: &Value) -> Option<&Map<String, Value>> {
    auth.get("claudeAiOauth").and_then(Value::as_object)
}

// signed_in_mut is where a token pair is kept, making room for one in a file
// that has none yet.
fn signed_in_mut(auth: &mut Value) -> Option<&mut Map<String, Value>> {
    let oauth = auth
        .as_object_mut()?
        .entry("claudeAiOauth")
        .or_insert_with(|| Value::Object(Default::default()));
    oauth.as_object_mut()
}

fn word(auth: &Value, key: &str) -> String {
    signed_in(auth)
        .and_then(|oauth| oauth.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn count(auth: &Value, key: &str) -> i64 {
    signed_in(auth)
        .and_then(|oauth| oauth.get(key))
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

// How long a token has still to be worth giving out: long enough for the
// request it is given for to be answered by it, rather than halfway through.
const FRESH_FOR: Duration = Duration::from_secs(5 * 60);

fn fresh(auth: &Value) -> bool {
    usable_auth(auth)
        && expiry(count(auth, "expiresAt"))
            .is_none_or(|until| SystemTime::now() + FRESH_FOR <= until)
}

// expiry is when a stored token runs out, whether it was kept as a count of
// seconds or, as Claude Code keeps it now, of milliseconds.
fn expiry(stored: i64) -> Option<SystemTime> {
    if stored == 0 {
        return None;
    }
    let since = if (-SECONDS_ONLY..SECONDS_ONLY).contains(&stored) {
        Duration::from_secs(unsigned(stored))
    } else {
        Duration::from_millis(unsigned(stored))
    };
    // a time beyond all reckoning has run out as surely as a past one
    Some(
        SystemTime::UNIX_EPOCH
            .checked_add(since)
            .unwrap_or(SystemTime::UNIX_EPOCH),
    )
}

// A count below this is too small to be milliseconds since the epoch: it is
// seconds, as Claude Code counted them before it switched.
const SECONDS_ONLY: i64 = 1_000_000_000_000;

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sign_in(expires_at: i64) -> Value {
        json!({"claudeAiOauth": {"accessToken": "a", "expiresAt": expires_at}})
    }

    fn secs_from_now(after: i64) -> i64 {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        i64::try_from(now).unwrap() + after
    }

    #[test]
    fn a_token_reads_the_same_whatever_it_was_counted_in() {
        let later = secs_from_now(60 * 60);
        assert!(fresh(&sign_in(later)), "an hour left is enough");
        assert!(
            fresh(&sign_in(later * 1_000)),
            "the same hour counted in milliseconds"
        );
        let soon = secs_from_now(60);
        assert!(!fresh(&sign_in(soon)), "a minute is inside the grace");
        assert!(!fresh(&sign_in(soon * 1_000)));
        assert!(fresh(&sign_in(0)), "no count is no running out");
    }

    #[test]
    fn a_sign_in_with_no_token_is_not_fresh_however_long_it_looks() {
        let later = secs_from_now(60 * 60);
        assert!(!fresh(&json!({"claudeAiOauth": {"accessToken": "", "expiresAt": later}})));
        assert!(!fresh(&json!({})), "and none at all");
    }

    #[test]
    fn a_count_of_no_reckoning_has_run_out() {
        assert_eq!(expiry(0), None);
        assert!(!fresh(&sign_in(-5)), "a token counted backwards");
        assert!(!fresh(&sign_in(i64::MIN)), "and one beyond counting");
        assert!(
            expiry(i64::MAX).is_some(),
            "a count past all reckoning still says when: gone"
        );
    }
}
