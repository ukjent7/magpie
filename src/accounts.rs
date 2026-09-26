use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{codex, copilot, settings};

mod codex_oauth;

const USAGE: &str = "usage: magpie accounts [claude|codex|grok|copilot|gemini|antigravity] [--json] | magpie accounts add codex | magpie accounts switch|forget codex <user>";

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct SavedLogin {
    agent: String,
    user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    seen: Option<Value>,
    #[serde(skip_serializing_if = "is_false")]
    on: bool,
    auth: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct AccountRow {
    agent: String,
    user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    active: bool,
    on: bool,
    windows: Vec<Value>,
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
        return mutate_account(args);
    }

    if args
        .first()
        .is_some_and(|arg| arg.eq_ignore_ascii_case("add"))
    {
        return codex_oauth::command(args).await;
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

    let mut rows = read_saved_logins()?
        .into_iter()
        .map(account_row)
        .collect::<Vec<_>>();
    if let Some((user, plan)) = codex::signed_in_identity() {
        merge_active(&mut rows, "codex", user, nonempty(plan));
    }
    if let Some(account) = copilot::signed_in_account().filter(|account| !account.user.is_empty()) {
        merge_active(&mut rows, "copilot", account.user, None);
    }

    if let Some(agent) = agent_filter {
        rows.retain(|row| row.agent == agent);
    }
    rows.sort_by_cached_key(|row| (row.agent.clone(), row.user.to_lowercase()));

    if as_json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_rows(&rows);
    }
    Ok(())
}

fn mutate_account(args: &[String]) -> Result<()> {
    let [action, agent, user] = args else {
        bail!("{USAGE}");
    };
    let Some(agent) = canonical_agent(agent) else {
        bail!("unknown agent {agent:?}\n{USAGE}");
    };
    ensure!(
        agent == "codex",
        "Rust account switching and forgetting currently support Codex only"
    );

    match action.to_ascii_lowercase().as_str() {
        "switch" => switch_codex_account(user)?,
        "forget" => forget_codex_account(user)?,
        _ => unreachable!("mutate_account only handles switch and forget"),
    }
    if action.eq_ignore_ascii_case("switch") {
        println!("✓ codex is now signed in as {user}");
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
        auth: Some(auth),
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
    left.user.eq_ignore_ascii_case(&right.user)
}

fn account_row(login: SavedLogin) -> AccountRow {
    AccountRow {
        agent: login.agent,
        user: login.user,
        plan: login.plan.filter(|plan| !plan.is_empty()),
        active: false,
        on: login.on,
        windows: Vec::new(),
    }
}

fn merge_active(rows: &mut Vec<AccountRow>, agent: &str, user: String, plan: Option<String>) {
    if let Some(row) = rows
        .iter_mut()
        .find(|row| row.agent == agent && row.user.eq_ignore_ascii_case(&user))
    {
        row.active = true;
        row.on = true;
        if row.plan.is_none() {
            row.plan = plan;
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
        println!("{mark} {:9} {}{plan}", row.agent, row.user);
    }
    println!("● signed in · ○ in use when the signed-in account runs out");
}
