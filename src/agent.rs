use std::{collections::HashMap, env, fs, path::PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    config::{self, ConfigFormat},
    settings,
};

#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub path: &'static str,
    pub provider_path: Option<&'static str>,
    pub catalog_prefix: &'static str,
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
        catalog_prefix: "",
        choices: &[],
    }
}

const fn prefixed_field(
    key: &'static str,
    label: &'static str,
    path: &'static str,
    catalog_prefix: &'static str,
) -> FieldSpec {
    FieldSpec {
        key,
        label,
        path,
        provider_path: None,
        catalog_prefix,
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
        catalog_prefix: "",
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
        catalog_prefix: "",
        choices: &[],
    }
}

const CLAUDE_MODEL: FieldSpec = field("model", "model", "model");
const CLAUDE_GATEWAY_ENV: &[&str] = &[
    "env.ANTHROPIC_BASE_URL",
    "env.ANTHROPIC_AUTH_TOKEN",
    "env.ANTHROPIC_API_KEY",
    "env.ANTHROPIC_MODEL",
    "env.ANTHROPIC_DEFAULT_OPUS_MODEL",
    "env.ANTHROPIC_DEFAULT_SONNET_MODEL",
    "env.ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "env.ANTHROPIC_DEFAULT_FABLE_MODEL",
    "env.ANTHROPIC_SMALL_FAST_MODEL",
    "env.CLAUDE_CODE_SUBAGENT_MODEL",
];
const CODEX_MODEL: FieldSpec = field("model", "model", "model");
const CODEX_EFFORT: FieldSpec = choices_field(
    "effort",
    "effort",
    "model_reasoning_effort",
    &["minimal", "low", "medium", "high", "xhigh", "max"],
);
const GEMINI_MODEL: FieldSpec = field("model", "model", "model.name");
const GEMINI_AUTH: FieldSpec = field("auth", "auth", "security.auth.selectedType");
const OPENCODE_MODEL: FieldSpec = prefixed_field("model", "model", "model", "magpie/");
const OPENCODE_SMALL: FieldSpec = prefixed_field("small", "small", "small_model", "magpie/");
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
        agent.spec.id == query.as_str() || agent.spec.aliases.contains(&query.as_str())
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

        if self.spec.id == "opencode" && !field.catalog_prefix.is_empty() {
            return self.set_opencode_model(field, value);
        }
        if self.spec.id == "commandcode" && field.key == "model" {
            return self.set_commandcode_model(value);
        }
        if self.spec.id == "gemini" && field.key == "model" {
            return self.set_gemini_model(value);
        }
        if self.spec.id == "gemini" && field.key == "auth" && gemini_gateway_configured(self)? {
            restore_gemini_config(self)?;
        }
        if self.spec.id == "claude" && field.key == "model" {
            return self.set_claude_model(value);
        }
        if self.spec.id == "codex" && field.key == "model" {
            return self.set_codex_model(value);
        }

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

    fn set_opencode_model(&self, field: &FieldSpec, value: &str) -> Result<()> {
        if let Some(model) = value.strip_prefix(field.catalog_prefix) {
            let provider = opencode_provider()?;
            let models = provider
                .get("models")
                .and_then(Value::as_object)
                .context("OpenCode model catalog is not an object")?;
            ensure!(
                models.contains_key(model),
                "{model:?} is not a model currently served by magpie"
            );
            config::set_jsonc_values(
                &self.path,
                &[
                    ("provider.magpie", provider),
                    (field.path, Value::String(value.to_owned())),
                ],
            )?;
        } else if value.is_empty() {
            config::delete(&self.path, self.spec.format, field.path)?;
        } else {
            config::set(&self.path, self.spec.format, field.path, value)?;
        }

        if !opencode_has_magpie_model(self)? {
            config::delete(&self.path, self.spec.format, "provider.magpie")?;
        }
        Ok(())
    }

    fn set_codex_model(&self, value: &str) -> Result<()> {
        if value.is_empty() {
            if let Some(provider_id) = codex_gateway_provider(self)? {
                restore_codex_config(self, Some(&provider_id))?;
            } else if is_legacy_codex_gateway(self)? {
                restore_codex_config(self, None)?;
            }
            return config::delete(&self.path, self.spec.format, "model");
        }

        if !is_gateway_model(value)? {
            if let Some(provider_id) = codex_gateway_provider(self)? {
                restore_codex_config(self, Some(&provider_id))?;
            } else if is_legacy_codex_gateway(self)? {
                restore_codex_config(self, None)?;
            }
            return config::set(&self.path, self.spec.format, "model", value);
        }

        let gateway_provider = codex_gateway_provider(self)?;
        let legacy_gateway = is_legacy_codex_gateway(self)?;
        let provider_id = gateway_provider
            .as_ref()
            .map_or_else(|| available_codex_provider_id(self), |id| Ok(id.clone()))?;
        let catalog_path = codex_catalog_path(self);
        let model_catalog = crate::codexcat::render()?;
        settings::write_json(&catalog_path, &model_catalog)?;
        if gateway_provider.is_none() && !legacy_gateway {
            stash_codex_config(self)?;
        }

        let provider = format!("model_providers.{provider_id}");
        let catalog_path = catalog_path.to_string_lossy().into_owned();
        let base_url = crate::gateway::v1_url();
        let assignments = [
            ("model_provider".to_owned(), provider_id),
            ("model_catalog_json".to_owned(), catalog_path),
            ("model".to_owned(), value.to_owned()),
            (format!("{provider}.name"), "magpie".to_owned()),
            (format!("{provider}.base_url"), base_url),
            (format!("{provider}.wire_api"), "responses".to_owned()),
            (
                format!("{provider}.experimental_bearer_token"),
                crate::gateway::TOKEN.to_owned(),
            ),
        ];
        let assignments = assignments
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        config::set_many(&self.path, self.spec.format, &assignments)?;
        config::delete(&self.path, self.spec.format, "openai_base_url")?;
        settle_codex_effort(self, value)
    }

    fn set_claude_model(&self, value: &str) -> Result<()> {
        if value.is_empty() {
            if claude_gateway_configured(self)? {
                restore_claude_config(self)?;
            }
            return config::delete(&self.path, self.spec.format, "model");
        }

        if !is_gateway_model(value)? {
            if claude_gateway_configured(self)? {
                restore_claude_config(self)?;
            }
            return config::set(&self.path, self.spec.format, "model", value);
        }

        if !claude_gateway_configured(self)? {
            stash_claude_config(self)?;
        }
        stash_claude_api_key_if_unstashed(self)?;
        config::delete(&self.path, self.spec.format, "env.ANTHROPIC_API_KEY")?;
        let model = Value::String(value.to_owned());
        config::set_jsonc_values(
            &self.path,
            &[
                ("model", model.clone()),
                (
                    "env.ANTHROPIC_BASE_URL",
                    Value::String(crate::gateway::url()),
                ),
                (
                    "env.ANTHROPIC_AUTH_TOKEN",
                    Value::String(crate::gateway::TOKEN.to_owned()),
                ),
                ("env.ANTHROPIC_MODEL", model.clone()),
                ("env.ANTHROPIC_DEFAULT_OPUS_MODEL", model.clone()),
                ("env.ANTHROPIC_DEFAULT_SONNET_MODEL", model.clone()),
                ("env.ANTHROPIC_DEFAULT_HAIKU_MODEL", model.clone()),
                ("env.ANTHROPIC_DEFAULT_FABLE_MODEL", model.clone()),
                ("env.ANTHROPIC_SMALL_FAST_MODEL", model.clone()),
                ("env.CLAUDE_CODE_SUBAGENT_MODEL", model),
            ],
        )
    }

    fn set_gemini_model(&self, value: &str) -> Result<()> {
        let env_path = gemini_env_path(self);
        let routed = gemini_gateway_configured(self)?;

        if value.is_empty() {
            if routed {
                forget_gemini_config()?;
                config::delete_env_many(&env_path, &["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"])?;
                config::delete_many(
                    &self.path,
                    self.spec.format,
                    &[GEMINI_AUTH.path, GEMINI_MODEL.path],
                )?;
                return Ok(());
            }
            return config::delete(&self.path, self.spec.format, GEMINI_MODEL.path);
        }

        if !is_gateway_model(value)? {
            if routed {
                restore_gemini_config(self)?;
            }
            return config::set(&self.path, self.spec.format, GEMINI_MODEL.path, value);
        }

        if !routed {
            stash_gemini_config(self)?;
        }
        let base_url = crate::gateway::url();
        config::set_env_many(
            &env_path,
            &[
                ("GOOGLE_GEMINI_BASE_URL", &base_url),
                ("GEMINI_API_KEY", crate::gateway::TOKEN),
            ],
        )?;
        config::set_jsonc_values(
            &self.path,
            &[
                (GEMINI_AUTH.path, Value::String("gemini-api-key".to_owned())),
                (GEMINI_MODEL.path, Value::String(value.to_owned())),
            ],
        )
    }

    fn set_commandcode_model(&self, value: &str) -> Result<()> {
        let providers_path = self.path.with_file_name("providers.json");
        let routed = config::get(&self.path, self.spec.format, "modelProvider")?.as_deref()
            == Some("magpie");

        if value.is_empty() {
            if routed {
                config::delete_many(&self.path, self.spec.format, &["model", "modelProvider"])?;
            } else {
                config::delete(&self.path, self.spec.format, "model")?;
            }
            return config::delete(&providers_path, ConfigFormat::Jsonc, "provider.magpie");
        }

        if let Some(model) = value.strip_prefix("magpie/")
            && is_gateway_model(model)?
        {
            let provider = commandcode_provider()?;
            config::set_jsonc_value(&providers_path, "provider.magpie", &provider)?;
            return config::set_jsonc_values(
                &self.path,
                &[
                    ("model", Value::String(value.to_owned())),
                    ("modelProvider", Value::String("magpie".to_owned())),
                ],
            );
        }

        if routed {
            config::delete(&self.path, self.spec.format, "modelProvider")?;
        }
        config::set(&self.path, self.spec.format, "model", value)?;
        config::delete(&providers_path, ConfigFormat::Jsonc, "provider.magpie")
    }
}

