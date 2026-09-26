use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{codex, copilot, settings};

mod codex_oauth;
mod codex_usage;
mod copilot_usage;

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
    first: bool,
    #[serde(skip)]
    copilot_token: Option<String>,
    #[serde(skip)]
    copilot_own: bool,
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
    if let Some(auth) = codex::signed_in_auth()
        && let Some((user, plan)) = codex::identity_from_auth(&auth)
    {
        merge_active(&mut rows, "codex", user, nonempty(plan), Some(auth));
    }
    if let Some(account) = copilot::signed_in_account() {
        merge_copilot_account(&mut rows, account);
    }
    select_copilot_account(&mut rows);

    if let Some(agent) = agent_filter {
        rows.retain(|row| row.agent == agent);
    }
    codex_usage::refresh(&mut rows).await;
    copilot_usage::refresh(&mut rows).await;
    rows.sort_by_cached_key(|row| (row.agent.clone(), row.user.to_lowercase()));

    if as_json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_rows(&rows);
    }
    Ok(())
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
            first: false,
            copilot_token: Some(account.github_token),
            copilot_own: true,
        });
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
        first: false,
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
    let is_copilot = login.agent == "copilot";
    let copilot_own = is_copilot && login.auth.is_none();
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
        first: login.first,
        copilot_token,
        copilot_own,
    }
}

fn merge_active(
    rows: &mut Vec<AccountRow>,
    agent: &str,
    user: String,
    plan: Option<String>,
    auth: Option<Value>,
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
            } else {
                row.user.eq_ignore_ascii_case(&user)
            }
    }) {
        row.active = true;
        row.on = true;
        if row.plan.is_none() {
            row.plan = plan;
        }
        if let Some(auth) = auth {
            row.auth = Some(auth);
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
        first: false,
        copilot_token: None,
        copilot_own: false,
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
