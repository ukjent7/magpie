use std::{ffi::OsString, process::ExitCode};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser};

use crate::{agent, profile, settings};

const VERSION: &str = crate::VERSION;

#[derive(Debug, Parser)]
#[command(name = "magpie", disable_help_flag = true, disable_version_flag = true)]
struct Cli {
    #[arg(short = 'h', long, action = ArgAction::SetTrue, global = true)]
    help: bool,
    #[arg(short = 'v', long, action = ArgAction::SetTrue, global = true)]
    version: bool,
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..
    )]
    args: Vec<OsString>,
}

pub async fn entry() -> ExitCode {
    match Cli::try_parse() {
        Ok(cli) => match run(cli).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("magpie: {error:#}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            ExitCode::from(u8::try_from(code).unwrap_or(2))
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    settings::migrate();

    if cli.help {
        println!("{}", usage());
        return Ok(());
    }
    if cli.version {
        println!("magpie {VERSION}");
        return Ok(());
    }

    let args = cli
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    if args
        .first()
        .is_some_and(|argument| argument.to_ascii_lowercase().starts_with("magpie:"))
    {
        return crate::provider::import_command(&args).await;
    }

    match args.as_slice() {
        [] => crate::tui::command(&[]).await,
        [command] if command == "help" => {
            println!("{}", usage());
            Ok(())
        }
        [command] if command == "version" => {
            println!("magpie {VERSION}");
            Ok(())
        }
        [command] if matches!(command.as_str(), "app" | "gui" | "tray") => {
            bail!("the Rust build does not include the desktop interface yet")
        }
        [command, rest @ ..] if command == "tui" => crate::tui::command(rest).await,
        [command] if command == "agents" => list_agents(false),
        [command] if command == "presets" => crate::provider::presets(),
        [command] if command == "sync" => {
            let providers = crate::catalog::sync_models_dev().await?;
            println!("✓ refreshed models.dev for {providers} providers");
            let refreshed = crate::provider::sync_live_models().await?;
            crate::agent::sync_catalog_models()?;
            if !refreshed.is_empty() {
                let models = refreshed
                    .iter()
                    .map(|(id, count)| format!("{id} ({count})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("✓ model lists: {models}");
            }
            Ok(())
        }
        [command, rest @ ..] if command == "usage" => crate::usage::command(rest),
        [command, rest @ ..] if command == "serve" => crate::gateway::command(rest).await,
        [command, rest @ ..] if command == "backup" => crate::backup::backup_command(rest),
        [command, rest @ ..] if command == "restore" => crate::backup::restore_command(rest),
        [command, rest @ ..] if command == "import" => crate::provider::import_command(rest).await,
        [command, rest @ ..] if command == "update" => crate::update::command(rest).await,
        [command] if command == "ls" || command == "list" => list_agents(true),
        [command] if command == "providers" => crate::provider::list(),
        [command] if command == "models" => crate::provider::models().await,
        [command] if command == "groups" => crate::groups::command(&[]),
        [command, rest @ ..] if command == "group" => crate::groups::command(rest),
        [command, rest @ ..] if command == "provider" => crate::provider::command(rest).await,
        [command, rest @ ..] if command == "profiles" => profile::list(rest),
        [command, rest @ ..] if command == "save" => profile::save(rest),
        [command, rest @ ..] if command == "use" => profile::apply(rest),
        [command, rest @ ..] if command == "rm" => profile::remove(rest),
        [name, rest @ ..] => handle_agent(name, rest),
    }
}

fn list_agents(detected_only: bool) -> Result<()> {
    let settings = settings::load();
    let agents = agent::all();
    let mut entries = agents
        .iter()
        .filter_map(|agent| {
            let detected = agent.is_detected();
            (!detected_only || detected).then_some((agent, detected))
        })
        .collect::<Vec<_>>();

    entries.sort_by_key(|(agent, _)| {
        settings
            .agent_order
            .iter()
            .position(|id| id == agent.spec.id)
            .unwrap_or(usize::MAX)
    });

    if entries.is_empty() {
        bail!("no supported agents found on this machine");
    }

    for (agent, detected) in entries {
        let hidden = settings.agents_hidden.iter().any(|id| id == agent.spec.id);
        if !detected {
            println!("  {:20} not detected", agent.spec.name);
            continue;
        }

        let values = agent.values()?;
        let fields = agent
            .spec
            .fields
            .iter()
            .filter_map(|field| {
                values
                    .iter()
                    .find(|(key, _)| *key == field.key)
                    .map(|(_, value)| value)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("{} {value}", field.label))
            })
            .collect::<Vec<_>>();
        let current = if fields.is_empty() {
            "—".to_owned()
        } else {
            fields.join("  ·  ")
        };
        let marker = if hidden { " [hidden]" } else { "" };
        println!(
            "  {:20} {}{}  {}",
            agent.spec.name,
            current,
            marker,
            agent.path.display()
        );
    }
    Ok(())
}

fn handle_agent(name: &str, args: &[String]) -> Result<()> {
    let agent = agent::find(name)?;
    match args {
        [] => {
            let values = agent.values()?;
            println!("{}  {}", agent.spec.name, agent.path.display());
            for field in agent.spec.fields {
                let value = values
                    .iter()
                    .find(|(key, _)| *key == field.key)
                    .map_or("", |(_, value)| value.as_str());
                println!(
                    "  {:12} {}",
                    field.label,
                    if value.is_empty() { "—" } else { value }
                );
            }
            Ok(())
        }
        [value] => {
            let field = if value == "default" {
                agent.spec.fields.first()
            } else {
                agent
                    .spec
                    .fields
                    .iter()
                    .skip(1)
                    .find(|field| field.choices.contains(&value.as_str()))
                    .or_else(|| agent.spec.fields.first())
            }
            .context("agent has no editable fields in the Rust migration")?;
            set_agent_field(
                &agent,
                field.key,
                if value == "default" { "" } else { value },
            )
        }
        [field_name, value] => set_agent_field(
            &agent,
            field_name,
            if value == "default" { "" } else { value },
        ),
        _ => bail!("usage: magpie <agent> [field] [value]"),
    }
}

fn set_agent_field(agent: &agent::Agent, field_name: &str, value: &str) -> Result<()> {
    let field = agent
        .spec
        .fields
        .iter()
        .find(|field| field.key == field_name || field.label == field_name)
        .with_context(|| format!("{} has no field {field_name:?}", agent.spec.name))?;
    agent.set(field.key, value)?;
    println!(
        "✓ {} {} {}",
        agent.spec.name,
        field.label,
        if value.is_empty() { "default" } else { value }
    );
    Ok(())
}

fn usage() -> String {
    [
        "magpie — one place to pick every agent's model",
        "",
        "  magpie                          open the terminal interface",
        "  magpie agents                   list every supported agent",
        "  magpie ls                       list agents detected on this machine",
        "  magpie tui                      open the terminal interface",
        "  magpie <agent>                  show its current settings",
        "  magpie <agent> <model>          set its model",
        "  magpie <agent> <field> <value>  set one field",
        "",
        "  magpie save <name>              save detected agent settings as a profile",
        "  magpie use <name>               apply a profile",
        "  magpie profiles                 list profiles",
        "  magpie rm <name>                delete a profile",
        "  magpie backup [--no-keys] [file] encrypt settings into a backup",
        "  magpie restore [--no-agents] <file> restore an encrypted backup",
        "  magpie import [-y] <link>       add a provider from a magpie:// link",
        "  magpie import apps [claude|codex] [-y] import configured providers",
        "",
        "  magpie providers                list API providers",
        "  magpie models                   list exposed provider models",
        "  magpie groups                   list routing groups",
        "  magpie group add <name> models=<m1>,<m2>",
        "  magpie group set <id> k=v…     change a routing group",
        "  magpie group rm <id>            remove a routing group",
        "  magpie presets                  list provider presets",
        "  magpie sync                     refresh the model catalog",
        "  magpie usage [today|7d|30d|all] summarize gateway tokens and cost",
        "  magpie update [check]           check for or install an update",
        "  magpie serve                    run the local API gateway",
        "  magpie provider <id>            show a provider",
        "  magpie provider add <preset> [key]",
        "  magpie provider add <name> url=<url> key=<key>",
        "  magpie provider models <id> [model ids…]",
        "  magpie provider test <id>     test every configured provider API",
        "  magpie provider icon <id> <file|name>  set a custom provider icon",
        "  magpie provider key <id> <key>  change its API key",
        "  magpie provider keys <id>       manage its API keys",
        "  magpie provider routing <id> [smart|order|rotate|usage]",
        "  magpie provider affinity <id> [auto|session|turn|off]",
        "  magpie provider fallback <id> [provider/model… | none]",
        "  magpie provider rm <id>         remove a provider",
        "",
        "  magpie --help                   show this help",
        "  magpie --version                show the version",
    ]
    .join("\n")
}