pub fn sync_catalog_models() -> Result<()> {
    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "codex")
        && codex_gateway_provider(&agent)?.is_some()
        && config::get(&agent.path, agent.spec.format, "model_catalog_json")?.as_deref()
            == Some(codex_catalog_path(&agent).to_string_lossy().as_ref())
    {
        let model_catalog = crate::codexcat::render()?;
        settings::write_json(&codex_catalog_path(&agent), &model_catalog)?;
    }

    if let Some(agent) = all()
        .into_iter()
        .find(|agent| agent.spec.id == "commandcode")
        && commandcode_has_magpie_model(&agent)?
    {
        config::set_jsonc_value(
            &agent.path.with_file_name("providers.json"),
            "provider.magpie",
            &commandcode_provider()?,
        )?;
    }

    let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "opencode") else {
        return Ok(());
    };
    if opencode_has_magpie_model(&agent)? {
        config::set_jsonc_value(&agent.path, "provider.magpie", &opencode_provider()?)?;
    }
    Ok(())
}

fn is_gateway_model(value: &str) -> Result<bool> {
    let (groups, entries) = crate::provider::desktop_group_data()?;
    Ok(entries.iter().any(|entry| entry.id == value)
        || groups
            .iter()
            .any(|group| !group.hidden && format!("group/{}", group.id) == value))
}

