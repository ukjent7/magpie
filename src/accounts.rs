use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{codex, copilot, settings};

mod claude_identity;
mod claude_oauth;
mod claude_usage;
mod codex_oauth;
mod codex_usage;
mod copilot_oauth;
mod copilot_usage;
mod grok_identity;
mod grok_oauth;
mod oauth;

const USAGE: &str = "usage: magpie accounts [claude|codex|grok|copilot|gemini|antigravity] [--json] | magpie accounts add claude|codex|grok|copilot | magpie accounts switch|forget claude|codex|grok|copilot <user>";

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct SavedLogin {
    agent: String,
    user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    home: Option<PathBuf>,
    seen: Option<Value>,
    #[serde(skip_serializing_if = "is_false")]
    on: bool,
    #[serde(skip_serializing_if = "is_false")]
    first: bool,
    auth: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize)]
struct AccountRow {
    agent: String,
    user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    active: bool,
    on: bool,
    windows: Vec<AccountWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip)]
    auth: Option<Value>,
    #[serde(skip)]
    profile: Option<Value>,
    #[serde(skip)]
    first: bool,
    #[serde(skip)]
    copilot_token: Option<String>,
    #[serde(skip)]
    copilot_own: bool,
    #[serde(skip)]
    grok_home: Option<PathBuf>,
    #[serde(skip)]
    grok_own: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountWindow {
    name: String,
    used: f64,
    remaining: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    display: String,
}

pub(crate) async fn command(args: &[String]) -> Result<()> {
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }

    if args
        .first()
        .is_some_and(|arg| arg.eq_ignore_ascii_case("switch") || arg.eq_ignore_ascii_case("forget"))
    {
        return mutate_account(args).await;
    }

    if args
        .first()
        .is_some_and(|arg| arg.eq_ignore_ascii_case("add"))
    {
        return match args.get(1).map(String::as_str) {
            Some(agent) if agent.eq_ignore_ascii_case("claude") => {
                claude_oauth::command(args).await
            }
            Some(agent) if agent.eq_ignore_ascii_case("codex") => codex_oauth::command(args).await,
            Some(agent) if agent.eq_ignore_ascii_case("copilot") => {
                copilot_oauth::command(args).await
            }
            Some(agent) if agent.eq_ignore_ascii_case("grok") => grok_oauth::command(args).await,
            _ => bail!("usage: magpie accounts add claude|codex|grok|copilot"),
        };
    }

    if let Some(subcommand) = args.first().filter(|arg| {
        matches!(
            arg.to_ascii_lowercase().as_str(),
            "add" | "switch" | "forget" | "project"
        )
    }) {
        bail!("`magpie accounts {subcommand}` is not implemented in the Rust CLI yet");
    }

    let mut agent_filter = None;
    let mut as_json = false;
    for argument in args {
        if argument.eq_ignore_ascii_case("--json") {
            as_json = true;
            continue;
        }
        let Some(agent) = canonical_agent(argument) else {
            bail!("unknown accounts option or agent {argument:?}\n{USAGE}");
        };
        agent_filter = Some(agent);
    }

    let mut rows = collected_rows(agent_filter.as_deref()).await?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_rows(&rows);
    }
    Ok(())
}

async fn collected_rows(agent_filter: Option<&str>) -> Result<Vec<AccountRow>> {
    let mut rows = read_saved_logins()?
        .into_iter()
        .map(account_row)
        .collect::<Vec<_>>();
    if (agent_filter.is_none() || agent_filter == Some("claude"))
        && let Some(login) = claude_identity::live_login().await
    {
        merge_active(
            &mut rows,
            "claude",
            login.user,
            login.plan,
            login.auth,
            login.profile,
        );
    }
    if (agent_filter.is_none() || agent_filter == Some("grok"))
        && let Some(login) = grok_identity::live_login()
    {
        merge_grok_account(&mut rows, login);
    }
    if let Some(auth) = codex::signed_in_auth()
        && let Some((user, plan)) = codex::identity_from_auth(&auth)
    {
        merge_active(&mut rows, "codex", user, nonempty(plan), Some(auth), None);
    }
    if (agent_filter.is_none() || agent_filter == Some("copilot"))
        && let Some(account) = copilot::signed_in_account()
    {
        merge_copilot_account(&mut rows, account);
    }
    select_grok_account(&mut rows);
    select_copilot_account(&mut rows);

    if let Some(agent) = agent_filter {
        rows.retain(|row| row.agent == agent);
    }
    claude_usage::refresh(&mut rows).await;
    codex_usage::refresh(&mut rows).await;
    copilot_usage::refresh(&mut rows).await;
    rows.sort_by_cached_key(|row| (row.agent.clone(), row.user.to_lowercase()));
    Ok(rows)
}

