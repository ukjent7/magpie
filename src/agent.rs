use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    config::{self, ConfigFormat},
    settings,
};

mod alma;
mod applied;
mod grok;
mod zcode;

pub use applied::Drift;

// magpie_id is the provider id these agents know the gateway by; a catalog
// model is spelled "magpie/<provider>/<model>" where the agent needs to
// tell magpie's models from its own.
pub(crate) const MAGPIE_ID: &str = "magpie";
pub(crate) const MAGPIE_PREFIX: &str = "magpie/";

// MagpieModel is one model of the catalog as these agents are shown it, for
// the ones that keep their own model files.
#[derive(Clone)]
pub(crate) struct MagpieModel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) efforts: Vec<String>,
    pub(crate) images: bool,
    pub(crate) context: usize,
    pub(crate) output: usize,
}

// magpie_models is the catalog: one entry per model of every provider the
// user added, then one per routing group.
pub(crate) fn magpie_models() -> Result<Vec<MagpieModel>> {
    #[cfg(test)]
    {
        if let Some(models) = testing::catalog() {
            return Ok(models);
        }
    }
    let (groups, entries) = crate::provider::desktop_group_data()?;
    let entries_by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut models = Vec::with_capacity(entries.len() + groups.len());
    for entry in &entries {
        let name = if entry.model.name.is_empty() {
            &entry.model.id
        } else {
            &entry.model.name
        };
        models.push(MagpieModel {
            id: entry.id.clone(),
            name: format!("{name} · {}", entry.provider_name),
            efforts: entry.model.efforts.clone(),
            images: entry.model.images,
            context: entry.model.context,
            output: entry.model.output,
        });
    }
    for group in groups.into_iter().filter(|group| !group.hidden) {
        let members = group
            .members
            .iter()
            .filter_map(|member| entries_by_id.get(member.as_str()).copied())
            .collect::<Vec<_>>();
        let Some((first, rest)) = members.split_first() else {
            continue;
        };
        let efforts = first
            .model
            .efforts
            .iter()
            .filter(|effort| {
                rest.iter()
                    .all(|member| member.model.efforts.contains(effort))
            })
            .cloned()
            .collect::<Vec<_>>();
        models.push(MagpieModel {
            id: format!("group/{}", group.id),
            name: format!("{} · routing group", group.name),
            efforts,
            images: members.iter().all(|member| member.model.images),
            context: members
                .iter()
                .map(|member| member.model.context)
                .filter(|context| *context > 0)
                .min()
                .unwrap_or_default(),
            output: members
                .iter()
                .map(|member| member.model.output)
                .filter(|output| *output > 0)
                .min()
                .unwrap_or_default(),
        });
    }
    Ok(models)
}

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
// the effort Claude Code starts with, as its /effort saves it; settings.json
// keeps low to xhigh, since max lasts a session only
const CLAUDE_EFFORT: FieldSpec = choices_field("effort", "effort", "effortLevel", CLAUDE_EFFORTS);
const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
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
// GOOSE_THINKING_EFFORT, the effort goose asks of a model that thinks, for
// every provider
const GOOSE_EFFORT: FieldSpec = choices_field(
    "effort",
    "effort",
    "GOOSE_THINKING_EFFORT",
    &["off", "low", "medium", "high", "max"],
);
const CURSOR_MODEL: FieldSpec = field("model", "model", "model.modelId");
const COPILOT_MODEL: FieldSpec = field("model", "model", "model");
// effortLevel, which Copilot saves beside the model and clears when its own
// /model changes the model; the levels are the model's, these when magpie's
// copy of Copilot's list doesn't say
const COPILOT_EFFORT: FieldSpec = choices_field(
    "effort",
    "effort",
    "effortLevel",
    &["low", "medium", "high", "xhigh"],
);
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
// models.large.reasoning_effort, kept beside the large model it is for
const CRUSH_EFFORT: FieldSpec = choices_field(
    "effort",
    "effort",
    "models.large.reasoning_effort",
    &["low", "medium", "high"],
);
const COMMAND_CODE_MODEL: FieldSpec = field("model", "model", "model");
// reasoningEffort keeps an effort for each model (Command Code's /effort
// saves it); this field is the current model's, so its path is read whole
const COMMAND_CODE_EFFORT: FieldSpec = choices_field(
    "effort",
    "effort",
    "reasoningEffort",
    &["low", "medium", "high", "xhigh", "max"],
);
const OMP_MODEL: FieldSpec = field("model", "model", "modelRoles.default");
// defaultThinkingLevel, the level sessions start with as omp's settings save
// it; unset omp takes high, and it also knows auto, its own pick
const OMP_EFFORT: FieldSpec = choices_field(
    "effort",
    "thinking",
    "defaultThinkingLevel",
    &["minimal", "low", "medium", "high", "xhigh", "max"],
);
const DEVIN_MODEL: FieldSpec = field("model", "model", "agent.model");
const HERMES_MODEL: FieldSpec = provider_model("model", "model", "model.provider", "model.default");
// agent.reasoning_effort, which Hermes's /reasoning saves; none turns
// reasoning off, and unset Hermes asks for medium
const HERMES_EFFORT: FieldSpec = choices_field(
    "effort",
    "effort",
    "agent.reasoning_effort",
    &["none", "minimal", "low", "medium", "high", "xhigh"],
);
const DSH_MODEL: FieldSpec = field("model", "model", "model");
// llm-deepseek's reasoningEffort, written into magpie's own entry of dsh's
// patch list, so the value is read and written with it
const DSH_EFFORT: FieldSpec = choices_field(
    "effort",
    "thinking",
    "reasoningEffort",
    &["off", "high", "max"],
);
const GROK_MODEL: FieldSpec = field("model", "model", "models.default");
const GROK_EFFORT: FieldSpec = field("effort", "effort", "models.default_reasoning_effort");
const ZCODE_PROVIDER: FieldSpec = field("provider", "provider", "provider.magpie");
const ALMA_MODEL: FieldSpec = field("model", "model", "chat.defaultModel");