fn claude_gateway_configured(agent: &Agent) -> Result<bool> {
    let base_url = config::get(&agent.path, agent.spec.format, "env.ANTHROPIC_BASE_URL")?;
    let token = config::get(&agent.path, agent.spec.format, "env.ANTHROPIC_AUTH_TOKEN")?;
    Ok(base_url.as_deref().is_some_and(is_gateway_root_url)
        && token.as_deref() == Some(crate::gateway::TOKEN))
}

fn gemini_env_path(agent: &Agent) -> PathBuf {
    agent.path.with_file_name(".env")
}

fn gemini_gateway_configured(agent: &Agent) -> Result<bool> {
    let env_path = gemini_env_path(agent);
    let base_url = config::get_env(&env_path, "GOOGLE_GEMINI_BASE_URL")?;
    let token = config::get_env(&env_path, "GEMINI_API_KEY")?;
    Ok(base_url.as_deref().is_some_and(is_gateway_root_url)
        && token.as_deref() == Some(crate::gateway::TOKEN))
}

fn stash_gemini_config(agent: &Agent) -> Result<()> {
    let env_path = gemini_env_path(agent);
    let mut stash = read_agent_stash()?;
    for (key, value) in [
        (
            "gemini.base_url",
            config::get_env(&env_path, "GOOGLE_GEMINI_BASE_URL")?,
        ),
        (
            "gemini.api_key",
            config::get_env(&env_path, "GEMINI_API_KEY")?,
        ),
        (
            "gemini.auth",
            config::get(&agent.path, agent.spec.format, GEMINI_AUTH.path)?,
        ),
        (
            "gemini.model",
            config::get(&agent.path, agent.spec.format, GEMINI_MODEL.path)?,
        ),
    ] {
        match value.filter(|value| !value.is_empty()) {
            Some(value) => {
                stash.insert(key.to_owned(), value);
            }
            None => {
                stash.remove(key);
            }
        }
    }
    write_agent_stash(&stash)
}

