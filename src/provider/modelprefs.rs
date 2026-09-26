// A model's name is the vendor's, or models.dev's, and agents are shown it
// with the provider after it (Claude Opus 5.5 · Claude Code). The user can
// give one of a provider's models a name of their own instead (Opus 5.5): it
// is kept in settings' model_names by "<provider id>/<model id>", apart from
// the vendor's list, so a Refresh, a restart or an upgrade leaves it, and the
// same model another provider serves keeps its own. Only the name changes:
// the model's id, and where requests for it go, stay as they were.
//
// The user can also keep only some of the reasoning levels a model has (low,
// medium and high of its six): settings' model_efforts, kept the same way.
// The lists magpie hands out — the gateway's, and those it writes into the
// agents' files — offer only those, in the vendor's order; a request for
// another still reaches the vendor as it did.

use std::{
    collections::BTreeMap,
    fs,
    sync::{LazyLock, Mutex, PoisonError},
    time::SystemTime,
};

use anyhow::{Result, bail, ensure};

use crate::{agent, catalog, settings};

use super::Provider;

const USAGE: &str = concat!(
    "usage:\n",
    "  magpie model name <provider/model>             the name the model goes by\n",
    "  magpie model name <provider/model> <name>      name it so everywhere: in magpie, in the gateway's\n",
    "                                                 model list, and in the lists magpie writes into the agents\n",
    "  magpie model name <provider/model> --reset     give it back its own name\n",
    "  magpie model efforts <provider/model>          the reasoning levels it offers, and those it has\n",
    "  magpie model efforts <provider/model> <l>,<l>  offer only these of them, e.g. low,medium,high\n",
    "  magpie model efforts <provider/model> --reset  offer every level it has again\n",
    "  magpie model names                             the models you named or narrowed\n",
    "\n",
    "  Only what agents are shown changes: they still pick the model, and requests still reach it,\n",
    "  as <provider/model>. The same model from another provider keeps its own name and levels.\n",
    "\n",
    "  e.g. magpie model name claude/claude-opus-5-5 \"Opus 5.5\"\n",
    "       magpie model efforts openai/gpt-6 low,medium,high",
);

// Preferences are what the user gave the models of every provider: names,
// and the levels left to offer.
#[derive(Clone, Default)]
struct Preferences {
    names: BTreeMap<String, String>,
    efforts: BTreeMap<String, Vec<String>>,
}

// PREFS remembers them by when settings.json last changed: a gateway
// request reads every provider's models, and the file says the same thing
// until someone edits it.
static PREFS: LazyLock<Mutex<Option<(SystemTime, u64, Preferences)>>> =
    LazyLock::new(|| Mutex::new(None));

fn preferences() -> Preferences {
    let path = settings::path();
    let changed = fs::metadata(&path).and_then(|meta| Ok((meta.modified()?, meta.len())));
    let mut found = PREFS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Ok((when, len)) = &changed {
        if let Some((at, size, kept)) = found.as_ref()
            && at == when
            && size == len
        {
            return kept.clone();
        }
    }
    let saved = loaded();
    if let Ok((when, len)) = changed {
        *found = Some((when, len, saved.clone()));
    }
    saved
}

fn loaded() -> Preferences {
    let saved = settings::load();
    Preferences {
        names: saved.model_names,
        efforts: saved.model_efforts,
    }
}