pub static ALL_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        name: "Claude Code",
        aliases: &["cc"],
        executable: "claude",
        relative_path: ".claude/settings.json",
        format: ConfigFormat::Jsonc,
        fields: &[CLAUDE_MODEL, CLAUDE_EFFORT],
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
        fields: &[GOOSE_MODEL, GOOSE_EFFORT],
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
        fields: &[COPILOT_MODEL, COPILOT_EFFORT],
    },
    AgentSpec {
        id: "crush",
        name: "Crush",
        aliases: &[],
        executable: "crush",
        relative_path: ".config/crush/crush.json",
        format: ConfigFormat::Jsonc,
        fields: &[CRUSH_LARGE, CRUSH_SMALL, CRUSH_EFFORT],
    },
    AgentSpec {
        id: "dsh",
        name: "DeepSeek Harness",
        aliases: &["deepseek-harness"],
        executable: "dsh",
        relative_path: ".dsh/config.yaml",
        format: ConfigFormat::Yaml,
        fields: &[DSH_MODEL, DSH_EFFORT],
    },
    AgentSpec {
        id: "commandcode",
        name: "Command Code",
        aliases: &["command-code", "cmd"],
        executable: "command-code",
        relative_path: ".commandcode/settings.json",
        format: ConfigFormat::Jsonc,
        fields: &[COMMAND_CODE_MODEL, COMMAND_CODE_EFFORT],
    },
    AgentSpec {
        id: "omp",
        name: "omp (oh-my-pi)",
        aliases: &["oh-my-pi"],
        executable: "omp",
        relative_path: ".omp/agent/config.yml",
        format: ConfigFormat::Yaml,
        fields: &[OMP_MODEL, OMP_EFFORT],
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
        fields: &[HERMES_MODEL, HERMES_EFFORT],
    },
    AgentSpec {
        id: "grok",
        name: "Grok Build",
        aliases: &["grok-build", "grok-cli"],
        executable: "",
        relative_path: ".grok/config.toml",
        format: ConfigFormat::Toml,
        fields: &[GROK_MODEL, GROK_EFFORT],
    },
    AgentSpec {
        id: "zcode",
        name: "ZCode",
        aliases: &["z-code"],
        executable: "",
        relative_path: ".zcode/v2/config.json",
        format: ConfigFormat::Jsonc,
        fields: &[ZCODE_PROVIDER],
    },
    AgentSpec {
        id: "alma",
        name: "Alma",
        aliases: &[],
        executable: "",
        relative_path: "",
        format: ConfigFormat::Jsonc,
        fields: &[ALMA_MODEL],
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
        if self.spec.id == "alma" {
            // Alma keeps its providers in the app; its data directory is
            // the only trace magpie can see.
            return self.path.is_dir();
        }
        if self.path.is_file() || self.path.parent().is_some_and(|path| path.is_dir()) {
            return true;
        }
        executable_exists(self.spec.executable)
    }

    pub fn values(&self) -> Result<Vec<(&'static str, String)>> {
        if self.spec.id == "dsh" {
            return Ok(vec![
                ("model", crate::dsh::get(&self.path)?),
                ("effort", crate::dsh::get_effort(&self.path)?),
            ]);
        }
        if self.spec.id == "zcode" {
            let wired = zcode::wired(&self.path)?;
            return Ok(vec![(
                "provider",
                if wired {
                    MAGPIE_ID.to_owned()
                } else {
                    String::new()
                },
            )]);
        }
        if self.spec.id == "alma" {
            return Ok(vec![("model", alma::get())]);
        }

        self.spec
            .fields
            .iter()
            .map(|field| {
                if self.spec.id == "commandcode" && field.key == "effort" {
                    // Command Code keeps an effort for each model: the value
                    // shown is the current model's, not the map's.
                    return Ok((field.key, self.commandcode_effort()?));
                }
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
        let field = self.find_field(field_name)?;

        if self.spec.id == "dsh" && field.key == "model" {
            return crate::dsh::set(&self.path, value);
        }
        if self.spec.id == "grok" && field.key == "model" {
            return grok::set_model(&self.path, value);
        }
        if self.spec.id == "zcode" && field.key == "provider" {
            return zcode::set(&self.path, value);
        }
        if self.spec.id == "alma" && field.key == "model" {
            return alma::set(value);
        }
        if self.spec.id == "opencode" && !field.catalog_prefix.is_empty() {
            return self.set_opencode_model(field, value);
        }
        if self.spec.id == "commandcode" && field.key == "model" {
            return self.set_commandcode_model(value);
        }
        if self.spec.id == "pi" && field.key == "model" {
            return self.set_pi_model(value);
        }
        if self.spec.id == "pi" && field.key == "effort" {
            return self.set_pi_effort(value);
        }
        if self.spec.id == "crush" && field.provider_path.is_some() {
            return self.set_crush_model(field, value);
        }
        if self.spec.id == "hermes" && field.key == "model" {
            return self.set_hermes_model(value);
        }
        if self.spec.id == "omp" && field.key == "model" {
            return self.set_omp_model(value);
        }
        if self.spec.id == "cursor" && field.key == "model" {
            return self.set_cursor_model(value);
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
        if self.spec.id == "dsh" && field.key == "effort" {
            return crate::dsh::set_effort(&self.path, value);
        }
        if self.spec.id == "commandcode" && field.key == "effort" {
            return self.set_commandcode_effort(value);
        }
        if self.spec.id == "crush" && field.key == "effort" {
            let large = config::get(&self.path, self.spec.format, "models.large.model")?
                .unwrap_or_default();
            ensure!(
                !large.is_empty(),
                "pick Crush's large model first; the effort is kept with it"
            );
        }
        if self.spec.id == "claude" && field.key == "effort" {
            ensure!(
                value.is_empty() || CLAUDE_EFFORTS.contains(&value),
                "Claude Code keeps an effort of {}, not {value:?}",
                CLAUDE_EFFORTS.join(", ")
            );
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

    // find_field looks a field up by key or, as it is shown, by label.
    fn find_field(&self, field_name: &str) -> Result<&FieldSpec> {
        self.spec
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
            })
    }

    // apply sets one of the agent's fields and remembers it as magpie's, so
    // it can be told apart from what something else writes there later.
    // Every setting of a field by the user goes through here.
    pub fn apply(&self, field_name: &str, value: &str) -> Result<()> {
        let Ok(field) = self.find_field(field_name) else {
            return Ok(());
        };
        self.set(field.key, value)?;
        let applied_value = self
            .values()
            .ok()
            .and_then(|values| values.into_iter().find(|(key, _)| *key == field.key))
            .map_or_else(String::new, |(_, value)| value);
        let _ = applied::record(self.spec.id, field.key, &applied_value);
        Ok(())
    }

    // drift says what, if anything, keeps the agent off what magpie set on
    // it, None when nothing does.
    pub fn drift(&self) -> Option<Drift> {
        applied::drift(self)
    }

    // reapply sets again what magpie set on the agent.
    pub fn reapply(&self) -> Result<()> {
        applied::reapply(self)
    }

    // keep takes the agent's config as it is now: what magpie set before is
    // forgotten, and no longer said to have been changed.
    pub fn keep(&self) -> Result<()> {
        applied::forget(self.spec.id)
    }

    // sync rewrites the model list magpie wrote into this agent's files, as
    // the catalog is now.
    pub fn sync(&self) -> Result<()> {
        match self.spec.id {
            "grok" => grok::sync(&self.path),
            "zcode" => zcode::sync(&self.path),
            "alma" => alma::sync(),
            _ => Ok(()),
        }
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

    // commandcode_effort is the effort kept for the model Command Code is on
    // now, of the map its settings hold for every model.
    fn commandcode_effort(&self) -> Result<String> {
        let model = config::get(&self.path, self.spec.format, "model")?.unwrap_or_default();
        Ok(cc_efforts(&self.path)?
            .get(model.as_str())
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    fn set_commandcode_effort(&self, value: &str) -> Result<()> {
        let model = config::get(&self.path, self.spec.format, "model")?.unwrap_or_default();
        ensure!(
            !model.is_empty(),
            "pick Command Code's model first; it keeps an effort for each model"
        );
        let mut efforts = cc_efforts(&self.path)?;
        if value.is_empty() {
            efforts.remove(&model);
        } else {
            efforts.insert(model, Value::String(value.to_owned()));
        }
        if efforts.is_empty() {
            return config::delete(&self.path, self.spec.format, COMMAND_CODE_EFFORT.path);
        }
        // written whole, as the model ids it keys hold dots
        config::set_jsonc_values(
            &self.path,
            &[(COMMAND_CODE_EFFORT.path, Value::Object(efforts))],
        )
    }

    fn set_pi_model(&self, value: &str) -> Result<()> {
        let models_path = self.path.with_file_name("models.json");
        if value.is_empty() {
            config::delete_many(
                &self.path,
                self.spec.format,
                &["defaultProvider", "defaultModel"],
            )?;
            return config::delete(&models_path, ConfigFormat::Jsonc, "providers.magpie");
        }

        if let Some(model) = value.strip_prefix("magpie/") {
            ensure!(
                is_gateway_model(model)?,
                "{model:?} is not a model currently served by magpie"
            );
            config::set_jsonc_value(&models_path, "providers.magpie", &pi_provider()?)?;
            return config::set_jsonc_values(
                &self.path,
                &[
                    ("defaultProvider", Value::String("magpie".to_owned())),
                    ("defaultModel", Value::String(model.to_owned())),
                ],
            );
        }

        let (provider, model) = value
            .split_once('/')
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .with_context(|| format!("expected provider/model, got {value:?}"))?;
        config::set_many(
            &self.path,
            self.spec.format,
            &[("defaultProvider", provider), ("defaultModel", model)],
        )?;
        if provider != "magpie" {
            config::delete(&models_path, ConfigFormat::Jsonc, "providers.magpie")?;
        }
        Ok(())
    }

    fn set_pi_effort(&self, value: &str) -> Result<()> {
        if value.is_empty() {
            config::delete(&self.path, self.spec.format, PI_EFFORT.path)?;
        } else {
            config::set(&self.path, self.spec.format, PI_EFFORT.path, value)?;
        }
        if config::get(&self.path, self.spec.format, "defaultProvider")?.as_deref()
            == Some("magpie")
        {
            config::set_jsonc_value(
                &self.path.with_file_name("models.json"),
                "providers.magpie",
                &pi_provider()?,
            )?;
        }
        Ok(())
    }

    fn set_crush_model(&self, field: &FieldSpec, value: &str) -> Result<()> {
        let provider_path = field
            .provider_path
            .context("Crush model field has no provider path")?;
        let route = value.strip_prefix("magpie/");
        if let Some(model) = route {
            ensure!(
                is_gateway_model(model)?,
                "{model:?} is not a model currently served by magpie"
            );
            config::set_jsonc_value(&self.path, "providers.magpie", &crush_provider()?)?;
            config::set_many(
                &self.path,
                self.spec.format,
                &[(provider_path, "magpie"), (field.path, model)],
            )?;
            return Ok(());
        }

        if value.is_empty() {
            let model_path = provider_path
                .strip_suffix(".provider")
                .context("Crush provider path does not end in .provider")?;
            config::delete(&self.path, self.spec.format, model_path)?;
        } else {
            let (provider, model) = value
                .split_once('/')
                .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
                .with_context(|| format!("expected provider/model, got {value:?}"))?;
            config::set_many(
                &self.path,
                self.spec.format,
                &[(provider_path, provider), (field.path, model)],
            )?;
        }

        if !crush_has_magpie_model(self)? {
            config::delete(&self.path, self.spec.format, "providers.magpie")?;
        }
        Ok(())
    }

    fn set_hermes_model(&self, value: &str) -> Result<()> {
        let routed = hermes_gateway_configured(self)?;
        if value.is_empty() {
            if routed {
                return restore_hermes_config(self);
            }
            return config::delete(&self.path, self.spec.format, HERMES_MODEL.path);
        }

        if let Some(model) = value.strip_prefix("magpie/") {
            ensure!(
                is_gateway_model(model)?,
                "{model:?} is not a model currently served by magpie"
            );
            if !routed {
                stash_hermes_config(self)?;
            }
            config::set_yaml_values(&self.path, &[("providers.magpie", hermes_provider()?)])?;
            return config::set_many(
                &self.path,
                self.spec.format,
                &[("model.provider", "magpie"), ("model.default", model)],
            );
        }

        if routed {
            restore_hermes_config(self)?;
        }
        config::set(&self.path, self.spec.format, HERMES_MODEL.path, value)
    }

    fn set_omp_model(&self, value: &str) -> Result<()> {
        let models_path = omp_models_path(self);
        if value.is_empty() {
            config::delete(&self.path, self.spec.format, OMP_MODEL.path)?;
        } else if let Some(model) = value.strip_prefix("magpie/") {
            ensure!(
                is_gateway_model(model)?,
                "{model:?} is not a model currently served by magpie"
            );
            prepare_omp_models(self, &models_path)?;
            config::set_yaml_values(&models_path, &[("providers.magpie", omp_provider()?)])?;
            return config::set(&self.path, self.spec.format, OMP_MODEL.path, value);
        } else {
            config::set(&self.path, self.spec.format, OMP_MODEL.path, value)?;
        }

        if !omp_has_magpie_model(self)? {
            config::delete(&models_path, ConfigFormat::Yaml, "providers.magpie")?;
        }
        Ok(())
    }

    fn set_cursor_model(&self, value: &str) -> Result<()> {
        if value.is_empty() {
            return config::delete_many(
                &self.path,
                self.spec.format,
                &["model", "hasChangedDefaultModel"],
            );
        }
        config::set_jsonc_values(
            &self.path,
            &[
                ("model.modelId", Value::String(value.to_owned())),
                ("model.displayModelId", Value::String(value.to_owned())),
                ("model.displayName", Value::String(value.to_owned())),
                ("hasChangedDefaultModel", Value::Bool(true)),
            ],
        )
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

    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "pi")
        && pi_gateway_configured(&agent)?
    {
        config::set_jsonc_value(
            &agent.path.with_file_name("models.json"),
            "providers.magpie",
            &pi_provider()?,
        )?;
    }

    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "crush")
        && crush_has_magpie_model(&agent)?
    {
        config::set_jsonc_value(&agent.path, "providers.magpie", &crush_provider()?)?;
    }

    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "hermes")
        && hermes_gateway_configured(&agent)?
    {
        config::set_yaml_values(&agent.path, &[("providers.magpie", hermes_provider()?)])?;
    }

    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "omp")
        && omp_has_magpie_model(&agent)?
    {
        let models_path = omp_models_path(&agent);
        prepare_omp_models(&agent, &models_path)?;
        config::set_yaml_values(&models_path, &[("providers.magpie", omp_provider()?)])?;
    }

    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "dsh") {
        crate::dsh::sync(&agent.path)?;
    }

    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "grok") {
        grok::sync(&agent.path)?;
    }

    if let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "zcode") {
        zcode::sync(&agent.path)?;
    }

    alma::sync()?;

    let Some(agent) = all().into_iter().find(|agent| agent.spec.id == "opencode") else {
        return Ok(());
    };
    if opencode_has_magpie_model(&agent)? {
        config::set_jsonc_value(&agent.path, "provider.magpie", &opencode_provider()?)?;
    }
    Ok(())
}

