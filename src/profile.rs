use std::{collections::BTreeMap, fs};

use anyhow::{Context, Result, bail};

use crate::{agent, settings};

type Profile = BTreeMap<String, serde_json::Value>;
type Profiles = BTreeMap<String, Profile>;

pub fn list(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("usage: magpie profiles");
    }
    let profiles = load()?;
    if profiles.is_empty() {
        println!("no profiles yet · magpie save <name>");
        return Ok(());
    }
    for (name, values) in profiles {
        let count = values
            .keys()
            .filter(|key| key.as_str() != "library")
            .count();
        println!("  {name}  {count} settings");
    }
    Ok(())
}

pub fn list_entries() -> Result<Vec<(String, String)>> {
    Ok(load()?
        .into_iter()
        .map(|(name, values)| {
            let count = values
                .keys()
                .filter(|key| key.as_str() != "library")
                .count();
            (name, format!("{count} settings"))
        })
        .collect())
}

pub fn snapshot_entries() -> Result<Vec<(String, String)>> {
    let mut snapshot = Vec::new();
    for current in agent::all()
        .into_iter()
        .filter(|current| current.is_detected())
    {
        for (field, value) in current.values()? {
            if !value.is_empty() {
                snapshot.push((format!("{}.{field}", current.spec.id), value));
            }
        }
    }
    Ok(snapshot)
}

pub fn save(args: &[String]) -> Result<()> {
    let [name] = args else {
        bail!("usage: magpie save <name>");
    };
    let name = name.trim();
    if name.is_empty() {
        bail!("profile name is empty");
    }

    save_named(name)?;
    println!("✓ saved profile {name}");
    Ok(())
}

pub fn save_named(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("profile name is empty");
    }

    let mut profiles = load()?;
    let mut snapshot = Profile::new();
    let agents = agent::all();
    let known_fields = agents
        .iter()
        .flat_map(|current| {
            current
                .spec
                .fields
                .iter()
                .map(move |field| format!("{}.{}", current.spec.id, field.key))
        })
        .collect::<std::collections::HashSet<_>>();

    for current in agents.into_iter().filter(|agent| agent.is_detected()) {
        for (key, value) in current.values()? {
            if !value.is_empty() {
                snapshot.insert(
                    format!("{}.{}", current.spec.id, key),
                    serde_json::Value::String(value),
                );
            }
        }
    }
    // Keep profile data the Rust port does not understand, such as library
    // setup and fields from newer Go builds, until those features are ported.
    if let Some(previous) = profiles.get(name) {
        for (key, value) in previous {
            if key == "library" || !known_fields.contains(key) {
                snapshot.insert(key.clone(), value.clone());
            }
        }
    }
    profiles.insert(name.to_owned(), snapshot);
    settings::write_json(&settings::profiles_path(), &profiles)?;
    Ok(())
}

pub fn apply(args: &[String]) -> Result<()> {
    let [name] = args else {
        bail!("usage: magpie use <name>");
    };
    let changed = apply_named(name)?;
    println!("✓ applied {name} ({changed} settings changed)");
    Ok(())
}

pub fn apply_named(name: &str) -> Result<usize> {
    let profiles = load()?;
    let profile = profiles
        .get(name)
        .with_context(|| format!("no profile named {name:?}"))?;
    let mut changed = 0;
    for (key, value) in profile {
        let Some(value) = value.as_str() else {
            continue;
        };
        let Some((agent_id, field)) = key.split_once('.') else {
            continue;
        };
        match agent::find(agent_id) {
            Ok(current) if current.is_detected() => {
                if !current
                    .spec
                    .fields
                    .iter()
                    .any(|candidate| candidate.key == field)
                {
                    continue;
                }
                let current_value = current
                    .values()?
                    .into_iter()
                    .find(|(key, _)| *key == field)
                    .map(|(_, value)| value);
                if current_value.as_deref() != Some(value) {
                    current
                        .set(field, value)
                        .with_context(|| format!("apply profile field {key:?}"))?;
                    changed += 1;
                }
            }
            _ => continue,
        }
    }
    Ok(changed)
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
    settings::write_json(&settings::profiles_path(), &profiles)?;
    Ok(())
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
