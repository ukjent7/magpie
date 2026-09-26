use anyhow::{Context, Result, bail, ensure};

use crate::provider::{self, Group, ModelEntry};

const GROUP_PREFIX: &str = "group/";

pub fn command(args: &[String]) -> Result<()> {
    match args {
        [] => list(),
        [verb] if matches!(verb.as_str(), "help" | "-h" | "--help") => {
            println!("{}", usage());
            Ok(())
        }
        [verb] if matches!(verb.as_str(), "list" | "ls") => list(),
        [verb, rest @ ..] if matches!(verb.as_str(), "add" | "new") => add(rest),
        [verb, rest @ ..] if matches!(verb.as_str(), "set" | "edit") => set(rest),
        [verb, rest @ ..] if verb == "rule" => crate::grouprule::command(rest),
        [verb, reference] if matches!(verb.as_str(), "rm" | "remove" | "delete") => {
            remove(reference)
        }
        [verb, reference] if matches!(verb.as_str(), "restore" | "unhide") => restore(reference),
        [verb, reference] if verb == "show" => show(reference),
        [reference] => show(reference),
        [verb, ..] => bail!("unknown group command {verb:?}\n\n{}", usage()),
    }
}

fn add(args: &[String]) -> Result<()> {
    let Some(name) = args.first().filter(|name| !name.contains('=')) else {
        bail!("usage: magpie group add <name> models=<model>[,<model>…]");
    };
    let mut group = Group::default();
    group.name = name.trim().to_owned();
    let entries = provider::available_model_entries()?;
    apply_pairs(&mut group, &args[1..], &entries, &[], true)?;

    if group.id.trim().is_empty() {
        group.id = new_id(&group.name, &provider::groups()?);
    } else {
        group.id = group.id.trim().to_ascii_lowercase();
        ensure!(
            !provider::groups()?
                .iter()
                .any(|existing| existing.id == group.id),
            "group {:?} already exists; use `magpie group set {}` to edit it",
            group.id,
            group.id
        );
    }
    ensure!(
        !group.members.is_empty(),
        "a group needs at least one model"
    );
    provider::save_group(group.clone())?;
    println!("✓ added {} (group/{})", group.name, group.id);
    show_group(&group, &entries)
}

fn set(args: &[String]) -> Result<()> {
    let [reference, pairs @ ..] = args else {
        bail!("usage: magpie group set <id> name=… models=… routing=… stays=…");
    };
    ensure!(
        !pairs.is_empty(),
        "provide at least one group field to update"
    );
    let mut group = find_group(reference)?;
    ensure!(
        !group.hidden,
        "{} is removed; `magpie group restore {}` brings it back first",
        group.id,
        group.id
    );
    let entries = provider::available_model_entries()?;
    let existing_members = group.members.clone();
    apply_pairs(&mut group, pairs, &entries, &existing_members, false)?;
    ensure!(
        !group.members.is_empty(),
        "a group needs at least one model"
    );
    provider::save_group(group.clone())?;
    println!("✓ saved {}", group.name);
    show_group(&group, &entries)
}

fn remove(reference: &str) -> Result<()> {
    let group = find_group(reference)?;
    ensure!(!group.hidden, "{} is already removed", group.id);
    provider::delete_group(&group.id)?;
    if group.auto {
        println!(
            "✓ hid automatically found group {}; restore with `magpie group restore {}`",
            group.id, group.id
        );
    } else {
        println!("✓ removed group {}", group.id);
    }
    Ok(())
}

fn restore(reference: &str) -> Result<()> {
    let group = find_group(reference)?;
    ensure!(group.hidden, "{} is not removed", group.id);
    provider::restore_group(&group.id)?;
    println!("✓ restored group {}", group.id);
    let entries = provider::available_model_entries()?;
    let restored = find_group(&group.id)?;
    show_group(&restored, &entries)
}