pub(crate) async fn quota_subscriptions() -> Vec<crate::quota::Quota> {
    let Ok(rows) = collected_rows(None).await else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|row| {
            crate::quota::Quota::subscription(
                row.agent,
                row.user.clone(),
                row.plan.clone().unwrap_or_default(),
                row.user,
                row.windows
                    .into_iter()
                    .map(|window| {
                        crate::quota::QuotaSpan::new(
                            window.name,
                            window.used,
                            window.remaining,
                            window.resets_at,
                            window.display,
                        )
                    })
                    .collect(),
                row.error.unwrap_or_default(),
            )
        })
        .collect()
}

fn merge_copilot_account(rows: &mut Vec<AccountRow>, account: copilot::Account) {
    let user = if account.user.is_empty() {
        "GitHub".to_owned()
    } else {
        account.user
    };
    let has_own = rows
        .iter()
        .any(|row| row.agent == "copilot" && row.copilot_own);
    if has_own {
        for row in rows
            .iter_mut()
            .filter(|row| row.agent == "copilot" && row.copilot_own)
        {
            row.user.clone_from(&user);
            row.copilot_token = Some(account.github_token.clone());
        }
    } else {
        rows.push(AccountRow {
            agent: "copilot".to_owned(),
            user: user.clone(),
            plan: None,
            active: false,
            on: false,
            windows: Vec::new(),
            error: None,
            auth: None,
            profile: None,
            first: false,
            copilot_token: Some(account.github_token),
            copilot_own: true,
            grok_home: None,
            grok_own: false,
        });
    }
}

fn merge_grok_account(rows: &mut Vec<AccountRow>, login: SavedLogin) {
    let home = grok_identity::home_dir();
    if let Some(row) = rows
        .iter_mut()
        .find(|row| row.agent == "grok" && row.grok_own)
    {
        row.user = login.user;
        row.plan = login.plan;
        row.grok_home = home;
        return;
    }
    rows.push(AccountRow {
        agent: "grok".to_owned(),
        user: login.user,
        plan: login.plan,
        active: true,
        on: true,
        windows: Vec::new(),
        error: None,
        auth: None,
        profile: None,
        first: false,
        copilot_token: None,
        copilot_own: false,
        grok_home: home,
        grok_own: true,
    });
}

fn select_grok_account(rows: &mut Vec<AccountRow>) {
    rows.retain(|row| {
        row.agent != "grok"
            || row
                .grok_home
                .as_deref()
                .and_then(grok_identity::read_credential)
                .is_some_and(|credential| credential.email.eq_ignore_ascii_case(&row.user))
    });
    let active = rows
        .iter()
        .rposition(|row| row.agent == "grok" && row.first)
        .or_else(|| {
            rows.iter()
                .position(|row| row.agent == "grok" && row.grok_own)
        })
        .or_else(|| rows.iter().position(|row| row.agent == "grok"));
    for row in rows.iter_mut().filter(|row| row.agent == "grok") {
        row.active = false;
    }
    if let Some(active) = active {
        rows[active].active = true;
        rows[active].on = true;
    }
}

fn select_copilot_account(rows: &mut Vec<AccountRow>) {
    rows.retain(|row| row.agent != "copilot" || row.copilot_token.is_some());
    let active = rows
        .iter()
        .rposition(|row| row.agent == "copilot" && row.first)
        .or_else(|| {
            rows.iter()
                .position(|row| row.agent == "copilot" && row.copilot_own)
        })
        .or_else(|| rows.iter().position(|row| row.agent == "copilot"));
    for row in rows.iter_mut().filter(|row| row.agent == "copilot") {
        row.active = false;
    }
    if let Some(active) = active {
        rows[active].active = true;
        rows[active].on = true;
    }
}