fn restore_gemini_config(agent: &Agent) -> Result<()> {
    let mut stash = read_agent_stash()?;
    let restored_env = [
        ("gemini.base_url", "GOOGLE_GEMINI_BASE_URL"),
        ("gemini.api_key", "GEMINI_API_KEY"),
    ]
    .into_iter()
    .filter_map(|(stash_key, env_key)| {
        stash
            .remove(stash_key)
            .filter(|value| !value.is_empty())
            .map(|value| (env_key, value))
    })
    .collect::<Vec<_>>();
    let restored_json = [
        ("gemini.auth", GEMINI_AUTH.path),
        ("gemini.model", GEMINI_MODEL.path),
    ]
    .into_iter()
    .filter_map(|(stash_key, key_path)| {
        stash
            .remove(stash_key)
            .filter(|value| !value.is_empty())
            .map(|value| (key_path, Value::String(value)))
    })
    .collect::<Vec<_>>();

    let env_path = gemini_env_path(agent);
    config::delete_env_many(&env_path, &["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"])?;
    let env_assignments = restored_env
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect::<Vec<_>>();
    if !env_assignments.is_empty() {
        config::set_env_many(&env_path, &env_assignments)?;
    }

    config::delete_many(
        &agent.path,
        agent.spec.format,
        &[GEMINI_AUTH.path, GEMINI_MODEL.path],
    )?;
    if !restored_json.is_empty() {
        config::set_jsonc_values(&agent.path, &restored_json)?;
    }
    write_agent_stash(&stash)
}

fn forget_gemini_config() -> Result<()> {
    let mut stash = read_agent_stash()?;
    for key in [
        "gemini.base_url",
        "gemini.api_key",
        "gemini.auth",
        "gemini.model",
    ] {
        stash.remove(key);
    }
    write_agent_stash(&stash)
}

fn is_gateway_root_url(value: &str) -> bool {
    if value.trim_end_matches('/') == crate::gateway::url() {
        return true;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && url.path() == "/"
        && url.query().is_none()
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "0.0.0.0" | "::1")
        )
}

