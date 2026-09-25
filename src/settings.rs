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

pub fn load() -> Settings {
    let path = path();
    let Ok(contents) = fs::read_to_string(path) else {
        return Settings::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn write_json(path: &std::path::Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("settings path has no parent directory")?;
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