pub(crate) fn is_gateway_model(value: &str) -> Result<bool> {
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

pub(crate) fn read_agent_stash() -> Result<HashMap<String, String>> {
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

pub(crate) fn write_agent_stash(stash: &HashMap<String, String>) -> Result<()> {
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

// cc_efforts is settings.json's reasoningEffort: the effort Command Code
// starts a session with, kept for each model its /effort was used on.
fn cc_efforts(path: &Path) -> Result<serde_json::Map<String, Value>> {
    Ok(
        config::get(path, ConfigFormat::Jsonc, COMMAND_CODE_EFFORT.path)?
            .and_then(|stored| serde_json::from_str::<Value>(&stored).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
    )
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

fn pi_gateway_configured(agent: &Agent) -> Result<bool> {
    Ok(
        config::get(&agent.path, agent.spec.format, "defaultProvider")?.as_deref()
            == Some("magpie"),
    )
}

fn pi_provider() -> Result<Value> {
    let (groups, entries) = crate::provider::desktop_group_data()?;
    let entries_by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut models = Vec::with_capacity(entries.len() + groups.len());

    for entry in &entries {
        let name = if entry.model.name.is_empty() {
            &entry.model.id
        } else {
            &entry.model.name
        };
        models.push(pi_model(
            &entry.id,
            &format!("{name} · {}", entry.provider_name),
            &entry.model.efforts,
            entry.model.images,
            entry.model.context,
        ));
    }
    for group in groups.into_iter().filter(|group| !group.hidden) {
        let members = group
            .members
            .iter()
            .filter_map(|member| entries_by_id.get(member.as_str()).copied())
            .collect::<Vec<_>>();
        let Some((first, rest)) = members.split_first() else {
            continue;
        };
        let efforts = first
            .model
            .efforts
            .iter()
            .filter(|effort| {
                rest.iter()
                    .all(|member| member.model.efforts.contains(effort))
            })
            .cloned()
            .collect::<Vec<_>>();
        let images = members.iter().all(|member| member.model.images);
        let context = members
            .iter()
            .map(|member| member.model.context)
            .filter(|context| *context > 0)
            .min()
            .unwrap_or_default();
        models.push(pi_model(
            &format!("group/{}", group.id),
            &format!("{} · routing group", group.name),
            &efforts,
            images,
            context,
        ));
    }

    Ok(json!({
        "name": "magpie",
        "baseUrl": crate::gateway::v1_url(),
        "api": "openai-completions",
        "apiKey": crate::gateway::TOKEN,
        "models": models,
    }))
}

fn pi_model(id: &str, name: &str, efforts: &[String], images: bool, context: usize) -> Value {
    let mut model = json!({
        "id": id,
        "name": name,
        "reasoning": !efforts.is_empty(),
    });
    let Some(fields) = model.as_object_mut() else {
        return model;
    };
    if images {
        fields.insert("input".to_owned(), json!(["text", "image"]));
    }
    let thinking_levels = efforts
        .iter()
        .filter(|effort| matches!(effort.as_str(), "xhigh" | "max"))
        .map(|effort| (effort.clone(), Value::String(effort.clone())))
        .collect::<serde_json::Map<_, _>>();
    if !thinking_levels.is_empty() {
        fields.insert(
            "thinkingLevelMap".to_owned(),
            Value::Object(thinking_levels),
        );
    }
    if context > 0 {
        fields.insert("contextWindow".to_owned(), json!(context));
    }
    model
}

fn crush_has_magpie_model(agent: &Agent) -> Result<bool> {
    for field in [CRUSH_LARGE, CRUSH_SMALL] {
        if config::get(
            &agent.path,
            agent.spec.format,
            field.provider_path.unwrap_or_default(),
        )?
        .as_deref()
            == Some("magpie")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn crush_provider() -> Result<Value> {
    let (groups, entries) = crate::provider::desktop_group_data()?;
    let entries_by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut models = Vec::with_capacity(entries.len() + groups.len());
    for entry in &entries {
        let name = if entry.model.name.is_empty() {
            &entry.model.id
        } else {
            &entry.model.name
        };
        models.push(crush_model(
            &entry.id,
            &format!("{name} · {}", entry.provider_name),
            entry.model.context,
            !entry.model.efforts.is_empty(),
        ));
    }
    for group in groups.into_iter().filter(|group| !group.hidden) {
        let members = group
            .members
            .iter()
            .filter_map(|member| entries_by_id.get(member.as_str()).copied())
            .collect::<Vec<_>>();
        let Some((first, rest)) = members.split_first() else {
            continue;
        };
        let can_reason = !first.model.efforts.is_empty()
            && first.model.efforts.iter().all(|effort| {
                rest.iter()
                    .all(|member| member.model.efforts.contains(effort))
            });
        let context = members
            .iter()
            .map(|member| member.model.context)
            .filter(|context| *context > 0)
            .min()
            .unwrap_or_default();
        models.push(crush_model(
            &format!("group/{}", group.id),
            &format!("{} · routing group", group.name),
            context,
            can_reason,
        ));
    }

    Ok(json!({
        "type": "openai",
        "name": "magpie",
        "base_url": crate::gateway::v1_url(),
        "api_key": crate::gateway::TOKEN,
        "models": models,
    }))
}

fn crush_model(id: &str, name: &str, context: usize, can_reason: bool) -> Value {
    json!({
        "id": id,
        "name": name,
        "context_window": if context == 0 { 200_000 } else { context },
        "default_max_tokens": 16_384,
        "can_reason": can_reason,
    })
}

fn hermes_gateway_configured(agent: &Agent) -> Result<bool> {
    Ok(config::get(&agent.path, agent.spec.format, "model.provider")?.as_deref() == Some("magpie"))
}

fn hermes_stash_key(agent: &Agent, key: &str) -> String {
    format!("hermes:{}:{key}", agent.path.display())
}

fn stash_hermes_config(agent: &Agent) -> Result<()> {
    let mut stash = read_agent_stash()?;
    for path in ["model.provider", "model.default"] {
        let key = hermes_stash_key(agent, path);
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

fn restore_hermes_config(agent: &Agent) -> Result<()> {
    let mut stash = read_agent_stash()?;
    let restored = ["model.provider", "model.default"]
        .into_iter()
        .filter_map(|path| {
            stash
                .remove(&hermes_stash_key(agent, path))
                .map(|value| (path.to_owned(), value))
        })
        .collect::<Vec<_>>();
    config::delete_many(
        &agent.path,
        agent.spec.format,
        &["model.provider", "model.default", "providers.magpie"],
    )?;
    let assignments = restored
        .iter()
        .map(|(path, value)| (path.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    if !assignments.is_empty() {
        config::set_many(&agent.path, agent.spec.format, &assignments)?;
    }
    write_agent_stash(&stash)
}

fn hermes_provider() -> Result<Value> {
    let (groups, entries) = crate::provider::desktop_group_data()?;
    let mut models = entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    let entries_by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    models.extend(
        groups
            .into_iter()
            .filter(|group| !group.hidden)
            .filter(|group| {
                group
                    .members
                    .iter()
                    .any(|member| entries_by_id.contains_key(member.as_str()))
            })
            .map(|group| format!("group/{}", group.id)),
    );

    Ok(json!({
        "name": "magpie",
        "base_url": crate::gateway::v1_url(),
        "api_key": crate::gateway::TOKEN,
        "api_mode": "chat_completions",
        "extra_headers": {"User-Agent": "hermes-agent"},
        "models": models,
    }))
}

fn omp_models_path(agent: &Agent) -> PathBuf {
    let directory = agent
        .path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let yml = directory.join("models.yml");
    if yml.exists() {
        return yml;
    }
    let yaml = directory.join("models.yaml");
    if yaml.exists() {
        return yaml;
    }
    yml
}

fn prepare_omp_models(agent: &Agent, path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let legacy_json = agent.path.with_file_name("models.json");
    config::convert_jsonc_to_yaml(&legacy_json, path)
}

fn omp_has_magpie_model(agent: &Agent) -> Result<bool> {
    Ok(config::yaml_mapping_values(&agent.path, "modelRoles")?
        .iter()
        .any(|model| model.starts_with("magpie/")))
}

fn omp_provider() -> Result<Value> {
    const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

    let (groups, entries) = crate::provider::desktop_group_data()?;
    let entries_by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut models = Vec::with_capacity(entries.len() + groups.len());
    for entry in &entries {
        let name = if entry.model.name.is_empty() {
            &entry.model.id
        } else {
            &entry.model.name
        };
        models.push(omp_model(
            &entry.id,
            &format!("{name} · {}", entry.provider_name),
            entry.model.context,
            &entry.model.efforts,
            EFFORTS,
        ));
    }
    for group in groups.into_iter().filter(|group| !group.hidden) {
        let members = group
            .members
            .iter()
            .filter_map(|member| entries_by_id.get(member.as_str()).copied())
            .collect::<Vec<_>>();
        let Some((first, rest)) = members.split_first() else {
            continue;
        };
        let efforts = first
            .model
            .efforts
            .iter()
            .filter(|effort| {
                rest.iter()
                    .all(|member| member.model.efforts.contains(effort))
            })
            .cloned()
            .collect::<Vec<_>>();
        let context = members
            .iter()
            .map(|member| member.model.context)
            .filter(|context| *context > 0)
            .min()
            .unwrap_or_default();
        models.push(omp_model(
            &format!("group/{}", group.id),
            &format!("{} · routing group", group.name),
            context,
            &efforts,
            EFFORTS,
        ));
    }

    Ok(json!({
        "baseUrl": crate::gateway::v1_url(),
        "api": "openai-completions",
        "auth": "none",
        "models": models,
    }))
}

fn omp_model(
    id: &str,
    name: &str,
    context: usize,
    efforts: &[String],
    known_efforts: &[&str],
) -> Value {
    let efforts = known_efforts
        .iter()
        .filter(|effort| {
            efforts
                .iter()
                .any(|model_effort| model_effort.as_str() == **effort)
        })
        .copied()
        .collect::<Vec<_>>();
    let mut model = json!({
        "id": id,
        "name": name,
        "reasoning": !efforts.is_empty(),
    });
    let Some(fields) = model.as_object_mut() else {
        return model;
    };
    if context > 0 {
        fields.insert("contextWindow".to_owned(), json!(context));
    }
    if !efforts.is_empty() {
        fields.insert(
            "thinking".to_owned(),
            json!({"mode": "effort", "efforts": efforts}),
        );
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
        "grok" => env_path("GROK_HOME")
            .unwrap_or_else(|| home.join(".grok"))
            .join("config.toml"),
        "zcode" => home.join(".zcode/v2/config.json"),
        "alma" => alma::dir(),
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
    if name.is_empty() {
        return false;
    }
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
pub(crate) mod testing {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Mutex, MutexGuard, PoisonError,
            atomic::{AtomicU64, Ordering},
        },
    };

    static LOCK: Mutex<()> = Mutex::new(());
    static CATALOG: Mutex<Option<Vec<super::MagpieModel>>> = Mutex::new(None);
    static APPLIED: Mutex<Option<PathBuf>> = Mutex::new(None);
    static ALMA: Mutex<Option<PathBuf>> = Mutex::new(None);
    static ALMA_API: Mutex<Option<String>> = Mutex::new(None);
    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn catalog() -> Option<Vec<super::MagpieModel>> {
        CATALOG
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn applied_path() -> Option<PathBuf> {
        APPLIED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn alma_dir() -> Option<PathBuf> {
        ALMA.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub(crate) fn alma_api() -> Option<String> {
        ALMA_API
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_catalog(models: Vec<super::MagpieModel>) {
        *CATALOG.lock().unwrap_or_else(PoisonError::into_inner) = Some(models);
    }

    pub(crate) fn set_alma_api(base: &str) {
        *ALMA_API.lock().unwrap_or_else(PoisonError::into_inner) = Some(base.to_owned());
    }

    // model is one catalog entry of the fake catalog: no efforts, no
    // context, no images.
    pub(crate) fn model(id: &str, name: &str) -> super::MagpieModel {
        super::MagpieModel {
            id: id.to_owned(),
            name: name.to_owned(),
            efforts: Vec::new(),
            images: false,
            context: 0,
            output: 0,
        }
    }

    // isolation replaces the catalog with two models of one provider, and
    // redirects applied.json and Alma's data directory at a fresh temp
    // directory — the real user's files are never touched. While an
    // Isolation is alive, tests that hold another wait.
    pub(crate) fn isolation() -> Isolation {
        let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let home =
            std::env::temp_dir().join(format!("magpie-agent-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        *CATALOG.lock().unwrap_or_else(PoisonError::into_inner) = Some(vec![
            model("deepseek/pro", "pro · DeepSeek"),
            model("deepseek/flash", "flash · DeepSeek"),
        ]);
        *APPLIED.lock().unwrap_or_else(PoisonError::into_inner) = Some(home.join("applied.json"));
        *ALMA.lock().unwrap_or_else(PoisonError::into_inner) = Some(home.join("alma"));
        *ALMA_API.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Isolation {
            _guard: guard,
            home,
        }
    }

    pub(crate) struct Isolation {
        _guard: MutexGuard<'static, ()>,
        home: PathBuf,
    }

    impl Isolation {
        pub(crate) fn path(&self) -> &std::path::Path {
            &self.home
        }
    }

    impl Drop for Isolation {
        fn drop(&mut self) {
            *CATALOG.lock().unwrap_or_else(PoisonError::into_inner) = None;
            *APPLIED.lock().unwrap_or_else(PoisonError::into_inner) = None;
            *ALMA.lock().unwrap_or_else(PoisonError::into_inner) = None;
            *ALMA_API.lock().unwrap_or_else(PoisonError::into_inner) = None;
            let _ = fs::remove_dir_all(&self.home);
        }
    }
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
