use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::{agent, config, provider};

const MAGPIE_MARK: &str = "# magpie";
const API_KEY_ENV: &str = "MAGPIE_API_KEY";

// EFFORTS are the thinking levels dsh knows; unset dsh starts on high.
const EFFORTS: &[&str] = &["off", "high", "max"];

#[derive(Default)]
struct PatchFile {
    head: Vec<String>,
    entries: Vec<PatchEntry>,
}

struct PatchEntry {
    id: String,
    magpie: bool,
    lines: Vec<String>,
}

impl PatchFile {
    fn parse(contents: &str) -> Result<Self> {
        let normalized = contents.replace("\r\n", "\n");
        let normalized = normalized.trim_end_matches('\n');
        let mut file = Self::default();

        if normalized.is_empty() {
            return Ok(file);
        }

        for line in normalized.split('\n') {
            let trimmed = line.trim();
            if line.starts_with("- ") || line == "-" {
                file.entries.push(PatchEntry {
                    id: String::new(),
                    magpie: false,
                    lines: vec![line.to_owned()],
                });
            } else if file.entries.is_empty() {
                ensure!(
                    trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "[]",
                    "{} is not a list of entries Magpie can edit",
                    "config.yaml"
                );
                if trimmed != "[]" {
                    file.head.push(line.to_owned());
                }
                continue;
            } else {
                ensure!(
                    trimmed.is_empty() || trimmed.starts_with('#') || line.starts_with(' '),
                    "{} is not a list of entries Magpie can edit",
                    "config.yaml"
                );
                let entry = file.entries.last_mut().context("patch entry disappeared")?;
                entry.lines.push(line.to_owned());
            }

            if let Some(entry) = file.entries.last_mut()
                && entry.id.is_empty()
                && let Some((id, magpie)) = patch_entry_id(line)
            {
                entry.id = id;
                entry.magpie = magpie;
            }
        }
        Ok(file)
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }

    fn write(&self, path: &Path) -> Result<()> {
        if self.entries.is_empty() && !path.exists() {
            return Ok(());
        }
        let mut lines = self.head.clone();
        lines.extend(
            self.entries
                .iter()
                .flat_map(|entry| entry.lines.iter().cloned()),
        );
        if self.entries.is_empty() {
            lines.push("[]".to_owned());
        }
        let contents = format!("{}\n", lines.join("\n"));
        config::atomic_write_for_settings(path, contents.as_bytes())
            .with_context(|| format!("write dsh patch list at {}", path.display()))
    }
}

fn patch_entry_id(line: &str) -> Option<(String, bool)> {
    let value = line
        .strip_prefix("- id:")
        .or_else(|| line.strip_prefix("  id:"))?;
    let (id, comment) = value.split_once('#').unwrap_or((value, ""));
    let id = id
        .trim()
        .trim_matches(|character| character == '\'' || character == '"');
    (!id.is_empty()).then(|| (id.to_owned(), comment.trim() == "magpie"))
}

fn read_patch(path: &Path) -> Result<Option<PatchFile>> {
    match fs::read_to_string(path) {
        Ok(contents) => PatchFile::parse(&contents)
            .with_context(|| format!("parse dsh patch list at {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read dsh patch list at {}", path.display()))
        }
    }
}

