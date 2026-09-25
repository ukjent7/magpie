use std::{env, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{self, ConfigFormat};

#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub path: &'static str,
    pub provider_path: Option<&'static str>,
    pub choices: &'static [&'static str],
}

#[derive(Debug)]
pub struct AgentSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub executable: &'static str,
    pub relative_path: &'static str,
    pub format: ConfigFormat,
    pub fields: &'static [FieldSpec],
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub spec: &'static AgentSpec,
    pub path: PathBuf,
}

const fn field(key: &'static str, label: &'static str, path: &'static str) -> FieldSpec {
    FieldSpec {
        key,
        label,
        path,
        provider_path: None,
        choices: &[],
    }
}

const fn choices_field(
    key: &'static str,
    label: &'static str,
    path: &'static str,
    choices: &'static [&'static str],
) -> FieldSpec {
    FieldSpec {
        key,
        label,
        path,
        provider_path: None,
        choices,
    }
}

const fn provider_model(
    key: &'static str,
    label: &'static str,
    provider_path: &'static str,
    model_path: &'static str,
) -> FieldSpec {
    FieldSpec {
        key,
        label,
        path: model_path,
        provider_path: Some(provider_path),
        choices: &[],
    }
}

const CLAUDE_MODEL: FieldSpec = field("model", "model", "model");
const CODEX_MODEL: FieldSpec = field("model", "model", "model");
const CODEX_EFFORT: FieldSpec = choices_field(
    "effort",
    "effort",
    "model_reasoning_effort",
    &["minimal", "low", "medium", "high", "xhigh", "max"],
);
const GEMINI_MODEL: FieldSpec = field("model", "model", "model.name");
const GEMINI_AUTH: FieldSpec = field("auth", "auth", "security.auth.selectedType");
const OPENCODE_MODEL: FieldSpec = field("model", "model", "model");
const OPENCODE_SMALL: FieldSpec = field("small", "small", "small_model");
const PI_MODEL: FieldSpec = provider_model("model", "model", "defaultProvider", "defaultModel");
const PI_EFFORT: FieldSpec = choices_field(
    "effort",
    "thinking",
    "defaultThinkingLevel",
    &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
);
const GOOSE_MODEL: FieldSpec = provider_model("model", "model", "GOOSE_PROVIDER", "GOOSE_MODEL");
const CURSOR_MODEL: FieldSpec = field("model", "model", "model.modelId");
const COPILOT_MODEL: FieldSpec = field("model", "model", "model");
const CRUSH_LARGE: FieldSpec = provider_model(
    "model",
    "large",
    "models.large.provider",
    "models.large.model",
);
const CRUSH_SMALL: FieldSpec = provider_model(
    "small",
    "small",
    "models.small.provider",
    "models.small.model",
);
const COMMAND_CODE_MODEL: FieldSpec = field("model", "model", "model");
const OMP_MODEL: FieldSpec = field("model", "model", "modelRoles.default");
const DEVIN_MODEL: FieldSpec = field("model", "model", "agent.model");
const HERMES_MODEL: FieldSpec = provider_model("model", "model", "model.provider", "model.default");