async fn mutate_account(args: &[String]) -> Result<()> {
    let [action, agent, user] = args else {
        bail!("{USAGE}");
    };
    let Some(agent) = canonical_agent(agent) else {
        bail!("unknown agent {agent:?}\n{USAGE}");
    };
    ensure!(
        matches!(agent, "claude" | "codex" | "copilot" | "grok"),
        "Rust account switching and forgetting currently support Claude Code, Codex, Grok, and Copilot"
    );

    match (action.to_ascii_lowercase().as_str(), agent) {
        ("switch", "codex") => switch_codex_account(user)?,
        ("forget", "codex") => forget_codex_account(user)?,
        ("switch", "claude") => switch_claude_account(user).await?,
        ("forget", "claude") => forget_claude_account(user).await?,
        ("switch", "copilot") => switch_copilot_account(user)?,
        ("forget", "copilot") => forget_copilot_account(user)?,
        ("switch", "grok") => switch_grok_account(user)?,
        ("forget", "grok") => forget_grok_account(user)?,
        _ => unreachable!("mutate_account only handles switch and forget"),
    }
    if action.eq_ignore_ascii_case("switch") {
        println!("✓ {agent} is now signed in as {user}");
    } else {
        println!("✓ forgot {user}");
    }
    Ok(())
}

fn canonical_agent(argument: &str) -> Option<&'static str> {
    match argument.to_ascii_lowercase().as_str() {
        "claude" | "cc" => Some("claude"),
        "codex" => Some("codex"),
        "grok" => Some("grok"),
        "copilot" => Some("copilot"),
        "gemini" | "gemini-cli" => Some("gemini"),
        "antigravity" | "ag" => Some("antigravity"),
        _ => None,
    }
}

fn logins_path() -> PathBuf {
    settings::providers_path().with_file_name("logins.json")
}

fn read_saved_logins() -> Result<Vec<SavedLogin>> {
    let path = logins_path();
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&contents).with_context(|| format!("parse {}", path.display()))
}

fn write_saved_logins(logins: &mut [SavedLogin]) -> Result<()> {
    logins.sort_by_cached_key(|login| (login.agent.clone(), login.user.to_lowercase()));
    let path = logins_path();
    let mut contents = serde_json::to_vec_pretty(logins).context("serialize saved accounts")?;
    contents.push(b'\n');
    crate::config::atomic_write_secret_for_settings(&path, &contents)
        .with_context(|| format!("write {}", path.display()))
}

fn switch_copilot_account(user: &str) -> Result<()> {
    let mut logins = read_saved_logins()?;
    let own_user = copilot_live_user();
    let own_target = own_user
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(user));
    let target_index = logins.iter().position(|login| {
        login.agent == "copilot"
            && login.user.eq_ignore_ascii_case(user)
            && login
                .auth
                .as_ref()
                .and_then(|auth| auth.get("oauth_token"))
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty())
    });
    ensure!(
        target_index.is_some() || own_target,
        "no saved Copilot account {user:?}"
    );
    let active_user = copilot_active_user(&logins, own_user.as_deref());
    if let Some(active) = active_user.filter(|active| !active.eq_ignore_ascii_case(user)) {
        if own_user
            .as_deref()
            .is_some_and(|own| own.eq_ignore_ascii_case(&active))
        {
            remember_copilot_own(&mut logins, &active, true, true)?;
        } else if let Some(login) = logins
            .iter_mut()
            .find(|login| login.agent == "copilot" && login.user.eq_ignore_ascii_case(&active))
        {
            login.on = true;
        }
    }
    for login in logins.iter_mut().filter(|login| login.agent == "copilot") {
        login.first = false;
    }
    if own_target {
        remember_copilot_own(&mut logins, user, true, true)?;
    } else if let Some(index) = target_index {
        logins[index].first = true;
        logins[index].on = true;
    }
    write_saved_logins(&mut logins)
}