fn stash_claude_config(agent: &Agent) -> Result<()> {
    let mut stash = read_agent_stash()?;
    for path in CLAUDE_GATEWAY_ENV {
        let key = claude_stash_key(path);
        match config::get(&agent.path, agent.spec.format, path)? {
            Some(value) if !value.is_empty() => {
                stash.insert(key, value);
            }
            _ => {
                stash.remove(&key);
            }
        }
    }
    write_agent_stash(&stash)
}

fn stash_claude_api_key_if_unstashed(agent: &Agent) -> Result<()> {
    let key = claude_stash_key("env.ANTHROPIC_API_KEY");
    let Some(value) = config::get(&agent.path, agent.spec.format, "env.ANTHROPIC_API_KEY")?
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let mut stash = read_agent_stash()?;
    if stash.contains_key(&key) {
        return Ok(());
    }
    stash.insert(key, value);
    write_agent_stash(&stash)
}

fn restore_claude_config(agent: &Agent) -> Result<()> {
    let mut stash = read_agent_stash()?;
    let restored = CLAUDE_GATEWAY_ENV
        .iter()
        .filter_map(|path| {
            let key = claude_stash_key(path);
            stash
                .remove(&key)
                .map(|value| (*path, Value::String(value)))
                .or_else(|| {
                    let legacy_key = match *path {
                        "env.ANTHROPIC_BASE_URL" => Some("claude.base_url"),
                        "env.ANTHROPIC_AUTH_TOKEN" => Some("claude.auth_token"),
                        _ => None,
                    }?;
                    stash
                        .remove(legacy_key)
                        .map(|value| (*path, Value::String(value)))
                })
        })
        .collect::<Vec<_>>();
    config::delete_many(&agent.path, agent.spec.format, CLAUDE_GATEWAY_ENV)?;
    if !restored.is_empty() {
        config::set_jsonc_values(&agent.path, &restored)?;
    }
    stash.remove("claude.model");
    write_agent_stash(&stash)
}

fn claude_stash_key(path: &str) -> String {
    match path {
        "env.ANTHROPIC_BASE_URL" => "claude.base_url".to_owned(),
        "env.ANTHROPIC_AUTH_TOKEN" => "claude.auth_token".to_owned(),
        _ => format!("claude.env.{}", path.trim_start_matches("env.")),
    }
}

fn settle_codex_effort(agent: &Agent, model: &str) -> Result<()> {
    let (groups, entries) = crate::provider::desktop_group_data()?;
    let efforts = if let Some(entry) = entries.iter().find(|entry| entry.id == model) {
        entry.model.efforts.clone()
    } else if let Some(group) = groups
        .iter()
        .find(|group| format!("group/{}", group.id) == model)
    {
        group
            .members
            .iter()
            .filter_map(|member| entries.iter().find(|entry| entry.id == *member))
            .flat_map(|entry| entry.model.efforts.iter().cloned())
            .fold(Vec::new(), |mut efforts, effort| {
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
                efforts
            })
    } else {
        Vec::new()
    };
    if efforts.is_empty() {
        return Ok(());
    }

    let current =
        config::get(&agent.path, agent.spec.format, "model_reasoning_effort")?.unwrap_or_default();
    if efforts.contains(&current) {
        return Ok(());
    }
    if let Some(default) = crate::codexcat::default_effort(&efforts) {
        config::set(
            &agent.path,
            agent.spec.format,
            "model_reasoning_effort",
            &default,
        )?;
    }
    Ok(())
}

fn codex_gateway_provider(agent: &Agent) -> Result<Option<String>> {
    let Some(provider_id) = config::get(&agent.path, agent.spec.format, "model_provider")? else {
        return Ok(None);
    };
    if !matches!(provider_id.as_str(), "magpie" | "magpie_gateway") {
        return Ok(None);
    }

    let prefix = format!("model_providers.{provider_id}");
    let base_url = config::get(
        &agent.path,
        agent.spec.format,
        &format!("{prefix}.base_url"),
    )?;
    let wire_api = config::get(
        &agent.path,
        agent.spec.format,
        &format!("{prefix}.wire_api"),
    )?;
    let token = config::get(
        &agent.path,
        agent.spec.format,
        &format!("{prefix}.experimental_bearer_token"),
    )?;
    Ok((base_url.as_deref().is_some_and(is_gateway_v1_url)
        && wire_api.as_deref() == Some("responses")
        && token.as_deref() == Some(crate::gateway::TOKEN))
    .then_some(provider_id))
}