pub static ALL_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        name: "Claude Code",
        aliases: &["cc"],
        executable: "claude",
        relative_path: ".claude/settings.json",
        format: ConfigFormat::Jsonc,
        fields: &[CLAUDE_MODEL],
    },
    AgentSpec {
        id: "codex",
        name: "Codex",
        aliases: &[],
        executable: "codex",
        relative_path: ".codex/config.toml",
        format: ConfigFormat::Toml,
        fields: &[CODEX_MODEL, CODEX_EFFORT],
    },
    AgentSpec {
        id: "gemini",
        name: "Gemini CLI",
        aliases: &[],
        executable: "gemini",
        relative_path: ".gemini/settings.json",
        format: ConfigFormat::Jsonc,
        fields: &[GEMINI_MODEL, GEMINI_AUTH],
    },
    AgentSpec {
        id: "opencode",
        name: "OpenCode",
        aliases: &["oc"],
        executable: "opencode",
        relative_path: ".config/opencode/opencode.json",
        format: ConfigFormat::Jsonc,
        fields: &[OPENCODE_MODEL, OPENCODE_SMALL],
    },
    AgentSpec {
        id: "pi",
        name: "Pi",
        aliases: &[],
        executable: "pi",
        relative_path: ".pi/agent/settings.json",
        format: ConfigFormat::Jsonc,
        fields: &[PI_MODEL, PI_EFFORT],
    },
    AgentSpec {
        id: "goose",
        name: "Goose",
        aliases: &[],
        executable: "goose",
        relative_path: ".config/goose/config.yaml",
        format: ConfigFormat::Yaml,
        fields: &[GOOSE_MODEL],
    },
    AgentSpec {
        id: "cursor",
        name: "Cursor CLI",
        aliases: &["cursor-agent"],
        executable: "cursor-agent",
        relative_path: ".cursor/cli-config.json",
        format: ConfigFormat::Jsonc,
        fields: &[CURSOR_MODEL],
    },
    AgentSpec {
        id: "copilot",
        name: "Copilot CLI",
        aliases: &["gh-copilot"],
        executable: "copilot",
        relative_path: ".copilot/settings.json",
        format: ConfigFormat::Jsonc,
        fields: &[COPILOT_MODEL],
    },
    AgentSpec {
        id: "crush",
        name: "Crush",
        aliases: &[],
        executable: "crush",
        relative_path: ".config/crush/crush.json",
        format: ConfigFormat::Jsonc,
        fields: &[CRUSH_LARGE, CRUSH_SMALL],
    },
    AgentSpec {
        id: "dsh",
        name: "DeepSeek Harness",
        aliases: &["deepseek-harness"],
        executable: "dsh",
        relative_path: ".dsh/config.yaml",
        format: ConfigFormat::Yaml,
        fields: &[],
    },
    AgentSpec {
        id: "commandcode",
        name: "Command Code",
        aliases: &["command-code", "cmd"],
        executable: "command-code",
        relative_path: ".commandcode/settings.json",
        format: ConfigFormat::Jsonc,
        fields: &[COMMAND_CODE_MODEL],
    },
    AgentSpec {
        id: "omp",
        name: "omp (oh-my-pi)",
        aliases: &["oh-my-pi"],
        executable: "omp",
        relative_path: ".omp/agent/config.yml",
        format: ConfigFormat::Yaml,
        fields: &[OMP_MODEL],
    },
    AgentSpec {
        id: "devin",
        name: "Devin",
        aliases: &[],
        executable: "devin",
        relative_path: ".config/devin/config.json",
        format: ConfigFormat::Jsonc,
        fields: &[DEVIN_MODEL],
    },
    AgentSpec {
        id: "hermes",
        name: "Hermes Agent",
        aliases: &["hermes-agent"],
        executable: "hermes",
        relative_path: ".hermes/config.yaml",
        format: ConfigFormat::Yaml,
        fields: &[HERMES_MODEL],
    },
];

pub fn all() -> Vec<Agent> {
    ALL_AGENTS
        .iter()
        .map(|spec| Agent {
            spec,
            path: resolve_path(spec),
        })
        .collect()
}