fn profiles(directory: &Path) -> Result<Vec<PathBuf>> {
    let profile_directory = directory.join("profiles");
    let entries = match fs::read_dir(&profile_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", profile_directory.display()));
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", profile_directory.display()))?;
        let path = entry.path().join("cordis.patch.yml");
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort_by_key(|path| {
        let profile = path
            .parent()
            .and_then(Path::file_name)
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        (profile != "web", profile)
    });
    Ok(paths)
}

fn directory(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

pub fn get(config_path: &Path) -> Result<String> {
    let directory = directory(config_path);
    let profiles = profiles(directory)?;
    let (path, row) = profiles
        .first()
        .map_or((config_path.to_owned(), "agent-loop"), |path| {
            (path.clone(), "agent-default-model")
        });
    let mut model = String::new();

    if !profiles.is_empty() {
        let settings_path = directory.join("settings.yaml");
        model = config::get(
            &settings_path,
            config::ConfigFormat::Yaml,
            "agent-default-model.model",
        )?
        .unwrap_or_default();
    }
    let patch = read_patch(&path)?;
    if model.is_empty()
        && let Some(patch) = patch.as_ref()
        && let Some(index) = patch.find(row)
    {
        model = patch.entries[index]
            .lines
            .iter()
            .find_map(|line| patch_model(line))
            .unwrap_or_default();
    }
    if model.is_empty() {
        return Ok(model);
    }

    let via_magpie = patch
        .as_ref()
        .and_then(|patch| {
            patch
                .find("llm-deepseek")
                .map(|index| patch.entries[index].magpie)
        })
        .unwrap_or(false);
    Ok(if via_magpie {
        format!("magpie/{model}")
    } else {
        model
    })
}

fn patch_model(line: &str) -> Option<String> {
    let value = line.trim_start().strip_prefix("model:")?.trim();
    let value = value
        .find(" #")
        .map_or(value, |index| value[..index].trim_end());
    if value.starts_with('"') {
        return serde_json::from_str(value).ok();
    }
    Some(value.trim_matches('\'').to_owned())
}

// files are the patch lists magpie writes: every profile's, else config.yaml.
fn files(directory: &Path, config_path: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = profiles(directory)?;
    if paths.is_empty() {
        paths.push(config_path.to_owned());
    }
    Ok(paths)
}

// effort_in is the thinking effort the sessions magpie starts begin on, read
// from magpie's own DeepSeek entry; another tool's entry is left alone.
fn effort_in(patch: &PatchFile) -> Option<String> {
    let entry = &patch.entries[patch.find("llm-deepseek")?];
    if !entry.magpie {
        return None;
    }
    entry.lines.iter().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key == "reasoningEffort").then(|| {
            value
                .trim()
                .trim_matches(|c| c == '\'' || c == '"')
                .to_owned()
        })
    })
}

// get_effort is what the first patch list magpie writes keeps, if any.
pub fn get_effort(config_path: &Path) -> Result<String> {
    let directory = directory(config_path);
    let path = profiles(directory)?
        .into_iter()
        .next()
        .unwrap_or_else(|| config_path.to_owned());
    let patch = read_patch(&path)?;
    Ok(patch.as_ref().and_then(effort_in).unwrap_or_default())
}

// set_effort writes the thinking effort into every DeepSeek entry magpie
// wrote; dsh's own row is used whole when there is none, so the effort goes
// with a model through magpie.
pub fn set_effort(config_path: &Path, value: &str) -> Result<()> {
    ensure!(
        value.is_empty() || EFFORTS.contains(&value),
        "DeepSeek Harness takes an effort of {}, not {value:?}",
        EFFORTS.join(", ")
    );

    let directory = directory(config_path);
    let mut done = false;
    for path in files(directory, config_path)? {
        let Some(mut patch) = read_patch(&path)? else {
            continue;
        };
        let Some(index) = patch.find("llm-deepseek") else {
            continue;
        };
        if !patch.entries[index].magpie {
            continue;
        }
        let modern = patch.entries[index]
            .lines
            .iter()
            .any(|line| line.trim_start().starts_with("apiKeyEnv:"));
        patch.entries[index].lines = provider_lines(modern, value)?;
        patch.write(&path)?;
        done = true;
    }
    ensure!(
        done || value.is_empty(),
        "pick a model through magpie for DeepSeek Harness first; the effort is kept with magpie's DeepSeek entry"
    );
    Ok(())
}

