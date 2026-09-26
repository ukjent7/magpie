use std::{collections::BTreeMap, fs};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{agent, library, settings};

// Profile is every agent's fields, and the library's setup, so a whole
// setup — the models, and which agent gets which server and skill —
// switches back in one move.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    // fields maps "agent.field" to its value. It has no key of its own in
    // the file, so a profile saved before the library was kept reads as it
    // always did.
    #[serde(flatten, default)]
    pub fields: BTreeMap<String, String>,
    // library is which agents got which servers and skills, and the
    // instructions; nothing for a profile saved while the library was
    // empty, which leaves the library as it is when the profile is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<library::Setup>,
}

pub type Profiles = BTreeMap<String, Profile>;

// Applied is what applying a profile did.
pub struct Applied {
    // changed counts the agent fields written.
    pub changed: usize,
    // library is what bringing the profile's setup back wrote into the
    // agents; nothing for a profile that carries none.
    pub library: Option<library::SyncResult>,
}

pub fn list(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("usage: magpie profiles");
    }
    let profiles = load()?;
    if profiles.is_empty() {
        println!("no profiles yet · magpie save <name>");
        return Ok(());
    }
    for (name, profile) in profiles {
        println!("  {name}  {}", long_summary(&profile));
    }
    Ok(())
}

pub fn list_entries() -> Result<Vec<(String, String)>> {
    Ok(load()?
        .into_iter()
        .map(|(name, profile)| (name, long_summary(&profile)))
        .collect())
}

// long_summary is the models a profile sets, and what it gives out from
// the library after them.
fn long_summary(profile: &Profile) -> String {
    let mut summary = profile
        .fields
        .iter()
        .filter(|(key, _)| key.ends_with(".model"))
        .map(|(key, value)| format!("{} {}", key.trim_end_matches(".model"), value))
        .collect::<Vec<_>>()
        .join(" · ");
    if let Some(setup) = profile.library.as_ref().filter(|s| !s.empty()) {
        if !summary.is_empty() {
            summary.push_str(" · ");
        }
        summary.push_str("+ ");
        summary.push_str(&setup.summary());
    }
    summary
}

// snapshot_entries is the current value of every detected agent's field
// that has one, as "agent.field"; the backup's agent settings.
pub(crate) fn snapshot_entries() -> Result<Vec<(String, String)>> {
    let mut entries = Vec::new();
    for current in agent::all().into_iter().filter(|a| a.is_detected()) {
        for (field, value) in current.values()? {
            if !value.is_empty() {
                entries.push((format!("{}.{}", current.spec.id, field), value));
            }
        }
    }
    Ok(entries)
}

pub(crate) fn backup_entries() -> Result<Profiles> {
    load()
}

pub(crate) fn restore_entries(incoming: &Profiles) -> Result<usize> {
    if incoming.is_empty() {
        return Ok(0);
    }
    let mut profiles = load()?;
    for (name, profile) in incoming {
        let name = name.trim();
        if name.is_empty() {
            bail!("profile name is empty");
        }
        profiles.insert(name.to_owned(), profile.clone());
    }
    store(&profiles)?;
    Ok(incoming.len())
}

// Snapshot captures every detected agent's fields, and the library's setup
// unless the library is empty.
pub fn snapshot() -> Result<Profile> {
    let mut profile = Profile::default();
    for current in agent::all().into_iter().filter(|a| a.is_detected()) {
        for (key, value) in current.values()? {
            if !value.is_empty() {
                profile
                    .fields
                    .insert(format!("{}.{}", current.spec.id, key), value);
            }
        }
    }
    let setup = library::snapshot()?;
    if !setup.empty() {
        profile.library = Some(setup);
    }
    Ok(profile)
}

pub fn save(args: &[String]) -> Result<()> {
    let [name] = args else {
        bail!("usage: magpie save <name>");
    };
    let profile = snapshot()?;
    put(name, &profile)?;
    let name = name.trim();
    println!("✓ saved profile {name}");
    if let Some(setup) = &profile.library {
        println!("  with the {}", setup.summary());
    }
    Ok(())
}

