use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use crate::{agent, catalog, settings};

use super::{Group, load, slug, store};

// Rename gives a provider another id, the one its models are picked by
// (<id>/<model>). What magpie keeps that names it by id follows: the routing
// groups it is in and their rules and classifier, the other providers'
// fallbacks, and which agents are shown it. The old id stays with it (was), so
// an agent still running on a config that names it the old way reaches it, and
// its usage so far is counted with it; the agents' own files are the agent
// package's to rewrite (agent::rename_provider). A signed-in account keeps the
// id of its agent.

// ACCOUNT_IDS are the ids of the subscriptions magpie can list: a provider
// taking one would hide the subscription once its user signs in.
const ACCOUNT_IDS: &[&str] = &[
    "antigravity",
    "claude",
    "codex",
    "copilot",
    "cursor",
    "devin",
    "gemini",
    "grok",
    "kiro",
    "zcode",
];

pub fn rename(from: &str, to: &str) -> Result<()> {
    let from = from.trim().to_lowercase();
    let to = to.trim().to_lowercase();
    ensure!(
        !to.is_empty() && to == slug(&to),
        "a provider's id must be lowercase letters, digits and dashes, not {to:?}"
    );
    if to == from {
        return Ok(());
    }
    if super::find(&from).is_ok_and(|provider| provider.account.is_some())
        || ACCOUNT_IDS.contains(&from.as_str())
    {
        bail!("{from} is a subscription: its id is its agent's");
    }
    ensure!(
        to != "magpie",
        "\"magpie\" is what agents call the gateway itself; pick another id"
    );
    ensure!(
        to != "group",
        "\"group\" starts the ids of routing groups; pick another id"
    );
    ensure!(
        !ACCOUNT_IDS.contains(&to.as_str()),
        "{to:?} is the id of the {to} subscription; pick another"
    );

    let mut file = load()?;
    let index = file
        .providers
        .iter()
        .position(|provider| provider.id == from)
        .with_context(|| format!("no provider {from:?}"))?;
    ensure!(
        !file.providers.iter().any(|provider| provider.id == to),
        "there is a provider {to:?} already"
    );
    // an id another provider once had is this one's now
    for provider in &mut file.providers {
        provider.was.retain(|was| *was != to);
    }
    let provider = &mut file.providers[index];
    provider.id.clone_from(&to);
    if !provider.was.contains(&from) {
        provider.was.push(from.clone());
    }
    for provider in &mut file.providers {
        for reference in &mut provider.fallback {
            let moved = renamed_in(reference, &from, &to);
            *reference = moved;
        }
    }
    for group in &mut file.groups {
        rename_group(group, &from, &to)?;
    }
    // the vendor's list last fetched goes with it
    catalog::rename_live(&from, &to);
    store(file)?;

    let mut saved = settings::load();
    let mut changed = move_keys(&mut saved.model_names, &from, &to);
    changed |= move_keys(&mut saved.model_efforts, &from, &to);
    for shown in saved.visible.values_mut() {
        for name in shown {
            if name.eq_ignore_ascii_case(&from) {
                *name = to.clone();
                changed = true;
            }
        }
    }
    if changed {
        settings::save(&saved)?;
    }
    Ok(())
}

// renamed_in is a model id ("provider/model", "magpie/provider/model") with
// the provider `from` named `to` instead.
fn renamed_in(reference: &str, from: &str, to: &str) -> String {
    let (prefix, rest) = match reference.strip_prefix("magpie/") {
        Some(rest) => ("magpie/", rest),
        None => ("", reference),
    };
    match rest.strip_prefix(&format!("{from}/")) {
        Some(model) => format!("{prefix}{to}/{model}"),
        None => reference.to_owned(),
    }
}

// renamed is ref with the provider it names by an id the provider had named
// by the id it has now.
pub(crate) fn renamed_ref(reference: &str) -> String {
    for (was, now) in renamed() {
        let moved = renamed_in(reference, &was, &now);
        if moved != reference {
            return moved;
        }
    }
    reference.to_owned()
}

// renamed maps the ids providers had to the ones they have now.
fn renamed() -> Vec<(String, String)> {
    load()
        .unwrap_or_default()
        .providers
        .into_iter()
        .flat_map(|provider| {
            let id = provider.id;
            provider.was.into_iter().map(move |was| (was, id.clone()))
        })
        .collect()
}

// rename_group moves the members, the rules' models and the classifier a
// renamed provider is among. The rules ride along with the group unexamined,
// so they are rewritten as the JSON they are kept in.
fn rename_group(group: &mut Group, from: &str, to: &str) -> Result<()> {
    for member in &mut group.members {
        let moved = renamed_in(member, from, to);
        *member = moved;
    }
    let mut value = serde_json::to_value(&*group).context("read the group's saved fields")?;
    if let Some(rules) = value.get_mut("rules").and_then(Value::as_array_mut) {
        for rule in rules {
            let Some(Value::String(used)) = rule.get_mut("use") else {
                continue;
            };
            let moved = renamed_in(used, from, to);
            *used = moved;
        }
    }
    if let Some(Value::String(classifier)) = value.get_mut("classifier") {
        let moved = renamed_in(classifier, from, to);
        *classifier = moved;
    }
    *group = serde_json::from_value(value).context("keep the group's renamed models")?;
    Ok(())
}

// move_keys gives the names and levels kept of a provider's models the id the
// provider has now.
fn move_keys<V: Clone>(kept: &mut BTreeMap<String, V>, from: &str, to: &str) -> bool {
    let prefix = format!("{from}/");
    let moved = kept
        .iter()
        .filter_map(|(key, value)| {
            let rest = key.strip_prefix(&prefix)?;
            Some((format!("{to}/{rest}"), value.clone()))
        })
        .collect::<Vec<_>>();
    if moved.is_empty() {
        return false;
    }
    kept.retain(|key, _| !key.starts_with(&prefix));
    kept.extend(moved);
    true
}

// rename_provider gives a provider another id and moves the agents on one of
// its models to the same model by the new id, each spelled the way the agent
// spells it. It answers the agents it moved.
pub fn rename_provider(from: &str, to: &str) -> Result<Vec<String>> {
    let from = from.trim().to_lowercase();
    let to = to.trim().to_lowercase();
    // what the agents are on is read before: the models they are shown name
    // the provider by the id it has
    let mut moves = Vec::new();
    if from != to {
        let models = agent::magpie_models()?;
        for agent in agent::all().into_iter().filter(|agent| agent.is_detected()) {
            for (key, value) in agent.values()? {
                let Some(moved) = moved_value(&value, &from, &to, &models) else {
                    continue;
                };
                if moved != value {
                    moves.push((agent.clone(), key, moved));
                }
            }
        }
    }

    rename(&from, &to)?;

    let mut moved = Vec::new();
    for (agent, key, value) in moves {
        let name = agent.spec.name.to_owned();
        agent.apply(key, &value)?;
        if moved.last() != Some(&name) {
            moved.push(name);
        }
    }
    agent::sync_catalog_models()?;
    Ok(moved)
}

// moved_value is what an agent keeps for a model of the provider being
// renamed, spelled by its new id: the reference is replaced where it sits in
// the value, since some agents are shown the id and some the name beside it.
fn moved_value(value: &str, from: &str, to: &str, models: &[agent::MagpieModel]) -> Option<String> {
    let old = format!("{from}/");
    let reference = models.iter().find(|model| {
        model
            .id
            .strip_prefix(&old)
            .is_some_and(|_| value.contains(&model.id))
    })?;
    let model = reference.id[old.len()..].to_owned();
    Some(value.replace(&reference.id, &format!("{to}/{model}")))
}