pub fn set(config_path: &Path, value: &str) -> Result<()> {
    if let Some(model) = value.strip_prefix("magpie/") {
        ensure!(agent::is_gateway_model(model)?, "unknown model {value:?}");
    }

    let directory = directory(config_path);
    let profiles = profiles(directory)?;
    let via_magpie = value.starts_with("magpie/");
    if profiles.is_empty() {
        return set_file(config_path, value, false);
    }

    for path in &profiles {
        set_file(path, value, true)?;
    }
    if config_path.exists() {
        set_file(config_path, "", false)?;
    }

    let env_path = directory.join(".env");
    if via_magpie {
        config::set_env_many(&env_path, &[(API_KEY_ENV, crate::gateway::TOKEN)])?;
    } else if config::get_env(&env_path, API_KEY_ENV)?.is_some() {
        config::delete_env_many(&env_path, &[API_KEY_ENV])?;
    }

    let settings_path = directory.join("settings.yaml");
    if !value.is_empty()
        && config::exists(
            &settings_path,
            config::ConfigFormat::Yaml,
            "agent-default-model",
        )?
    {
        config::delete(
            &settings_path,
            config::ConfigFormat::Yaml,
            "agent-default-model",
        )?;
    }
    Ok(())
}

fn set_file(path: &Path, value: &str, modern: bool) -> Result<()> {
    let mut patch = read_patch(path)?.unwrap_or_default();
    // a model through magpie keeps the effort its entry already holds
    let effort = effort_in(&patch).unwrap_or_default();
    let model = value.strip_prefix("magpie/");
    match (value.is_empty(), modern, model) {
        (true, _, _) => {
            drop_entry(&mut patch, path, "agent-default-model")?;
            drop_entry(&mut patch, path, "api-gateway")?;
            drop_entry(&mut patch, path, "agent-loop")?;
            drop_entry(&mut patch, path, "llm-deepseek")?;
        }
        (false, true, Some(model)) => {
            put_entry(
                &mut patch,
                path,
                "llm-deepseek",
                provider_lines(true, &effort)?,
            )?;
            put_entry(
                &mut patch,
                path,
                "agent-default-model",
                default_lines(model)?,
            )?;
        }
        (false, true, None) => {
            drop_entry(&mut patch, path, "llm-deepseek")?;
            put_entry(
                &mut patch,
                path,
                "agent-default-model",
                default_lines(value)?,
            )?;
        }
        (false, false, Some(model)) => {
            put_entry(
                &mut patch,
                path,
                "llm-deepseek",
                provider_lines(false, &effort)?,
            )?;
            put_entry(&mut patch, path, "agent-loop", loop_lines(model)?)?;
            put_entry(&mut patch, path, "api-gateway", route_lines(model)?)?;
        }
        (false, false, None) => {
            drop_entry(&mut patch, path, "api-gateway")?;
            drop_entry(&mut patch, path, "llm-deepseek")?;
            put_entry(&mut patch, path, "agent-loop", loop_lines(value)?)?;
        }
    }
    patch.write(path)
}

fn put_entry(patch: &mut PatchFile, path: &Path, id: &str, lines: Vec<String>) -> Result<()> {
    if let Some(index) = patch.find(id) {
        if !patch.entries[index].magpie {
            let mut stash = agent::read_agent_stash()?;
            stash.insert(stash_key(path, id), patch.entries[index].lines.join("\n"));
            agent::write_agent_stash(&stash)?;
        }
        patch.entries[index] = PatchEntry {
            id: id.to_owned(),
            magpie: true,
            lines,
        };
    } else {
        patch.entries.push(PatchEntry {
            id: id.to_owned(),
            magpie: true,
            lines,
        });
    }
    Ok(())
}

fn drop_entry(patch: &mut PatchFile, path: &Path, id: &str) -> Result<()> {
    let Some(index) = patch.find(id) else {
        return Ok(());
    };
    if !patch.entries[index].magpie {
        return Ok(());
    }

    let mut stash = agent::read_agent_stash()?;
    let old = stash.remove(&stash_key(path, id));
    if old.is_some() {
        agent::write_agent_stash(&stash)?;
    }
    if let Some(lines) = old {
        patch.entries[index] = PatchEntry {
            id: id.to_owned(),
            magpie: false,
            lines: lines.split('\n').map(str::to_owned).collect(),
        };
    } else {
        patch.entries.remove(index);
    }
    Ok(())
}

fn stash_key(path: &Path, id: &str) -> String {
    format!("dsh:{}:{id}", path.display())
}

fn quote(value: &str) -> Result<String> {
    serde_json::to_string(value).context("quote YAML string")
}