fn is_gateway_v1_url(value: &str) -> bool {
    if value.trim_end_matches('/') == crate::gateway::v1_url() {
        return true;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && url.path().trim_end_matches('/') == "/v1"
        && url.query().is_none()
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "0.0.0.0" | "::1")
        )
}

fn available_codex_provider_id(agent: &Agent) -> Result<String> {
    for provider_id in ["magpie", "magpie_gateway"] {
        let prefix = format!("model_providers.{provider_id}");
        if !config::exists(&agent.path, agent.spec.format, &prefix)? {
            return Ok(provider_id.to_owned());
        }
    }
    bail!("Codex already has providers named magpie and magpie_gateway")
}

fn stash_codex_config(agent: &Agent) -> Result<()> {
    let mut stash = read_agent_stash()?;
    for (stash_key, config_key) in [
        ("codex.provider", "model_provider"),
        ("codex.catalog", "model_catalog_json"),
        ("codex.effort", "model_reasoning_effort"),
        ("codex.openai_base_url", "openai_base_url"),
    ] {
        match config::get(&agent.path, agent.spec.format, config_key)? {
            Some(value) if !value.is_empty() => {
                stash.insert(stash_key.to_owned(), value);
            }
            _ => {
                stash.remove(stash_key);
            }
        }
    }
    write_agent_stash(&stash)
}

fn restore_codex_config(agent: &Agent, provider_id: Option<&str>) -> Result<()> {
    let mut stash = read_agent_stash()?;
    let mut deletes = vec![
        "model_provider".to_owned(),
        "model_catalog_json".to_owned(),
        "openai_base_url".to_owned(),
    ];
    if let Some(provider_id) = provider_id {
        deletes.push(format!("model_providers.{provider_id}"));
    }
    let mut assignments = Vec::new();
    for (stash_key, config_key) in [
        ("codex.provider", "model_provider"),
        ("codex.catalog", "model_catalog_json"),
        ("codex.effort", "model_reasoning_effort"),
        ("codex.openai_base_url", "openai_base_url"),
    ] {
        if let Some(value) = stash.remove(stash_key) {
            assignments.push((config_key.to_owned(), value));
        }
    }
    let delete_refs = deletes.iter().map(String::as_str).collect::<Vec<_>>();
    config::delete_many(&agent.path, agent.spec.format, &delete_refs)?;
    let assignment_refs = assignments
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    if !assignment_refs.is_empty() {
        config::set_many(&agent.path, agent.spec.format, &assignment_refs)?;
    }

    let catalog_path = codex_catalog_path(agent);
    if let Err(error) = fs::remove_file(&catalog_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).with_context(|| format!("remove {}", catalog_path.display()));
    }
    stash.remove("codex.model");
    write_agent_stash(&stash)
}

fn codex_catalog_path(agent: &Agent) -> PathBuf {
    agent
        .path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("magpie-gateway-models.json")
}

fn is_legacy_codex_gateway(agent: &Agent) -> Result<bool> {
    let Some(base_url) = config::get(&agent.path, agent.spec.format, "openai_base_url")? else {
        return Ok(false);
    };
    let Ok(url) = url::Url::parse(&base_url) else {
        return Ok(false);
    };
    Ok(url.path().trim_end_matches('/') == "/backend-api/codex"
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "0.0.0.0" | "::1")
        ))
}

fn agent_stash_path() -> PathBuf {
    settings::providers_path().with_file_name("stash.json")
}