fn forget_copilot_account(user: &str) -> Result<()> {
    if copilot_live_user().is_some_and(|own| own.eq_ignore_ascii_case(user)) {
        bail!("{user} is signed in to the Copilot editor or CLI; sign out there");
    }
    let mut logins = read_saved_logins()?;
    let own_user = copilot_live_user();
    let active = copilot_active_user(&logins, own_user.as_deref());
    ensure!(
        !active
            .as_deref()
            .is_some_and(|active| active.eq_ignore_ascii_case(user)),
        "magpie uses {user} first; put another account first"
    );
    let original_len = logins.len();
    logins.retain(|login| {
        !(login.agent == "copilot"
            && login.user.eq_ignore_ascii_case(user)
            && login
                .auth
                .as_ref()
                .and_then(|auth| auth.get("oauth_token"))
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty()))
    });
    ensure!(
        original_len != logins.len(),
        "no saved Copilot account {user:?}"
    );
    write_saved_logins(&mut logins)
}

fn switch_grok_account(user: &str) -> Result<()> {
    let mut logins = read_saved_logins()?;
    let own_user = grok_identity::live_login().map(|login| login.user);
    let own_target = own_user
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(user));
    let target_index = logins.iter().position(|login| {
        login.agent == "grok"
            && login.user.eq_ignore_ascii_case(user)
            && grok_saved_login_matches(login)
    });
    ensure!(
        target_index.is_some() || own_target,
        "no saved Grok account {user:?}"
    );
    let active_user = grok_active_user(&logins, own_user.as_deref());
    if let Some(active) = active_user.filter(|active| !active.eq_ignore_ascii_case(user)) {
        if own_user
            .as_deref()
            .is_some_and(|own| own.eq_ignore_ascii_case(&active))
        {
            remember_grok_own(&mut logins, &active, true, true)?;
        } else if let Some(login) = logins
            .iter_mut()
            .find(|login| login.agent == "grok" && login.user.eq_ignore_ascii_case(&active))
        {
            login.on = true;
        }
    }
    for login in logins.iter_mut().filter(|login| login.agent == "grok") {
        login.first = false;
    }
    if own_target {
        remember_grok_own(&mut logins, user, true, true)?;
    } else if let Some(index) = target_index {
        logins[index].first = true;
        logins[index].on = true;
    }
    write_saved_logins(&mut logins)
}

fn forget_grok_account(user: &str) -> Result<()> {
    let own_user = grok_identity::live_login().map(|login| login.user);
    if own_user
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(user))
    {
        bail!("Grok is signed in as {user} in its CLI; sign out there first");
    }
    let mut logins = read_saved_logins()?;
    let active = grok_active_user(&logins, own_user.as_deref());
    ensure!(
        !active
            .as_deref()
            .is_some_and(|active| active.eq_ignore_ascii_case(user)),
        "magpie uses {user} first; put another account first"
    );
    let index = logins
        .iter()
        .position(|login| {
            login.agent == "grok"
                && login.user.eq_ignore_ascii_case(user)
                && grok_saved_login_matches(login)
        })
        .with_context(|| format!("no saved Grok account {user:?}"))?;
    let home = logins.remove(index).home;
    write_saved_logins(&mut logins)?;
    if let Some(home) = home {
        let _ = grok_identity::remove_login_home(&home);
    }
    Ok(())
}

fn grok_active_user(logins: &[SavedLogin], own_user: Option<&str>) -> Option<String> {
    logins
        .iter()
        .rfind(|login| login.first && grok_login_usable(login, own_user))
        .map(|login| login.user.clone())
        .or_else(|| own_user.map(str::to_owned))
        .or_else(|| {
            logins
                .iter()
                .find(|login| grok_login_usable(login, own_user))
                .map(|login| login.user.clone())
        })
}

fn grok_login_usable(login: &SavedLogin, own_user: Option<&str>) -> bool {
    login.agent == "grok"
        && (grok_saved_login_matches(login)
            || (login.home.is_none()
                && own_user.is_some_and(|user| login.user.eq_ignore_ascii_case(user))))
}

fn grok_saved_login_matches(login: &SavedLogin) -> bool {
    login.home.as_deref().is_some_and(|home| {
        grok_identity::read_credential(home)
            .is_some_and(|credential| credential.email.eq_ignore_ascii_case(&login.user))
    })
}