fn list() -> Result<()> {
    let groups = provider::groups()?;
    let entries = provider::available_model_entries()?;
    let visible = groups
        .iter()
        .filter(|group| !group.hidden)
        .collect::<Vec<_>>();
    if visible.is_empty() {
        println!("no routing groups yet · `magpie group add <name> models=<m1>,<m2>`");
    }
    for group in visible {
        let routing = routing_name(&group.routing);
        let affinity = affinity_name(&group.affinity);
        let mut members = Vec::new();
        for member in &group.members {
            if let Some(entry) = entries.iter().find(|entry| entry.id == *member) {
                members.push(format!("{} · {}", entry.provider_name, entry.model.name));
            } else {
                members.push(format!("{member} (not served)"));
            }
        }
        println!(
            "  {}  {GROUP_PREFIX}{}  {}{}  {}{}",
            group.name,
            group.id,
            routing,
            if group.affinity.is_empty() {
                String::new()
            } else {
                format!(" · stays {affinity}")
            },
            members.join(if group.routing == "order" {
                " → "
            } else {
                " · "
            }),
            if group.auto { " · found" } else { "" }
        );
    }
    let hidden = groups
        .iter()
        .filter(|group| group.hidden)
        .collect::<Vec<_>>();
    if !hidden.is_empty() {
        println!(
            "removed: {} · restore with `magpie group restore <id>`",
            hidden
                .iter()
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn show(reference: &str) -> Result<()> {
    let group = find_group(reference)?;
    let entries = provider::available_model_entries()?;
    show_group(&group, &entries)
}

fn show_group(group: &Group, entries: &[ModelEntry]) -> Result<()> {
    println!("{}  {GROUP_PREFIX}{}", group.name, group.id);
    println!("  routing  {}", routing_name(&group.routing));
    println!("  stays    {}", affinity_name(&group.affinity));
    if group.auto {
        println!("  source   automatically discovered; editing makes it yours");
    }
    if group.hidden {
        println!(
            "  status   removed; restore with `magpie group restore {}`",
            group.id
        );
    }
    for (index, member) in group.members.iter().enumerate() {
        let label = entries
            .iter()
            .find(|entry| entry.id == *member)
            .map(|entry| format!("{} · {}", entry.provider_name, entry.model.name))
            .unwrap_or_else(|| "not served now; skipped".to_owned());
        println!("  {:>2}. {member}  {label}", index + 1);
    }
    Ok(())
}

fn find_group(reference: &str) -> Result<Group> {
    let reference = reference
        .trim()
        .strip_prefix("magpie/")
        .unwrap_or(reference.trim());
    let reference = reference.strip_prefix(GROUP_PREFIX).unwrap_or(reference);
    let groups = provider::groups()?;
    groups
        .iter()
        .find(|group| group.id == reference)
        .or_else(|| {
            groups
                .iter()
                .find(|group| group.id.eq_ignore_ascii_case(reference))
        })
        .or_else(|| {
            groups
                .iter()
                .find(|group| !group.hidden && group.name.eq_ignore_ascii_case(reference))
        })
        .cloned()
        .with_context(|| {
            let ids = groups
                .iter()
                .filter(|group| !group.hidden)
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if ids.is_empty() {
                format!("no group {reference:?}; add one with `magpie group add`")
            } else {
                format!("no group {reference:?}; groups: {ids}")
            }
        })
}

fn apply_pairs(
    group: &mut Group,
    pairs: &[String],
    entries: &[ModelEntry],
    keep: &[String],
    allow_id: bool,
) -> Result<()> {
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("expected key=value, got {pair:?}"))?;
        let key = key.trim().to_ascii_lowercase();
        match key.as_str() {
            "id" if allow_id => group.id = value.trim().to_owned(),
            "id" => bail!("a group's id stays as it is; add a new group for another"),
            "name" => group.name = value.trim().to_owned(),
            "models" | "members" | "model" => {
                group.members = resolve_members(value, entries, keep)?;
            }
            "models+" | "members+" | "model+" => {
                for member in resolve_members(value, entries, keep)? {
                    if !group.members.contains(&member) {
                        group.members.push(member);
                    }
                }
            }
            "models-" | "members-" | "model-" => {
                for member in split_list(value) {
                    let member = member.strip_prefix("magpie/").unwrap_or(member);
                    let index = group
                        .members
                        .iter()
                        .position(|saved| saved.eq_ignore_ascii_case(member))
                        .or_else(|| {
                            group.members.iter().position(|saved| {
                                saved
                                    .split_once('/')
                                    .is_some_and(|(_, model)| model.eq_ignore_ascii_case(member))
                            })
                        })
                        .with_context(|| format!("{member} is not in this group's members"))?;
                    group.members.remove(index);
                }
            }
            "routing" | "strategy" => group.routing = parse_routing(value)?,
            "stays" | "stay" | "affinity" => group.affinity = parse_affinity(value)?,
            _ => bail!(
                "unknown group field {key:?}; use name, models, models+, models-, routing or stays"
            ),
        }
    }
    Ok(())
}

fn resolve_members(value: &str, entries: &[ModelEntry], keep: &[String]) -> Result<Vec<String>> {
    let mut members = Vec::new();
    for input in split_list(value) {
        let member = resolve_member(input, entries, keep)?;
        if !members.contains(&member) {
            members.push(member);
        }
    }
    Ok(members)
}

fn resolve_member(value: &str, entries: &[ModelEntry], keep: &[String]) -> Result<String> {
    let value = value.trim().strip_prefix("magpie/").unwrap_or(value.trim());
    ensure!(
        !value.starts_with(GROUP_PREFIX),
        "a group cannot be used as a member of another group"
    );
    if let Some(member) = entries
        .iter()
        .map(|entry| entry.id.as_str())
        .chain(keep.iter().map(String::as_str))
        .find(|id| id.eq_ignore_ascii_case(value))
    {
        return Ok(member.to_owned());
    }
    let matches = entries
        .iter()
        .filter(|entry| entry.model.id.eq_ignore_ascii_case(value))
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [only] => Ok((*only).to_owned()),
        [] => bail!("magpie knows no model {value:?}; `magpie models` lists available models"),
        _ => bail!(
            "{value} is served by multiple providers; specify one: {}",
            matches.join(", ")
        ),
    }
}

