use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{codex, copilot, settings};

const USAGE: &str =
    "usage: magpie accounts [claude|codex|grok|copilot|gemini|antigravity] [--json]";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SavedLogin {
    agent: String,
    user: String,
    plan: Option<String>,
    on: bool,
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

pub(crate) fn command(args: &[String]) -> Result<()> {
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(());
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

fn print_rows(rows: &[AccountRow]) {
    if rows.is_empty() {
        println!("no accounts yet · add one: magpie accounts add <agent>");
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