pub fn find(query: &str) -> Result<Agent> {
    let query = query.trim().to_lowercase();
    let all = all();

    if let Some(found) = all.iter().find(|agent| {
        agent.spec.id == query.as_str()
            || agent
                .spec
                .aliases
                .contains(&query.as_str())
    }) {
        return Ok(found.clone());
    }

    let matches = all
        .iter()
        .filter(|agent| {
            agent.spec.id.starts_with(query.as_str())
                || agent.spec.name.to_lowercase().starts_with(query.as_str())
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [agent] => Ok((*agent).clone()),
        [] => bail!(
            "unknown agent {query:?}; supported agents: {}",
            ALL_AGENTS
                .iter()
                .map(|agent| agent.id)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        many => bail!(
            "{query:?} is ambiguous: {}",
            many.iter()
                .map(|agent| agent.spec.id)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

impl Agent {
    pub fn is_detected(&self) -> bool {
        if self.path.is_file() || self.path.parent().is_some_and(|path| path.is_dir()) {
            return true;
        }
        executable_exists(self.spec.executable)
    }

    pub fn values(&self) -> Result<Vec<(&'static str, String)>> {
        self.spec
            .fields
            .iter()
            .map(|field| {
                let model =
                    config::get(&self.path, self.spec.format, field.path)?.unwrap_or_default();
                let value = match field.provider_path {
                    Some(provider_path) => {
                        let provider = config::get(&self.path, self.spec.format, provider_path)?
                            .unwrap_or_default();
                        if provider.is_empty() {
                            model
                        } else if model.is_empty() {
                            String::new()
                        } else {
                            format!("{provider}/{model}")
                        }
                    }
                    None => model,
                };
                Ok((field.key, value))
            })
            .collect()
    }

    pub fn set(&self, field_name: &str, value: &str) -> Result<()> {
        let field = self
            .spec
            .fields
            .iter()
            .find(|field| field.key == field_name || field.label == field_name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no field {field_name:?}; fields: {}",
                    self.spec.name,
                    self.spec
                        .fields
                        .iter()
                        .map(|field| field.key)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

        if value.is_empty() {
            if let Some(provider_path) = field.provider_path {
                config::delete_many(&self.path, self.spec.format, &[field.path, provider_path])?;
            } else {
                config::delete(&self.path, self.spec.format, field.path)?;
            }
        } else if let Some(provider_path) = field.provider_path {
            let (provider, model) = value
                .split_once('/')
                .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
                .with_context(|| format!("expected provider/model, got {value:?}"))?;
            config::set_many(
                &self.path,
                self.spec.format,
                &[(provider_path, provider), (field.path, model)],
            )?;
        } else {
            config::set(&self.path, self.spec.format, field.path, value)?;
        }
        Ok(())
    }
}

fn resolve_path(spec: &AgentSpec) -> PathBuf {
    let home = env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .unwrap_or_else(|| PathBuf::from("."));
    let config = env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));

    match spec.id {
        "claude" => env_path("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|| home.join(".claude"))
            .join("settings.json"),
        "codex" => env_path("CODEX_HOME")
            .unwrap_or_else(|| home.join(".codex"))
            .join("config.toml"),
        "opencode" => {
            let directory = config.join("opencode");
            let jsonc = directory.join("opencode.jsonc");
            if jsonc.exists() {
                jsonc
            } else {
                directory.join("opencode.json")
            }
        }
        "goose" if cfg!(windows) => env_path("APPDATA")
            .unwrap_or_else(|| home.clone())
            .join("Block/goose/config/config.yaml"),
        "crush" if cfg!(windows) => env_path("LOCALAPPDATA")
            .unwrap_or_else(|| home.clone())
            .join("crush/crush.json"),
        "devin" if cfg!(windows) => env_path("APPDATA")
            .unwrap_or_else(|| home.clone())
            .join("devin/config.json"),
        "dsh" => env_path("DSH_HOME")
            .unwrap_or_else(|| home.join(".dsh"))
            .join("config.yaml"),
        "hermes" => env_path("HERMES_HOME")
            .unwrap_or_else(|| home.join(".hermes"))
            .join("config.yaml"),
        "omp" => {
            let directory = home.join(".omp/agent");
            let yml = directory.join("config.yml");
            if yml.exists() {
                yml
            } else {
                directory.join("config.yaml")
            }
        }
        "commandcode" => home.join(".commandcode/settings.json"),
        "goose" => config.join("goose/config.yaml"),
        "crush" => config.join("crush/crush.json"),
        "devin" => config.join("devin/config.json"),
        _ => home.join(spec.relative_path),
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn executable_exists(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    let suffixes = if cfg!(windows) {
        env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![".EXE".to_owned(), ".BAT".to_owned(), ".CMD".to_owned()])
    } else {
        vec![String::new()]
    };

    env::split_paths(&path).any(|directory| {
        suffixes.iter().any(|suffix| {
            let executable = directory.join(format!("{name}{suffix}"));
            executable.is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_and_prefixes_resolve_to_stable_agent_ids() {
        assert_eq!(find("cc").unwrap().spec.id, "claude");
        assert_eq!(find("op").unwrap().spec.id, "opencode");
    }
}