fn parse_routing(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "smart" | "default" | "auto" => Ok(String::new()),
        "order" | "ordered" | "in-order" => Ok("order".to_owned()),
        "rotate" | "in-turn" | "round-robin" => Ok("rotate".to_owned()),
        "usage" | "least-used" => Ok("usage".to_owned()),
        _ => bail!("routing must be smart, order, rotate or usage"),
    }
}

fn parse_affinity(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" | "default" => Ok(String::new()),
        "session" => Ok("session".to_owned()),
        "turn" | "within-a-turn" => Ok("turn".to_owned()),
        "off" | "none" | "never" => Ok("off".to_owned()),
        _ => bail!("stays must be auto, session, turn or off"),
    }
}

fn new_id(name: &str, groups: &[Group]) -> String {
    let mut base = slug(name);
    if base.is_empty() {
        base.push_str("group");
    }
    let taken = |candidate: &str| {
        groups
            .iter()
            .any(|group| group.id.eq_ignore_ascii_case(candidate))
    };
    if !taken(&base) {
        return base;
    }
    for number in 2_u32.. {
        let candidate = format!("{base}-{number}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!("a group id is always available")
}

fn slug(value: &str) -> String {
    let mut slug = String::new();
    for character in value.trim().to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

fn split_list(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|item| !item.is_empty())
}

fn routing_name(value: &str) -> &str {
    match value {
        "order" => "order",
        "rotate" => "rotate",
        "usage" => "usage",
        _ => "smart",
    }
}

fn affinity_name(value: &str) -> &str {
    if value.is_empty() { "auto" } else { value }
}

fn usage() -> &'static str {
    "usage:\n  magpie groups\n  magpie group [id|name]\n  magpie group add <name> models=<m1>[,<m2>…] [routing=…] [stays=…]\n  magpie group set <id> name=… models=… models+=… models-=… routing=… stays=…\n  magpie group rm <id>\n  magpie group restore <id>"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(provider: &str, model: &str) -> ModelEntry {
        ModelEntry {
            id: format!("{provider}/{model}"),
            model: crate::catalog::Model {
                id: model.to_owned(),
                name: model.to_owned(),
                ..crate::catalog::Model::default()
            },
            provider_id: provider.to_owned(),
            provider_name: provider.to_owned(),
            icon: String::new(),
            family: String::new(),
        }
    }

    #[test]
    fn model_members_resolve_qualified_and_unique_ids() {
        let entries = [entry("a", "same"), entry("b", "different")];
        assert_eq!(resolve_member("a/same", &entries, &[]).unwrap(), "a/same");
        assert_eq!(
            resolve_member("different", &entries, &[]).unwrap(),
            "b/different"
        );
        assert!(resolve_member("group/other", &entries, &[]).is_err());
    }

    #[test]
    fn ambiguous_bare_model_names_require_a_provider() {
        let entries = [entry("a", "same"), entry("b", "same")];
        assert!(resolve_member("same", &entries, &[]).is_err());
        assert_eq!(resolve_member("b/same", &entries, &[]).unwrap(), "b/same");
    }

    #[test]
    fn routing_and_affinity_aliases_normalize_to_saved_values() {
        assert_eq!(parse_routing("round-robin").unwrap(), "rotate");
        assert_eq!(parse_routing("default").unwrap(), "");
        assert_eq!(parse_affinity("within-a-turn").unwrap(), "turn");
        assert_eq!(parse_affinity("never").unwrap(), "off");
    }
}