fn remember_grok_own(
    logins: &mut Vec<SavedLogin>,
    user: &str,
    on: bool,
    first: bool,
) -> Result<()> {
    if let Some(login) = logins.iter_mut().find(|login| {
        login.agent == "grok" && login.user.eq_ignore_ascii_case(user) && login.home.is_none()
    }) {
        login.on |= on;
        login.first = first;
        return Ok(());
    }
    if logins
        .iter()
        .any(|login| login.agent == "grok" && login.user.eq_ignore_ascii_case(user))
    {
        return Ok(());
    }
    let seen = Value::String(
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format account timestamp")?,
    );
    logins.push(SavedLogin {
        agent: "grok".to_owned(),
        user: user.to_owned(),
        seen: Some(seen),
        on,
        first,
        ..SavedLogin::default()
    });
    Ok(())
}

fn copilot_active_user(logins: &[SavedLogin], own_user: Option<&str>) -> Option<String> {
    logins
        .iter()
        .rfind(|login| login.first && copilot_login_usable(login, own_user))
        .map(|login| login.user.clone())
        .or_else(|| own_user.map(str::to_owned))
        .or_else(|| {
            logins
                .iter()
                .find(|login| copilot_login_usable(login, own_user))
                .map(|login| login.user.clone())
        })
}

fn copilot_login_usable(login: &SavedLogin, own_user: Option<&str>) -> bool {
    login.agent == "copilot"
        && (login
            .auth
            .as_ref()
            .and_then(|auth| auth.get("oauth_token"))
            .and_then(Value::as_str)
            .is_some_and(|token| !token.is_empty())
            || own_user.is_some_and(|user| login.user.eq_ignore_ascii_case(user)))
}

fn copilot_live_user() -> Option<String> {
    copilot::signed_in_account().map(|account| {
        if account.user.is_empty() {
            "GitHub".to_owned()
        } else {
            account.user
        }
    })
}

fn remember_copilot_own(
    logins: &mut Vec<SavedLogin>,
    user: &str,
    on: bool,
    first: bool,
) -> Result<()> {
    if let Some(login) = logins.iter_mut().find(|login| {
        login.agent == "copilot" && login.user.eq_ignore_ascii_case(user) && login.auth.is_none()
    }) {
        login.on |= on;
        login.first = first;
        return Ok(());
    }
    if logins
        .iter()
        .any(|login| login.agent == "copilot" && login.user.eq_ignore_ascii_case(user))
    {
        return Ok(());
    }
    let seen = Value::String(
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format account timestamp")?,
    );
    logins.push(SavedLogin {
        agent: "copilot".to_owned(),
        user: user.to_owned(),
        seen: Some(seen),
        on,
        first,
        ..SavedLogin::default()
    });
    Ok(())
}

fn switch_codex_account(user: &str) -> Result<()> {
    let mut logins = read_saved_logins()?;
    let index = logins
        .iter()
        .rposition(|login| login.agent == "codex" && login.user.eq_ignore_ascii_case(user))
        .with_context(|| format!("no saved Codex account {user:?}"))?;
    let target = logins[index].clone();
    let target_user = target.user.clone();
    let auth = target
        .auth
        .as_ref()
        .context("the saved Codex sign-in has no credentials")?;
    ensure!(
        usable_codex_auth(auth),
        "the saved Codex sign-in is unreadable"
    );

    let current = live_codex_login()?;
    if current
        .as_ref()
        .is_some_and(|current| same_login(current, &target))
    {
        return Ok(());
    }

    if let Some(mut current) = current {
        current.on = target.on;
        upsert_login(&mut logins, current);
        for login in &mut logins {
            if login.agent == "codex" && login.user.eq_ignore_ascii_case(&target_user) {
                login.on = target.on;
            }
        }
        write_saved_logins(&mut logins)?;
    }

    let auth_file = codex::auth_file_path().context("cannot locate Codex auth.json")?;
    let mut contents = serde_json::to_vec_pretty(auth).context("serialize saved Codex sign-in")?;
    contents.push(b'\n');
    crate::config::atomic_write_secret_for_settings(&auth_file, &contents)
        .with_context(|| format!("install Codex sign-in at {}", auth_file.display()))
}

fn forget_codex_account(user: &str) -> Result<()> {
    if codex::signed_in_identity().is_some_and(|(active, _)| active.eq_ignore_ascii_case(user)) {
        bail!("Codex is signed in to {user} now; switch to another account first");
    }

    let mut logins = read_saved_logins()?;
    let original_len = logins.len();
    logins.retain(|login| !(login.agent == "codex" && login.user.eq_ignore_ascii_case(user)));
    ensure!(
        original_len != logins.len(),
        "no saved Codex account {user:?}"
    );
    write_saved_logins(&mut logins)
}