// forget is what a change to the settings does to the remembered ones: the
// file may keep the same size and its time may not move, and a name the
// user just gave has to show at once.
fn forget() {
    *PREFS.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

// shown is what the user asked of these models: named as they named it, and
// offering only the levels they kept.
pub(crate) fn shown(provider_id: &str, models: Vec<catalog::Model>) -> Vec<catalog::Model> {
    let preferences = preferences();
    if preferences.names.is_empty() && preferences.efforts.is_empty() {
        return models;
    }
    models
        .into_iter()
        .map(|mut model| {
            let key = format!("{provider_id}/{}", model.id);
            if let Some(name) = preferences.names.get(&key)
                && !name.is_empty()
            {
                model.name.clone_from(name);
            }
            if let Some(kept) = preferences.efforts.get(&key) {
                model.efforts = efforts_kept(&model.efforts, kept);
            }
            model
        })
        .collect()
}

// efforts_kept is all without the levels the user didn't keep, in its own
// order; all of them when none of those kept is among them any more (the
// vendor's list changed).
pub(crate) fn efforts_kept(all: &[String], kept: &[String]) -> Vec<String> {
    if kept.is_empty() {
        return all.to_vec();
    }
    let some = all
        .iter()
        .filter(|level| kept.contains(level))
        .cloned()
        .collect::<Vec<_>>();
    if some.is_empty() { all.to_vec() } else { some }
}

// name_of is the name the user gave a provider's model, if any.
pub fn name_of(provider_id: &str, model: &str) -> Option<String> {
    named_in(&preferences().names, provider_id, model)
}

fn named_in(names: &BTreeMap<String, String>, provider_id: &str, model: &str) -> Option<String> {
    names
        .get(&format!("{provider_id}/{model}"))
        .cloned()
        .filter(|name| !name.is_empty())
}

// split_ref is "provider/model" as the provider it names, by its id now, and
// the model.
fn split_ref(reference: &str) -> Result<(Provider, String)> {
    let typed = reference.trim().trim_start_matches("magpie/");
    ensure!(
        !typed.starts_with("group/"),
        "that is a routing group, not a provider's model"
    );
    let Some((provider_id, model)) = typed.split_once('/') else {
        bail!("name a model as provider/model, not {reference:?} (magpie models lists them)");
    };
    ensure!(
        !provider_id.is_empty() && !model.is_empty(),
        "name a model as provider/model, not {reference:?} (magpie models lists them)"
    );
    Ok((super::find(provider_id)?, model.to_owned()))
}

// set_name names a provider's model, spelled "provider/model"; an empty name
// gives it back its own. The agents that keep the models in files of their
// own are told, as for any other change of the catalog.
pub fn set_name(reference: &str, name: &str) -> Result<()> {
    let (provider, model) = split_ref(reference)?;
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(
        name.chars().count() <= 80,
        "a model's name is at most 80 characters"
    );
    ensure!(
        name.is_empty() || serves(&provider, &model),
        "{} has no model {model} (magpie provider {} lists them)",
        provider.id,
        provider.id
    );
    let key = format!("{}/{}", provider.id, model);
    let mut saved = settings::load();
    if saved.model_names.get(&key).cloned().unwrap_or_default() == name {
        return Ok(());
    }
    if name.is_empty() {
        saved.model_names.remove(&key);
    } else {
        saved.model_names.insert(key, name);
    }
    settings::save(&saved)?;
    forget();
    agent::sync_catalog_models()
}

// set_efforts keeps only these of a provider's model's reasoning levels in
// the lists magpie hands out; none, or all it has, offers them all again.
// They must be levels the model has.
pub fn set_efforts(reference: &str, efforts: &[String]) -> Result<()> {
    let (provider, model) = split_ref(reference)?;
    let all = catalog::model_levels(&provider.id, provider.catalog_id(), &model);
    let mut keep: Vec<String> = Vec::new();
    for level in efforts {
        let level = level.trim().to_lowercase();
        if level.is_empty() {
            continue;
        }
        ensure!(
            all.contains(&level),
            if all.is_empty() {
                format!("{}/{} has no reasoning levels to choose from", provider.id, model)
            } else {
                format!(
                    "{}/{} has no reasoning level {level:?} (it has {})",
                    provider.id,
                    model,
                    all.join(", ")
                )
            }
        );
        if !keep.contains(&level) {
            keep.push(level);
        }
    }
    keep = efforts_kept(&all, &keep);
    if keep.len() == all.len() {
        keep.clear();
    }
    let key = format!("{}/{}", provider.id, model);
    let mut saved = settings::load();
    if saved.model_efforts.get(&key).cloned().unwrap_or_default() == keep {
        return Ok(());
    }
    if keep.is_empty() {
        saved.model_efforts.remove(&key);
    } else {
        saved.model_efforts.insert(key, keep);
    }
    settings::save(&saved)?;
    forget();
    agent::sync_catalog_models()
}

// serves reports whether model is one of the provider's, listed or exposed:
// a name given to a model it doesn't have would never be shown.
fn serves(provider: &Provider, model: &str) -> bool {
    let has = |models: Vec<catalog::Model>| models.iter().any(|known| known.id == model);
    has(catalog::available_models(
        &provider.id,
        provider.catalog_id(),
    )) || has(catalog::exposed_models(
        &provider.id,
        provider.catalog_id(),
        &provider.models,
    ))
}

// ---- the command line ------------------------------------------------------

pub fn command(args: &[String]) -> Result<()> {
    let Some(verb) = args.first() else {
        bail!("{USAGE}");
    };
    match verb.as_str() {
        "names" | "ls" | "list" => list(),
        "name" | "rename" => name(&args[1..]),
        "efforts" | "effort" | "levels" => efforts(&args[1..]),
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn name(args: &[String]) -> Result<()> {
    let [reference, rest @ ..] = args else {
        bail!("{USAGE}");
    };
    let (provider, model) = split_ref(reference)?;
    let id = format!("{}/{}", provider.id, model);
    let own = catalog::listed_name(&provider.id, provider.catalog_id(), &model);
    if rest.is_empty() {
        match name_of(&provider.id, &model) {
            Some(given) => println!("{given} · {id} · its own name is {own}"),
            None => println!("{own} · {id} · its own name"),
        }
        return Ok(());
    }
    let given = if is_reset(rest) {
        String::new()
    } else {
        rest.join(" ")
    };
    set_name(&id, &given)?;
    if given.is_empty() {
        println!("✓ {id} is called {own} again");
    } else {
        println!(
            "✓ {id} is called {}",
            given.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }
    Ok(())
}

fn efforts(args: &[String]) -> Result<()> {
    let [reference, rest @ ..] = args else {
        bail!("{USAGE}");
    };
    let (provider, model) = split_ref(reference)?;
    let id = format!("{}/{}", provider.id, model);
    let all = catalog::model_levels(&provider.id, provider.catalog_id(), &model);
    if rest.is_empty() {
        if all.is_empty() {
            println!("{id} has no reasoning levels");
            return Ok(());
        }
        let kept = preferences().efforts.get(&id).cloned().unwrap_or_default();
        let line = all
            .iter()
            .map(|level| {
                if kept.is_empty() || kept.contains(level) {
                    level.clone()
                } else {
                    format!("{level} (left out)")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        println!("{line}");
        println!(
            "{}",
            if kept.is_empty() {
                "all offered"
            } else {
                "only the plain ones are offered · --reset offers them all"
            }
        );
        return Ok(());
    }
    let mut levels: Vec<String> = Vec::new();
    if !is_reset(rest) {
        for arg in rest {
            levels.extend(
                arg.split([',', ' ', '/'])
                    .filter(|level| !level.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    set_efforts(&id, &levels)?;
    let kept = preferences().efforts.get(&id).cloned().unwrap_or_default();
    if kept.is_empty() {
        println!("✓ {id} offers every level it has: {}", all.join(", "));
    } else {
        println!("✓ {id} offers {}", kept.join(", "));
    }
    Ok(())
}

fn list() -> Result<()> {
    let preferences = preferences();
    let mut keys = preferences
        .names
        .keys()
        .chain(preferences.efforts.keys())
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        println!("no model is named or narrowed yet · magpie model name <provider/model> <name>");
        return Ok(());
    }
    let width = keys.iter().map(|key| key.len()).max().unwrap_or_default();
    for key in keys {
        let mut line = format!("  {key:<width$}");
        if let Some(name) = preferences.names.get(*key).filter(|name| !name.is_empty()) {
            line.push_str(&format!("  {name}"));
        }
        if let Some(levels) = preferences.efforts.get(*key).filter(|levels| !levels.is_empty()) {
            line.push_str(&format!("  {}", levels.join("/")));
        }
        println!("{line}");
    }
    Ok(())
}

fn is_reset(args: &[String]) -> bool {
    matches!(args, [one] if matches!(one.as_str(), "--reset" | "-r" | "--default" | "default" | "reset"))
}