fn read_agent_stash() -> Result<HashMap<String, String>> {
    let path = agent_stash_path();
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .with_context(|| format!("parse agent settings stash at {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => {
            Err(error).with_context(|| format!("read agent settings stash at {}", path.display()))
        }
    }
}

fn write_agent_stash(stash: &HashMap<String, String>) -> Result<()> {
    settings::write_json(&agent_stash_path(), stash)
}

fn opencode_has_magpie_model(agent: &Agent) -> Result<bool> {
    ["model", "small_model"]
        .into_iter()
        .try_fold(false, |found, path| {
            if found {
                return Ok(true);
            }
            Ok(config::get(&agent.path, agent.spec.format, path)?
                .is_some_and(|value| value.starts_with("magpie/")))
        })
}

fn opencode_provider() -> Result<Value> {
    let entries = crate::provider::available_model_entries()?;
    let entries_by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut models = serde_json::Map::new();
    for entry in &entries {
        let name = if entry.model.name.is_empty() {
            &entry.model.id
        } else {
            &entry.model.name
        };
        models.insert(
            entry.id.clone(),
            opencode_model(
                &format!("{name} · {}", entry.provider_name),
                entry.model.images,
            ),
        );
    }
    for group in crate::provider::groups()?
        .into_iter()
        .filter(|group| !group.hidden)
    {
        let members = group
            .members
            .iter()
            .filter_map(|member| entries_by_id.get(member.as_str()).copied())
            .collect::<Vec<_>>();
        if members.is_empty() {
            continue;
        }
        models.insert(
            format!("group/{}", group.id),
            opencode_model(
                &format!("{} · routing group", group.name),
                members.iter().all(|member| member.model.images),
            ),
        );
    }

    Ok(json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "magpie",
        "options": {
            "baseURL": crate::gateway::v1_url(),
            "apiKey": crate::gateway::TOKEN,
        },
        "models": models,
    }))
}

fn commandcode_has_magpie_model(agent: &Agent) -> Result<bool> {
    Ok(config::get(&agent.path, agent.spec.format, "modelProvider")?.as_deref() == Some("magpie"))
}

fn commandcode_provider() -> Result<Value> {
    let (groups, entries) = crate::provider::desktop_group_data()?;
    let entries_by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut models = serde_json::Map::new();

    for entry in &entries {
        models.insert(
            entry.id.clone(),
            commandcode_model(
                if entry.model.name.is_empty() {
                    &entry.model.id
                } else {
                    &entry.model.name
                },
                &entry.model.efforts,
            ),
        );
    }
    for group in groups.into_iter().filter(|group| !group.hidden) {
        let members = group
            .members
            .iter()
            .filter_map(|member| entries_by_id.get(member.as_str()).copied())
            .collect::<Vec<_>>();
        if members.is_empty() {
            continue;
        }
        models.insert(
            format!("group/{}", group.id),
            commandcode_model(&format!("{} · routing group", group.name), &[]),
        );
    }

    Ok(json!({
        "name": "magpie",
        "api": "openai-completions",
        "baseURL": crate::gateway::v1_url(),
        "apiKey": false,
        "models": models,
    }))
}

fn commandcode_model(name: &str, efforts: &[String]) -> Value {
    let mut model = json!({"name": name});
    let efforts = efforts
        .iter()
        .filter(|effort| ["low", "medium", "high", "xhigh", "max"].contains(&effort.as_str()))
        .collect::<Vec<_>>();
    if !efforts.is_empty()
        && let Some(fields) = model.as_object_mut()
    {
        fields.insert("reasoning".to_owned(), Value::Bool(true));
        fields.insert("reasoningEfforts".to_owned(), json!(efforts));
    }
    model
}

fn opencode_model(name: &str, images: bool) -> Value {
    if images {
        json!({
            "name": name,
            "attachment": true,
            "modalities": {"input": ["text", "image"], "output": ["text"]},
        })
    } else {
        json!({"name": name})
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