async fn switch_claude_account(user: &str) -> Result<()> {
    let mut logins = read_saved_logins()?;
    let index = logins
        .iter()
        .rposition(|login| login.agent == "claude" && login.user.eq_ignore_ascii_case(user))
        .with_context(|| format!("no saved Claude account {user:?}"))?;
    let target = logins[index].clone();
    let auth = target
        .auth
        .as_ref()
        .context("the saved Claude Code sign-in has no credentials")?;
    ensure!(
        claude_identity::usable_auth(auth),
        "the saved Claude Code sign-in is unreadable"
    );

    let current = claude_identity::live_login().await;
    if current
        .as_ref()
        .is_some_and(|current| same_login(current, &target))
    {
        return Ok(());
    }

    if let Some(mut current) = current {
        current.on = target.on;
        current.seen = Some(Value::String(
            OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .context("format account timestamp")?,
        ));
        let current_user = current.user.clone();
        upsert_login(&mut logins, current);
        for login in &mut logins {
            if login.agent == "claude" && login.user.eq_ignore_ascii_case(&current_user) {
                login.on = target.on;
            }
        }
        write_saved_logins(&mut logins)?;
    }

    claude_identity::install_login(auth, target.profile.as_ref())
}

async fn forget_claude_account(user: &str) -> Result<()> {
    if claude_identity::live_login()
        .await
        .is_some_and(|active| active.user.eq_ignore_ascii_case(user))
    {
        bail!("Claude is signed in to {user} now; switch to another account first");
    }

    let mut logins = read_saved_logins()?;
    let original_len = logins.len();
    logins.retain(|login| !(login.agent == "claude" && login.user.eq_ignore_ascii_case(user)));
    ensure!(
        original_len != logins.len(),
        "no saved Claude account {user:?}"
    );
    write_saved_logins(&mut logins)
}

fn live_codex_login() -> Result<Option<SavedLogin>> {
    let Some(auth_file) = codex::signed_in_auth_file() else {
        return Ok(None);
    };
    let contents = fs::read(&auth_file)
        .with_context(|| format!("read active Codex sign-in at {}", auth_file.display()))?;
    let auth: Value = serde_json::from_slice(&contents).context("parse active Codex sign-in")?;
    let Some((user, plan)) = codex::signed_in_identity() else {
        return Ok(None);
    };
    let seen = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format account timestamp")?;
    Ok(Some(SavedLogin {
        agent: "codex".to_owned(),
        user,
        plan: nonempty(plan),
        seen: Some(Value::String(seen)),
        on: false,
        first: false,
        auth: Some(auth),
        profile: None,
        home: None,
        extra: BTreeMap::new(),
    }))
}

fn usable_codex_auth(auth: &Value) -> bool {
    auth.get("auth_mode").and_then(Value::as_str) != Some("apikey")
        && auth
            .pointer("/tokens/access_token")
            .and_then(Value::as_str)
            .is_some_and(|token| !token.is_empty())
}

fn upsert_login(logins: &mut Vec<SavedLogin>, mut login: SavedLogin) {
    if let Some(saved) = logins.iter_mut().find(|saved| same_login(saved, &login)) {
        login.on |= saved.on;
        login.extra = std::mem::take(&mut saved.extra);
        *saved = login;
    } else {
        logins.push(login);
    }
}

fn same_login(left: &SavedLogin, right: &SavedLogin) -> bool {
    if left.agent != right.agent {
        return false;
    }
    if left.agent == "codex"
        && let (Some(left_auth), Some(right_auth)) = (&left.auth, &right.auth)
        && let (Some(left_id), Some(right_id)) = (
            codex::account_id_from_auth(left_auth),
            codex::account_id_from_auth(right_auth),
        )
    {
        return left_id == right_id;
    }
    if left.agent == "claude"
        && let (Some(left_profile), Some(right_profile)) = (&left.profile, &right.profile)
        && let Some(same) = claude_identity::same_account(left_profile, right_profile)
    {
        return same;
    }
    left.user.eq_ignore_ascii_case(&right.user)
}

