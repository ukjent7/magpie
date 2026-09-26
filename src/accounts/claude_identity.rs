use std::{env, fs, path::PathBuf, time::Duration};

#[cfg(target_os = "macos")]
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command as AsyncCommand;

use anyhow::{Context, Result, bail};

use super::SavedLogin;

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
            let mut command = Command::new("security");
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

fn claude_user(email: &str, plan: &str, profile: Option<&Value>) -> String {
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
        AsyncCommand::new("claude")
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
        let output = Command::new("security")
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
