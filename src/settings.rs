use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub theme: String,
    pub lang: String,
    pub tray: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub proxy: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agent_order: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents_hidden: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents_shown: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            lang: "system".into(),
            tray: "panel".into(),
            proxy: String::new(),
            agent_order: Vec::new(),
            agents_hidden: Vec::new(),
            agents_shown: Vec::new(),
        }
    }
}

pub fn path() -> PathBuf {
    config_dir().join("magpie/settings.json")
}

pub fn profiles_path() -> PathBuf {
    config_dir().join("magpie/profiles.json")
}

pub fn providers_path() -> PathBuf {
    config_dir().join("magpie/providers.json")
}

pub fn cache_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
                .map(PathBuf::from)
                .map(|home| home.join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from(".cache"))
}

pub fn migrate() {
    let config = config_dir();
    copy_tree(&config.join("dial"), &config.join("magpie"));

    let cache = cache_dir();
    copy_tree(&cache.join("dial"), &cache.join("magpie"));
}

pub fn load() -> Settings {
    let path = path();
    let Ok(contents) = fs::read_to_string(path) else {
        return Settings::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn write_json(path: &std::path::Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("settings path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize settings")?;
    bytes.push(b'\n');
    crate::config::atomic_write_for_settings(path, &bytes)
}

fn config_dir() -> PathBuf {
    if let Some(config) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(config);
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
}

fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
    if destination.exists() || !source.is_dir() || fs::create_dir(destination).is_err() {
        return;
    }
    copy_contents(source, destination);
    if let Ok(metadata) = fs::metadata(source) {
        let _ = fs::set_permissions(destination, metadata.permissions());
    }
}

fn copy_contents(source: &std::path::Path, destination: &std::path::Path) {
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    for entry in entries.flatten() {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if fs::create_dir(&destination_path).is_ok() {
                copy_contents(&source_path, &destination_path);
                if let Ok(metadata) = fs::metadata(source_path) {
                    let _ = fs::set_permissions(destination_path, metadata.permissions());
                }
            }
        } else if kind.is_file()
            && fs::copy(&source_path, &destination_path).is_ok()
            && let Ok(metadata) = fs::metadata(source_path)
        {
            let _ = fs::set_permissions(destination_path, metadata.permissions());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_settings_use_system_defaults() {
        let defaults = Settings::default();
        assert_eq!(defaults.theme, "system");
        assert_eq!(defaults.lang, "system");
        assert_eq!(defaults.tray, "panel");
    }
}