// save_named captures what is set now under name, replacing what was
// there: what the app and the terminal interface do.
pub fn save_named(name: &str) -> Result<()> {
    put(name, &snapshot()?)
}

fn put(name: &str, profile: &Profile) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("profile name is empty");
    }
    let mut profiles = load()?;
    profiles.insert(name.to_owned(), profile.clone());
    store(&profiles)
}

pub fn apply(args: &[String]) -> Result<()> {
    let [name] = args else {
        bail!("usage: magpie use <name>");
    };
    let applied = apply_named(name)?;
    println!("✓ applied {name} ({} changed)", applied.changed);
    for line in report(&applied) {
        println!("  {line}");
    }
    Ok(())
}

// report is what applying did to the library, a line each, for the
// terminal: none for a profile that carries no setup.
pub fn report(applied: &Applied) -> Vec<String> {
    let Some(result) = &applied.library else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    if !result.changed.is_empty() {
        lines.push(format!("library written into {}", result.changed.join(", ")));
    }
    if !result.missing.is_empty() {
        lines.push(format!(
            "skipped, no longer in the library: {}",
            result.missing.join(", ")
        ));
    }
    lines.extend(
        result
            .problems
            .iter()
            .map(|p| format!("{} {}: {}", p.agent, p.what, p.error)),
    );
    lines
}

pub fn apply_named(name: &str) -> Result<Applied> {
    let profiles = load()?;
    let profile = profiles
        .get(name)
        .with_context(|| format!("no profile named {name:?}"))?;
    apply_profile(profile)
}

// apply_profile writes every field that differs from what is set now, then
// brings the library's setup back and writes it into the agents, their own
// files kept aside first.
fn apply_profile(profile: &Profile) -> Result<Applied> {
    let mut applied = Applied {
        changed: apply_fields(&profile.fields)?,
        library: None,
    };
    if let Some(setup) = &profile.library {
        applied.library = Some(library::restore(setup.clone())?);
    }
    Ok(applied)
}

// apply_fields writes every value that differs from what is set now. The
// providers go first: switching one re-settles the model behind it. The
// models come next, which settle what the other fields — Claude Code's
// tiers — hang on.
fn apply_fields(fields: &BTreeMap<String, String>) -> Result<usize> {
    let mut keys = fields.keys().collect::<Vec<_>>();
    keys.sort_unstable_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    let mut changed = 0;
    for key in keys {
        let Some((agent_id, field)) = key.split_once('.') else {
            continue;
        };
        let value = &fields[key];
        let Ok(current) = agent::find(agent_id) else {
            continue;
        };
        if !current.is_detected()
            || !current
                .spec
                .fields
                .iter()
                .any(|candidate| candidate.key == field)
        {
            continue;
        }
        let set = current
            .values()?
            .into_iter()
            .find(|(key, _)| *key == field)
            .is_some_and(|(_, here)| here == *value);
        if set {
            continue;
        }
        current
            .set(field, value)
            .with_context(|| format!("apply profile field {key:?}"))?;
        changed += 1;
    }
    Ok(changed)
}

// rank is the order a profile's fields go in: providers, then models, then
// what hangs on those.
fn rank(key: &str) -> u8 {
    match key.rsplit_once('.') {
        Some((_, "provider")) => 0,
        Some((_, "model")) => 1,
        _ => 2,
    }
}

pub fn remove(args: &[String]) -> Result<()> {
    let [name] = args else {
        bail!("usage: magpie rm <name>");
    };
    delete_named(name)?;
    println!("✓ removed profile {name}");
    Ok(())
}

pub fn delete_named(name: &str) -> Result<()> {
    let mut profiles = load()?;
    if profiles.remove(name).is_none() {
        bail!("no profile named {name:?}");
    }
    store(&profiles)
}

fn store(profiles: &Profiles) -> Result<()> {
    settings::write_json(&settings::profiles_path(), profiles)
}

fn load() -> Result<Profiles> {
    let path = settings::profiles_path();
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Profiles::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&contents).with_context(|| format!("parse {}", path.display()))
}