fn account_row(login: SavedLogin) -> AccountRow {
    let is_copilot = login.agent == "copilot";
    let copilot_own = is_copilot && login.auth.is_none();
    let is_grok = login.agent == "grok";
    let grok_own = is_grok && login.home.is_none();
    let grok_home = login.home.clone().or_else(|| {
        if is_grok {
            grok_identity::home_dir()
        } else {
            None
        }
    });
    let user = if is_copilot && login.user.is_empty() {
        "GitHub".to_owned()
    } else {
        login.user
    };
    let copilot_token = if is_copilot {
        login
            .auth
            .as_ref()
            .and_then(|auth| auth.get("oauth_token"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    } else {
        None
    };
    AccountRow {
        agent: login.agent,
        user,
        plan: login.plan.filter(|plan| !plan.is_empty()),
        active: false,
        on: login.on,
        windows: Vec::new(),
        error: None,
        auth: login.auth,
        profile: login.profile,
        first: login.first,
        copilot_token,
        copilot_own,
        grok_home,
        grok_own,
    }
}

fn merge_active(
    rows: &mut Vec<AccountRow>,
    agent: &str,
    user: String,
    plan: Option<String>,
    auth: Option<Value>,
    profile: Option<Value>,
) {
    if let Some(row) = rows.iter_mut().find(|row| {
        row.agent == agent
            && if agent == "codex"
                && let (Some(left), Some(right)) = (&row.auth, &auth)
                && let (Some(left_id), Some(right_id)) = (
                    codex::account_id_from_auth(left),
                    codex::account_id_from_auth(right),
                )
            {
                left_id == right_id
            } else if agent == "claude"
                && let (Some(left), Some(right)) = (&row.profile, &profile)
                && let Some(same) = claude_identity::same_account(left, right)
            {
                same
            } else {
                row.user.eq_ignore_ascii_case(&user)
            }
    }) {
        row.active = true;
        row.on = true;
        if plan.is_some() {
            row.plan = plan;
        }
        if let Some(auth) = auth {
            row.auth = Some(auth);
        }
        if let Some(profile) = profile {
            row.profile = Some(profile);
        }
        return;
    }
    rows.push(AccountRow {
        agent: agent.to_owned(),
        user,
        plan,
        active: true,
        on: true,
        windows: Vec::new(),
        error: None,
        auth,
        profile,
        first: false,
        copilot_token: None,
        copilot_own: false,
        grok_home: None,
        grok_own: false,
    });
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn is_false(value: &bool) -> bool {
    !value
}

fn print_rows(rows: &[AccountRow]) {
    if rows.is_empty() {
        println!("no accounts yet");
        return;
    }

    for row in rows {
        let mark = if row.active {
            "●"
        } else if row.on {
            "○"
        } else {
            " "
        };
        let plan = row
            .plan
            .as_deref()
            .map(|plan| format!(" · {plan}"))
            .unwrap_or_default();
        let mut line = format!("{mark} {:9} {}{plan}", row.agent, row.user);
        for window in &row.windows {
            let name = window
                .name
                .replace(" hours", "h")
                .replace(" hour", "h")
                .replace(" days", "d")
                .replace(" day", "d");
            line.push_str(&format!("  {name} {:.0}%", window.used));
            if !window.display.is_empty() {
                line.push_str(&format!(" ({})", window.display));
            }
            if let Some(reset) = window.resets_at.as_deref().and_then(short_until) {
                line.push_str(&format!(" ↻{reset}"));
            }
        }
        if let Some(error) = &row.error {
            line.push_str(&format!("  {error}"));
        }
        println!("{line}");
    }
    println!("● signed in · ○ in use when the signed-in account runs out");
}

fn short_until(reset_at: &str) -> Option<String> {
    let reset_at = OffsetDateTime::parse(reset_at, &Rfc3339).ok()?;
    let seconds = (reset_at - OffsetDateTime::now_utc()).whole_seconds();
    let remaining = match seconds {
        value if value <= 0 => return Some("now".to_owned()),
        value if value < 3600 => format!("{}m", value / 60),
        value if value < 48 * 3600 => format!("{}h{}m", value / 3600, (value % 3600) / 60),
        value => format!("{}d{}h", value / 86400, (value % 86400) / 3600),
    };
    Some(remaining)
}