// provider_lines is the DeepSeek entry pointing dsh at the gateway, with the
// catalog as the models its /model offers; effort is the thinking level
// sessions start on, dsh's own default when empty.
fn provider_lines(modern: bool, effort: &str) -> Result<Vec<String>> {
    let key = if modern {
        format!("    apiKeyEnv: {API_KEY_ENV}")
    } else {
        format!("    apiKey: {}", quote(crate::gateway::TOKEN)?)
    };
    let effort = if effort.is_empty() { "high" } else { effort };
    let mut lines = vec![
        format!("- id: llm-deepseek {MAGPIE_MARK}"),
        "  config:".to_owned(),
        key,
        format!("    baseURL: {}", quote(&crate::gateway::v1_url())?),
        "    thinking: enabled".to_owned(),
        format!("    reasoningEffort: {effort}"),
        "    models:".to_owned(),
    ];
    let models = catalog_models()?;
    if models.is_empty() {
        lines[6] = "    models: []".to_owned();
    }
    for (id, name) in models {
        lines.push(format!("      - id: {}", quote(&id)?));
        lines.push(format!("        name: {}", quote(&name)?));
    }
    Ok(lines)
}

fn catalog_models() -> Result<Vec<(String, String)>> {
    let (groups, entries) = provider::desktop_group_data()?;
    let mut models = entries
        .iter()
        .map(|entry| {
            let name = if entry.model.name.is_empty() {
                entry.model.id.as_str()
            } else {
                entry.model.name.as_str()
            };
            (
                entry.id.clone(),
                format!("{name} · {}", entry.provider_name),
            )
        })
        .collect::<Vec<_>>();
    models.extend(
        groups
            .into_iter()
            .filter(|group| !group.hidden)
            .filter(|group| {
                group.members.iter().any(|member| {
                    entries.iter().any(|entry| {
                        entry.id == *member || entry.model.id == *member || {
                            member.split_once('/').is_some_and(|(provider, model)| {
                                !model.is_empty()
                                    && (entry.provider_id == provider
                                        || entry.provider_name.eq_ignore_ascii_case(provider))
                            })
                        }
                    })
                })
            })
            .map(|group| {
                (
                    format!("group/{}", group.id),
                    format!("{} · routing group", group.name),
                )
            }),
    );
    Ok(models)
}

fn default_lines(model: &str) -> Result<Vec<String>> {
    Ok(vec![
        format!("- id: agent-default-model {MAGPIE_MARK}"),
        "  config:".to_owned(),
        "    provider: deepseek-official".to_owned(),
        format!("    model: {}", quote(model)?),
    ])
}

fn loop_lines(model: &str) -> Result<Vec<String>> {
    Ok(vec![
        format!("- id: agent-loop {MAGPIE_MARK}"),
        "  config:".to_owned(),
        "    agents:".to_owned(),
        "      - id: main".to_owned(),
        "        provider: deepseek-official".to_owned(),
        format!("        model: {}", quote(model)?),
        "        cwd: !!js process.cwd()".to_owned(),
    ])
}

fn route_lines(model: &str) -> Result<Vec<String>> {
    Ok(vec![
        format!("- id: api-gateway {MAGPIE_MARK}"),
        "  config:".to_owned(),
        "    provider: deepseek-official".to_owned(),
        format!("    model: {}", quote(model)?),
    ])
}

pub fn sync(config_path: &Path) -> Result<()> {
    let directory = directory(config_path);
    for path in files(directory, config_path)? {
        let Ok(Some(mut patch)) = read_patch(&path) else {
            continue;
        };
        let Some(index) = patch.find("llm-deepseek") else {
            continue;
        };
        if !patch.entries[index].magpie {
            continue;
        }
        let modern = patch.entries[index]
            .lines
            .iter()
            .any(|line| line.trim_start().starts_with("apiKeyEnv:"));
        // rewriting the entry keeps the effort it was given
        let effort = effort_in(&patch).unwrap_or_default();
        let lines = provider_lines(modern, &effort)?;
        if patch.entries[index].lines == lines {
            continue;
        }
        patch.entries[index].lines = lines;
        patch.write(&path)?;
    }
    Ok(())
}
