use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::settings;

mod balance;
mod icon;
mod import;
mod import_apps;
mod test;
pub(crate) use import::command as import_command;

const USAGE: &str = "usage: magpie presets | magpie providers | magpie models | magpie provider <id> | magpie provider add <preset> [key] | magpie provider add <name> id=<id> url=<url> key=<key> | magpie provider models <id> [ids…] | magpie provider test <id> | magpie provider icon <id> <file|name> | magpie provider key <id> <key> | magpie provider keys <id> [add <key> [name=<name>] [protocol=<protocol>] | use|on|off|rm <key-id> | rename <key-id> <name> | protocol <key-id> <protocol|any>] | magpie provider routing <id> [smart|order|rotate|usage] | magpie provider affinity <id> [auto|session|turn|off] | magpie provider fallback <id> [provider/model… | none] | magpie provider rm <id>";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresetKind {
    Vendor,
    Relay,
    Local,
}

struct PresetRegion {
    id: &'static str,
    chat: &'static str,
    responses: &'static str,
    anthropic: &'static str,
}

struct Preset {
    id: &'static str,
    name: &'static str,
    icon: &'static str,
    kind: PresetKind,
    chat: &'static str,
    responses: &'static str,
    anthropic: &'static str,
    catalog: &'static str,
    website: &'static str,
    keys_url: &'static str,
    regions: &'static [PresetRegion],
}

impl Preset {
    const EMPTY: Self = Self {
        id: "",
        name: "",
        icon: "",
        kind: PresetKind::Vendor,
        chat: "",
        responses: "",
        anthropic: "",
        catalog: "",
        website: "",
        keys_url: "",
        regions: &[],
    };

    fn provider(&self) -> Provider {
        Provider {
            id: self.id.to_owned(),
            name: self.name.to_owned(),
            icon: self.icon.to_owned(),
            preset: self.id.to_owned(),
            chat: self.chat.to_owned(),
            responses: self.responses.to_owned(),
            anthropic: self.anthropic.to_owned(),
            catalog: self.catalog.to_owned(),
            website: self.website.to_owned(),
            keys_url: self.keys_url.to_owned(),
            ..Provider::default()
        }
    }
}

const YYLX_REGIONS: &[PresetRegion] = &[
    PresetRegion {
        id: "auto",
        chat: "https://app.yylx.io/v1",
        responses: "",
        anthropic: "https://app.yylx.io",
    },
    PresetRegion {
        id: "global",
        chat: "https://global.yylx.io/v1",
        responses: "",
        anthropic: "https://global.yylx.io",
    },
    PresetRegion {
        id: "cn",
        chat: "https://cn.yylx.io/v1",
        responses: "",
        anthropic: "https://cn.yylx.io",
    },
];

const PRESETS: &[Preset] = &[
    Preset {
        id: "anthropic",
        name: "Anthropic",
        icon: "claude-color",
        anthropic: "https://api.anthropic.com",
        catalog: "anthropic",
        website: "https://console.anthropic.com",
        keys_url: "https://console.anthropic.com/settings/keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "openai",
        name: "OpenAI",
        icon: "openai",
        chat: "https://api.openai.com/v1",
        responses: "https://api.openai.com/v1",
        catalog: "openai",
        website: "https://platform.openai.com",
        keys_url: "https://platform.openai.com/api-keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "google",
        name: "Google Gemini",
        icon: "gemini-color",
        chat: "https://generativelanguage.googleapis.com/v1beta/openai",
        catalog: "google",
        website: "https://aistudio.google.com",
        keys_url: "https://aistudio.google.com/apikey",
        ..Preset::EMPTY
    },
    Preset {
        id: "deepseek",
        name: "DeepSeek",
        icon: "deepseek-color",
        chat: "https://api.deepseek.com/v1",
        responses: "https://api.deepseek.com/v1",
        anthropic: "https://api.deepseek.com/anthropic",
        catalog: "deepseek",
        website: "https://platform.deepseek.com",
        keys_url: "https://platform.deepseek.com/api_keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "xai",
        name: "xAI",
        icon: "xai",
        chat: "https://api.x.ai/v1",
        responses: "https://api.x.ai/v1",
        anthropic: "https://api.x.ai",
        catalog: "xai",
        website: "https://console.x.ai",
        keys_url: "https://console.x.ai",
        ..Preset::EMPTY
    },
    Preset {
        id: "moonshot",
        name: "Kimi",
        icon: "kimi",
        chat: "https://api.moonshot.ai/v1",
        anthropic: "https://api.moonshot.ai/anthropic",
        catalog: "moonshotai",
        website: "https://platform.moonshot.ai",
        keys_url: "https://platform.moonshot.ai/console/api-keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "moonshot-cn",
        name: "Kimi (China)",
        icon: "kimi",
        chat: "https://api.moonshot.cn/v1",
        anthropic: "https://api.moonshot.cn/anthropic",
        catalog: "moonshotai",
        website: "https://platform.moonshot.cn",
        keys_url: "https://platform.moonshot.cn/console/api-keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "zhipu",
        name: "Zhipu GLM",
        icon: "zhipu-color",
        chat: "https://open.bigmodel.cn/api/paas/v4",
        anthropic: "https://open.bigmodel.cn/api/anthropic",
        catalog: "zhipuai",
        website: "https://open.bigmodel.cn",
        keys_url: "https://open.bigmodel.cn/usercenter/proj-mgmt/apikeys",
        ..Preset::EMPTY
    },
    Preset {
        id: "zai",
        name: "Z.ai",
        icon: "zai",
        chat: "https://api.z.ai/api/paas/v4",
        anthropic: "https://api.z.ai/api/anthropic",
        catalog: "zhipuai",
        website: "https://z.ai",
        keys_url: "https://z.ai/manage-apikey/apikey-list",
        ..Preset::EMPTY
    },
    Preset {
        id: "minimax",
        name: "MiniMax",
        icon: "minimax-color",
        chat: "https://api.minimax.io/v1",
        anthropic: "https://api.minimax.io/anthropic",
        catalog: "minimax",
        website: "https://platform.minimax.io",
        keys_url: "https://platform.minimax.io/user-center/basic-information/interface-key",
        ..Preset::EMPTY
    },
    Preset {
        id: "minimax-cn",
        name: "MiniMax (China)",
        icon: "minimax-color",
        chat: "https://api.minimaxi.com/v1",
        anthropic: "https://api.minimaxi.com/anthropic",
        catalog: "minimax",
        website: "https://platform.minimaxi.com",
        keys_url: "https://platform.minimaxi.com/user-center/basic-information/interface-key",
        ..Preset::EMPTY
    },
    Preset {
        id: "qwen",
        name: "Qwen",
        icon: "qwen-color",
        chat: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        anthropic: "https://dashscope-intl.aliyuncs.com/apps/anthropic",
        catalog: "alibaba",
        website: "https://modelstudio.console.alibabacloud.com",
        keys_url: "https://modelstudio.console.alibabacloud.com/?tab=playground#/api-key",
        ..Preset::EMPTY
    },
    Preset {
        id: "qwen-cn",
        name: "Qwen (China)",
        icon: "qwen-color",
        chat: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        anthropic: "https://dashscope.aliyuncs.com/apps/anthropic",
        catalog: "alibaba",
        website: "https://bailian.console.aliyun.com",
        keys_url: "https://bailian.console.aliyun.com/?tab=model#/api-key",
        ..Preset::EMPTY
    },
    Preset {
        id: "mistral",
        name: "Mistral",
        icon: "mistral-color",
        chat: "https://api.mistral.ai/v1",
        catalog: "mistral",
        website: "https://console.mistral.ai",
        keys_url: "https://console.mistral.ai/api-keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "groq",
        name: "Groq",
        icon: "groq",
        chat: "https://api.groq.com/openai/v1",
        responses: "https://api.groq.com/openai/v1",
        catalog: "groq",
        website: "https://console.groq.com",
        keys_url: "https://console.groq.com/keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "ollama-cloud",
        name: "Ollama Cloud",
        icon: "ollama",
        chat: "https://ollama.com/v1",
        anthropic: "https://ollama.com",
        catalog: "ollama-cloud",
        website: "https://docs.ollama.com/cloud",
        keys_url: "https://ollama.com/settings/keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        icon: "openrouter",
        kind: PresetKind::Relay,
        chat: "https://openrouter.ai/api/v1",
        anthropic: "https://openrouter.ai/api",
        catalog: "openrouter",
        website: "https://openrouter.ai",
        keys_url: "https://openrouter.ai/keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "opencode-go",
        name: "OpenCode Go",
        icon: "opencode",
        kind: PresetKind::Relay,
        chat: "https://opencode.ai/zen/go/v1",
        responses: "https://opencode.ai/zen/go/v1",
        anthropic: "https://opencode.ai/zen/go",
        catalog: "opencode-go",
        website: "https://opencode.ai/docs/go",
        keys_url: "https://opencode.ai/auth",
        regions: &[],
    },
    Preset {
        id: "opencode-zen",
        name: "OpenCode Zen",
        icon: "opencode",
        kind: PresetKind::Relay,
        chat: "https://opencode.ai/zen/v1",
        responses: "https://opencode.ai/zen/v1",
        anthropic: "https://opencode.ai/zen",
        catalog: "opencode",
        website: "https://opencode.ai/docs/zen",
        keys_url: "https://opencode.ai/auth",
        regions: &[],
    },
    Preset {
        id: "together",
        name: "Together AI",
        icon: "together-color",
        kind: PresetKind::Relay,
        chat: "https://api.together.xyz/v1",
        catalog: "togetherai",
        website: "https://api.together.ai",
        keys_url: "https://api.together.ai/settings/api-keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "fireworks",
        name: "Fireworks",
        icon: "fireworks-color",
        kind: PresetKind::Relay,
        chat: "https://api.fireworks.ai/inference/v1",
        catalog: "fireworks-ai",
        website: "https://fireworks.ai",
        keys_url: "https://app.fireworks.ai/settings/users/api-keys",
        ..Preset::EMPTY
    },
    Preset {
        id: "siliconflow",
        name: "SiliconFlow",
        icon: "siliconcloud-color",
        kind: PresetKind::Relay,
        chat: "https://api.siliconflow.cn/v1",
        catalog: "siliconflow",
        website: "https://cloud.siliconflow.cn",
        keys_url: "https://cloud.siliconflow.cn/account/ak",
        ..Preset::EMPTY
    },
    Preset {
        id: "aihubmix",
        name: "AiHubMix",
        icon: "aihubmix-color",
        kind: PresetKind::Relay,
        chat: "https://aihubmix.com/v1",
        anthropic: "https://aihubmix.com",
        website: "https://aihubmix.com",
        keys_url: "https://console.aihubmix.com/token",
        ..Preset::EMPTY
    },
    Preset {
        id: "302ai",
        name: "302.AI",
        icon: "ai302-color",
        kind: PresetKind::Relay,
        chat: "https://api.302.ai/v1",
        anthropic: "https://api.302.ai",
        website: "https://302.ai",
        keys_url: "https://302.ai/api-keys/list",
        ..Preset::EMPTY
    },
    Preset {
        id: "yylx",
        name: "鱼鱼连线",
        icon: "yylx",
        kind: PresetKind::Relay,
        chat: "https://app.yylx.io/v1",
        anthropic: "https://app.yylx.io",
        website: "https://yylx.io",
        keys_url: "https://app.yylx.io/keys",
        regions: YYLX_REGIONS,
        ..Preset::EMPTY
    },
    Preset {
        id: "ollama",
        name: "Ollama",
        icon: "ollama",
        kind: PresetKind::Local,
        chat: "http://localhost:11434/v1",
        anthropic: "http://localhost:11434",
        website: "https://ollama.com",
        ..Preset::EMPTY
    },
    Preset {
        id: "lmstudio",
        name: "LM Studio",
        icon: "lmstudio",
        kind: PresetKind::Local,
        chat: "http://localhost:1234/v1",
        website: "https://lmstudio.ai",
        ..Preset::EMPTY
    },
];

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ProviderFile {
    providers: Vec<Provider>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: Vec<Group>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub routing: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub affinity: String,
    #[serde(skip_serializing_if = "is_false")]
    pub auto: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
pub struct ModelEntry {
    pub id: String,
    pub model: crate::catalog::Model,
    pub provider_id: String,
    pub provider_name: String,
    pub icon: String,
}

#[derive(Clone, Debug)]
pub struct DesktopKey {
    pub id: String,
    pub name: String,
    pub primary: bool,
    pub active: bool,
    pub protocol: String,
}

#[derive(Clone, Debug)]
pub struct DesktopProvider {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub protocols: Vec<String>,
    pub active_keys: usize,
    pub has_key: bool,
    pub key_required: bool,
    pub account: bool,
    pub routing: String,
    pub affinity: String,
    pub keys: Vec<DesktopKey>,
    pub hidden: bool,
    pub models: Vec<String>,
    pub model_count: usize,
}

#[derive(Clone, Debug)]
pub struct DesktopPreset {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub key_required: bool,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct KeyAccount {
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    key: String,
    #[serde(skip_serializing_if = "is_false")]
    off: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    protocol: String,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Provider {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    icon: String,
    #[serde(skip)]
    icon_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    preset: String,
    key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    key_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keys: Vec<KeyAccount>,
    #[serde(skip_serializing_if = "String::is_empty")]
    key_protocol: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    chat: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    responses: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    anthropic: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fallback: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    routing: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    affinity: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    balance_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    balance_path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    models: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    catalog: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    website: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    keys_url: String,
    #[serde(skip_serializing_if = "is_false")]
    hidden: bool,
    #[serde(skip)]
    account: Option<ProviderAccount>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone)]
pub(crate) enum ProviderAccount {
    Codex { auth_file: PathBuf },
    Copilot { account: crate::copilot::Account },
}

pub(crate) struct GatewayProvider {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) chat: String,
    pub(crate) responses: String,
    pub(crate) anthropic: String,
    pub(crate) fallback: Vec<String>,
    pub(crate) routing: String,
    pub(crate) affinity: String,
    pub(crate) keys: Vec<GatewayKey>,
    pub(crate) has_configured_keys: bool,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) models: Vec<String>,
    pub(crate) model_keys: HashMap<String, HashSet<String>>,
    pub(crate) model_apis: HashMap<String, HashSet<String>>,
    pub(crate) account: Option<ProviderAccount>,
    pub(crate) hidden: bool,
}

pub(crate) struct GatewayKey {
    pub(crate) key: String,
    pub(crate) protocol: String,
    pub(crate) active: bool,
}

pub(crate) fn key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut fingerprint = String::with_capacity(10);
    use std::fmt::Write as _;
    for byte in &digest[..5] {
        let _ = write!(fingerprint, "{byte:02x}");
    }
    fingerprint
}

fn providers_with_local_accounts(mut providers: Vec<Provider>) -> Vec<Provider> {
    if !providers.iter().any(|provider| provider.id == "codex")
        && let Some(auth_file) = crate::codex::signed_in_auth_file()
    {
        providers.push(Provider {
            id: "codex".to_owned(),
            name: "Codex".to_owned(),
            icon: "codex-color".to_owned(),
            responses: "https://chatgpt.com/backend-api/codex".to_owned(),
            catalog: "codex".to_owned(),
            website: "https://chatgpt.com/codex".to_owned(),
            models: Vec::new(),
            account: Some(ProviderAccount::Codex { auth_file }),
            ..Provider::default()
        });
    }
    if !providers.iter().any(|provider| provider.id == "copilot")
        && let Some(account) = crate::copilot::signed_in_account()
    {
        providers.push(Provider {
            id: "copilot".to_owned(),
            name: "Copilot".to_owned(),
            icon: "githubcopilot".to_owned(),
            chat: "https://api.githubcopilot.com".to_owned(),
            responses: "https://api.githubcopilot.com".to_owned(),
            anthropic: "https://api.githubcopilot.com".to_owned(),
            catalog: "copilot".to_owned(),
            website: "https://github.com/features/copilot".to_owned(),
            account: Some(ProviderAccount::Copilot { account }),
            ..Provider::default()
        });
    }
    providers
}

fn has_provider_credential(provider: &Provider) -> bool {
    !provider.key.is_empty()
        || provider
            .keys
            .iter()
            .any(|key| !key.off && !key.key.is_empty())
        || provider.is_local()
        || provider.account.is_some()
}

fn account_label(account: &ProviderAccount) -> String {
    match account {
        ProviderAccount::Copilot { account } if !account.user.is_empty() => {
            format!("signed in as {}", account.user)
        }
        _ => "signed in".to_owned(),
    }
}

fn host_of(url: &str) -> String {
    let url = url.trim();
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

impl GatewayProvider {
    pub(crate) fn host(&self) -> String {
        for url in [&self.chat, &self.responses, &self.anthropic] {
            if !url.is_empty() {
                return host_of(url);
            }
        }
        String::new()
    }

    // Where is what the provider's calls go to, as usage keeps it: the API's
    // host, and for a subscription who is signed in there too. The id alone
    // can't tell: it can be given to another vendor or account later.
    pub(crate) fn where_(&self) -> String {
        let host = self.host();
        let user = match self.account.as_ref() {
            Some(ProviderAccount::Codex { .. }) => {
                crate::codex::signed_in_identity().map_or_else(String::new, |(user, _)| user)
            }
            Some(ProviderAccount::Copilot { account }) => account.user.clone(),
            None => String::new(),
        };
        match (host.is_empty(), user.is_empty()) {
            (true, true) => String::new(),
            (true, false) => user,
            (false, true) => host,
            (false, false) => format!("{host} as {user}"),
        }
    }
}

pub(crate) struct GatewayGroup {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) members: Vec<String>,
    pub(crate) routing: String,
    pub(crate) affinity: String,
}

pub(crate) struct GatewayCatalog {
    pub(crate) providers: Vec<GatewayProvider>,
    pub(crate) groups: Vec<GatewayGroup>,
}

pub(crate) fn gateway_catalog() -> Result<GatewayCatalog> {
    let mut file = load()?;
    file.providers = providers_with_local_accounts(file.providers);
    let entries = model_entries(&file.providers);
    let groups = groups_in(&file.groups, &entries)
        .into_iter()
        .filter(|group| {
            !group.hidden
                && group
                    .members
                    .iter()
                    .any(|member| has_ready_member(member, &file.providers))
        })
        .map(|group| GatewayGroup {
            id: group.id,
            name: group.name,
            members: group.members,
            routing: group.routing,
            affinity: group.affinity,
        })
        .collect();
    let providers = file
        .providers
        .into_iter()
        .map(|provider| {
            let models = crate::catalog::exposed_models(
                &provider.id,
                provider.catalog_id(),
                &provider.models,
            )
            .into_iter()
            .map(|model| model.id)
            .collect();
            let model_keys = crate::catalog::available_models(&provider.id, provider.catalog_id())
                .into_iter()
                .filter(|model| !model.keys.is_empty())
                .map(|model| (model.id, model.keys.into_iter().collect()))
                .collect();
            let model_apis = crate::catalog::available_models(&provider.id, provider.catalog_id())
                .into_iter()
                .filter(|model| !model.apis.is_empty())
                .map(|model| (model.id, model.apis.into_iter().collect()))
                .collect();
            let keys = std::iter::once(GatewayKey {
                key: provider.key.clone(),
                protocol: provider.key_protocol.clone(),
                active: true,
            })
            .filter(|key| !key.key.is_empty())
            .chain(
                provider
                    .keys
                    .iter()
                    .filter(|key| !key.key.is_empty())
                    .map(|key| GatewayKey {
                        key: key.key.clone(),
                        protocol: key.protocol.clone(),
                        active: !key.off,
                    }),
            )
            .collect();
            let has_configured_keys =
                !provider.key.is_empty() || provider.keys.iter().any(|key| !key.key.is_empty());
            GatewayProvider {
                id: provider.id,
                name: provider.name,
                chat: provider.chat,
                responses: provider.responses,
                anthropic: provider.anthropic,
                fallback: provider.fallback,
                routing: provider.routing,
                affinity: provider.affinity,
                keys,
                has_configured_keys,
                headers: provider.headers,
                models,
                model_keys,
                model_apis,
                account: provider.account.clone(),
                hidden: provider.hidden,
            }
        })
        .collect();
    Ok(GatewayCatalog { providers, groups })
}

pub fn available_model_entries() -> Result<Vec<ModelEntry>> {
    Ok(desktop_group_data()?.1)
}

pub fn groups() -> Result<Vec<Group>> {
    Ok(desktop_group_data()?.0)
}

pub fn desktop_group_data() -> Result<(Vec<Group>, Vec<ModelEntry>)> {
    let mut file = load()?;
    file.providers = providers_with_local_accounts(file.providers);
    let entries = model_entries(&file.providers);
    Ok((groups_in(&file.groups, &entries), entries))
}

pub fn desktop_presets() -> Vec<DesktopPreset> {
    PRESETS
        .iter()
        .map(|preset| DesktopPreset {
            id: preset.id.to_owned(),
            name: preset.name.to_owned(),
            endpoint: [preset.chat, preset.responses, preset.anthropic]
                .into_iter()
                .find(|endpoint| !endpoint.is_empty())
                .unwrap_or_default()
                .to_owned(),
            key_required: preset.kind != PresetKind::Local,
        })
        .collect()
}

pub fn desktop_providers() -> Result<Vec<DesktopProvider>> {
    providers_with_local_accounts(load()?.providers)
        .into_iter()
        .map(|provider| {
            let exposed = crate::catalog::exposed_models(
                &provider.id,
                provider.catalog_id(),
                &provider.models,
            );
            let model_count = exposed.len();
            let models = exposed
                .into_iter()
                .take(8)
                .map(|model| {
                    if model.name.is_empty() {
                        model.id
                    } else {
                        model.name
                    }
                })
                .collect();
            let protocols = [
                ("Chat Completions", &provider.chat),
                ("Responses", &provider.responses),
                ("Anthropic Messages", &provider.anthropic),
            ]
            .into_iter()
            .filter(|(_, endpoint)| !endpoint.is_empty())
            .map(|(protocol, _)| protocol.to_owned())
            .collect();
            let endpoint = [&provider.chat, &provider.responses, &provider.anthropic]
                .into_iter()
                .find(|endpoint| !endpoint.is_empty())
                .cloned()
                .unwrap_or_default();
            let has_key = !provider.key.is_empty()
                || provider
                    .keys
                    .iter()
                    .any(|key| !key.key.is_empty() && !key.off);
            let active_keys = usize::from(!provider.key.is_empty())
                + provider
                    .keys
                    .iter()
                    .filter(|key| !key.key.is_empty() && !key.off)
                    .count();
            let key_required = !provider.is_local() && provider.account.is_none();
            let account = provider.account.is_some();
            let routing = if provider.routing.is_empty() {
                "smart".to_owned()
            } else {
                provider.routing.clone()
            };
            let affinity = affinity_name(&provider.affinity).to_owned();
            let mut keys = Vec::with_capacity(provider.keys.len() + 1);
            if !provider.key.is_empty() {
                keys.push(DesktopKey {
                    id: key_id(&provider.key),
                    name: provider.key_name.clone(),
                    primary: true,
                    active: true,
                    protocol: if provider.key_protocol.is_empty() {
                        "any".to_owned()
                    } else {
                        provider.key_protocol.clone()
                    },
                });
            }
            keys.extend(
                provider
                    .keys
                    .iter()
                    .filter(|key| !key.key.is_empty())
                    .map(|key| DesktopKey {
                        id: key_id(&key.key),
                        name: key.name.clone(),
                        primary: false,
                        active: !key.off,
                        protocol: if key.protocol.is_empty() {
                            "any".to_owned()
                        } else {
                            key.protocol.clone()
                        },
                    }),
            );

            Ok(DesktopProvider {
                id: provider.id,
                name: provider.name,
                endpoint,
                protocols,
                active_keys,
                has_key,
                key_required,
                account,
                routing,
                affinity,
                keys,
                hidden: provider.hidden,
                models,
                model_count,
            })
        })
        .collect()
}

pub fn add_desktop_preset(id: &str, key: &str) -> Result<String> {
    let preset = find_preset(id).with_context(|| format!("unknown provider preset {id:?}"))?;
    let mut provider = preset.provider();
    provider.key = key.trim().to_owned();
    Ok(store_new_provider(provider)?.id)
}

pub fn add_desktop_custom(name: &str, endpoint: &str, key: &str) -> Result<String> {
    let provider = Provider {
        name: name.trim().to_owned(),
        chat: endpoint.trim().to_owned(),
        key: key.trim().to_owned(),
        ..Provider::default()
    };
    Ok(store_new_provider(provider)?.id)
}

pub fn set_desktop_key(id: &str, key: &str) -> Result<()> {
    ensure!(!key.trim().is_empty(), "provider key is empty");
    let key = key.trim();
    let fingerprint = key_id(key);
    update_desktop_provider(id, |provider| {
        ensure!(
            provider.account.is_none(),
            "signed-in provider keys are managed by the agent"
        );
        ensure!(
            !provider
                .keys
                .iter()
                .any(|saved| key_id(&saved.key) == fingerprint),
            "key is already configured as a secondary key"
        );
        provider.key = key.to_owned();
        Ok(())
    })
}

pub fn add_desktop_key(id: &str, key: &str, name: &str, protocol: &str) -> Result<()> {
    let key = key.trim();
    let name = name.trim().to_owned();
    let protocol = parse_key_protocol(protocol)?;
    update_desktop_provider(id, |provider| {
        ensure!(
            provider.account.is_none(),
            "signed-in provider keys are managed by the agent"
        );
        add_key_to_provider(provider, key, name, protocol)?;
        Ok(())
    })
}

pub fn update_desktop_key(id: &str, action: &str, reference: &str) -> Result<()> {
    update_key_for(id, action, reference, false)
}

pub fn set_desktop_key_details(
    id: &str,
    reference: &str,
    name: &str,
    protocol: &str,
) -> Result<()> {
    let name = name.trim().to_owned();
    let protocol = parse_key_protocol(protocol)?;
    update_desktop_provider(id, |provider| {
        let index = locate_key(provider, reference)
            .with_context(|| format!("{} has no key {reference:?}", provider.name))?;
        if index == 0 {
            provider.key_name = name;
            provider.key_protocol = protocol;
        } else {
            provider.keys[index - 1].name = name;
            provider.keys[index - 1].protocol = protocol;
        }
        Ok(())
    })
}

pub fn set_desktop_routing(id: &str, routing: &str) -> Result<()> {
    let routing = normalize_routing(routing)?;
    update_desktop_provider(id, |provider| {
        provider.routing = routing;
        Ok(())
    })
}

pub fn set_desktop_affinity(id: &str, affinity: &str) -> Result<()> {
    let affinity = normalize_affinity(affinity)?;
    update_desktop_provider(id, |provider| {
        provider.affinity = affinity;
        Ok(())
    })
}

fn update_desktop_provider(
    id: &str,
    update: impl FnOnce(&mut Provider) -> Result<()>,
) -> Result<()> {
    let mut file = load()?;
    update(find_provider_mut(&mut file, id)?)?;
    store(file)
}

pub fn remove_desktop_provider(id: &str) -> Result<()> {
    remove_provider_data(id).map(|_| ())
}

pub async fn refresh_desktop_models(id: &str) -> Result<usize> {
    let provider = find(id)?;
    refresh_models(&provider, false).await
}

pub fn add_desktop_group(mut group: Group) -> Result<String> {
    group.name = group.name.trim().to_owned();
    ensure!(!group.name.is_empty(), "group name is empty");
    let base = slug(&group.name);
    let base = if base.is_empty() {
        "group".to_owned()
    } else {
        base
    };
    let groups = groups()?;
    group.id = if groups
        .iter()
        .any(|existing| existing.id.eq_ignore_ascii_case(&base))
    {
        (2_u32..)
            .map(|suffix| format!("{base}-{suffix}"))
            .find(|candidate| {
                groups
                    .iter()
                    .all(|existing| !existing.id.eq_ignore_ascii_case(candidate))
            })
            .context("could not generate a unique group id")?
    } else {
        base
    };
    let id = group.id.clone();
    save_group(group)?;
    Ok(id)
}

pub fn save_group(mut group: Group) -> Result<()> {
    group.id = group.id.trim().to_ascii_lowercase();
    if group.id.is_empty() {
        group.id = slug(&group.name);
    }
    ensure!(
        !group.id.is_empty() && slug(&group.id) == group.id,
        "a group's id must be lowercase letters, digits and dashes, not {:?}",
        group.id
    );
    group.name = group.name.trim().to_owned();
    if group.name.is_empty() {
        group.name.clone_from(&group.id);
    }
    group.members = group
        .members
        .into_iter()
        .map(|member| member.trim().to_owned())
        .filter(|member| !member.is_empty() && !member.starts_with("group/"))
        .fold(Vec::new(), |mut members, member| {
            if !members.contains(&member) {
                members.push(member);
            }
            members
        });
    ensure!(!group.members.is_empty(), "a group needs a model in it");
    if !matches!(group.routing.as_str(), "order" | "rotate" | "usage") {
        group.routing.clear();
    }
    if !matches!(group.affinity.as_str(), "session" | "turn" | "off" | "") {
        group.affinity.clear();
    }
    group.auto = false;
    group.hidden = false;

    let mut file = load()?;
    if let Some(existing) = file.groups.iter_mut().find(|saved| saved.id == group.id) {
        *existing = group;
    } else {
        file.groups.push(group);
    }
    store(file)
}

pub fn delete_group(id: &str) -> Result<()> {
    let mut file = load()?;
    let original_len = file.groups.len();
    file.groups.retain(|group| group.id != id);
    let auto_exists = auto_groups(&model_entries(&file.providers))
        .iter()
        .any(|group| group.id == id);
    ensure!(
        file.groups.len() != original_len || auto_exists,
        "no group {id:?}"
    );
    if auto_exists {
        file.groups.push(Group {
            id: id.to_owned(),
            hidden: true,
            ..Group::default()
        });
    }
    store(file)
}

pub fn restore_group(id: &str) -> Result<()> {
    let mut file = load()?;
    file.groups
        .retain(|group| !(group.id == id && group.hidden));
    store(file)
}

pub fn list() -> Result<()> {
    let providers = providers_with_local_accounts(load()?.providers);
    if providers.is_empty() {
        println!("no providers yet · magpie provider add <name> url=<url> key=<key>");
        return Ok(());
    }

    struct Row {
        name: String,
        id: String,
        host: String,
        key: String,
        models: String,
        uses: String,
    }

    let uses = uses_by_provider();
    let rows = providers
        .into_iter()
        .map(|provider| {
            let mut name = provider.name.clone();
            if provider.preset.is_empty() {
                name.push_str(" (custom)");
            }
            let id = if provider.hidden {
                format!("{} [hidden]", provider.id)
            } else {
                provider.id.clone()
            };
            let key = if provider.key.is_empty() {
                provider
                    .keys
                    .iter()
                    .find(|key| !key.off && !key.key.is_empty())
                    .map(|key| key.key.as_str())
            } else {
                Some(provider.key.as_str())
            };
            let key = if let Some(account) = provider.account.as_ref() {
                format!("● {}", account_label(account))
            } else {
                match key {
                    Some(key) => format!("● {}", mask(key)),
                    None if provider.is_local() => "● no key needed".to_owned(),
                    None => "○ no key".to_owned(),
                }
            };
            let exposed = crate::catalog::exposed_models(
                &provider.id,
                provider.catalog_id(),
                &provider.models,
            )
            .len();
            let live = crate::catalog::live_models(&provider.id);
            let models = if live.is_empty() {
                format!("{exposed} models")
            } else {
                format!(
                    "{exposed} of {} models",
                    crate::catalog::available_models(&provider.id, provider.catalog_id()).len()
                )
            };
            let mut usage = uses
                .get(&provider.id)
                .filter(|agents| !agents.is_empty())
                .map_or_else(String::new, |agents| format!("← {}", agents.join(", ")));
            if !provider.fallback.is_empty() {
                if !usage.is_empty() {
                    usage.push_str("  ");
                }
                usage.push_str("↳ ");
                usage.push_str(&provider.fallback.join(" → "));
            }

            Row {
                name,
                id,
                host: provider.host(),
                key,
                models,
                uses: usage,
            }
        })
        .collect::<Vec<_>>();
    let widths = [
        rows.iter()
            .map(|row| row.name.chars().count())
            .max()
            .unwrap_or_default(),
        rows.iter()
            .map(|row| row.id.chars().count())
            .max()
            .unwrap_or_default(),
        rows.iter()
            .map(|row| row.host.chars().count())
            .max()
            .unwrap_or_default(),
        rows.iter()
            .map(|row| row.key.chars().count())
            .max()
            .unwrap_or_default(),
        rows.iter()
            .map(|row| row.models.chars().count())
            .max()
            .unwrap_or_default(),
    ];
    for row in rows {
        println!(
            "  {}  {}  {}  {}  {}  {}",
            pad_cell(&row.name, widths[0]),
            pad_cell(&row.id, widths[1]),
            pad_cell(&row.host, widths[2]),
            pad_cell(&row.key, widths[3]),
            pad_cell(&row.models, widths[4]),
            row.uses
        );
    }
    Ok(())
}

fn uses_by_provider() -> HashMap<String, Vec<String>> {
    let mut uses = HashMap::<String, Vec<String>>::new();
    for agent in crate::agent::all()
        .into_iter()
        .filter(crate::agent::Agent::is_detected)
    {
        let Some(field) = agent.spec.fields.first() else {
            continue;
        };
        let Ok(values) = agent.values() else {
            continue;
        };
        let Some(value) = values
            .iter()
            .find(|(key, _)| *key == field.key)
            .map(|(_, value)| value)
        else {
            continue;
        };
        let value = value.strip_prefix("magpie/").unwrap_or(value);
        if value.starts_with("group/") {
            continue;
        }
        if let Some((provider, _)) = value.split_once('/') {
            uses.entry(provider.to_owned())
                .or_default()
                .push(agent.spec.name.to_owned());
        }
    }
    uses
}

fn pad_cell(value: &str, width: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(width.saturating_sub(value.chars().count()))
    )
}

pub fn presets() -> Result<()> {
    for (kind, label) in [
        (PresetKind::Vendor, "vendors"),
        (PresetKind::Relay, "relays"),
        (PresetKind::Local, "local"),
    ] {
        println!("{label}:");
        for preset in PRESETS.iter().filter(|preset| preset.kind == kind) {
            let endpoint = [preset.chat, preset.responses, preset.anthropic]
                .into_iter()
                .find(|url| !url.is_empty())
                .unwrap_or("no endpoint");
            println!("  {:16} {:20} {endpoint}", preset.id, preset.name);
        }
    }
    Ok(())
}

pub async fn models() -> Result<()> {
    if let Err(error) = crate::catalog::sync_if_stale().await {
        eprintln!("magpie: could not refresh models.dev; using cached catalog: {error:#}");
    }
    let providers = providers_with_local_accounts(load()?.providers);
    let mut found = false;
    for provider in providers
        .iter()
        .filter(|provider| !provider.hidden && has_provider_credential(provider))
    {
        let models =
            crate::catalog::exposed_models(&provider.id, provider.catalog_id(), &provider.models);
        if models.is_empty() {
            continue;
        }
        found = true;
        println!("{} ({})", provider.name, provider.id);
        for model in models {
            let name = if model.name == model.id {
                String::new()
            } else {
                format!("  {}", model.name)
            };
            let efforts = if model.efforts.is_empty() {
                String::new()
            } else {
                format!("  ({})", model.efforts.join("/"))
            };
            println!("  {}{name}{efforts}", model.id);
        }
    }
    let available = available_model_entries()?;
    for group in groups()?.into_iter().filter(|group| !group.hidden) {
        let members = group
            .members
            .iter()
            .filter(|member| available.iter().any(|entry| entry.id == **member))
            .count();
        if members == 0 {
            continue;
        }
        found = true;
        println!("{} (routing group)", group.name);
        println!("  group/{}  ({} ready members)", group.id, members);
    }
    if !found {
        if providers.is_empty() {
            println!("no providers yet · magpie provider add <preset> starts with a vendor");
        } else {
            println!("no models available yet · run magpie sync or magpie provider models <id>");
        }
    }
    Ok(())
}

pub(crate) async fn sync_live_models() -> Result<Vec<(String, usize)>> {
    let providers = providers_with_local_accounts(load()?.providers);
    let mut refreshed = Vec::new();

    for provider in providers.into_iter().filter(|provider| {
        !provider.hidden
            && has_provider_credential(provider)
            && (!provider.chat.is_empty()
                || !provider.responses.is_empty()
                || !provider.anthropic.is_empty())
    }) {
        let result =
            tokio::time::timeout(Duration::from_secs(8), refresh_models(&provider, false)).await;
        if let Ok(Ok(count)) = result {
            refreshed.push((provider.id, count));
        }
    }

    Ok(refreshed)
}

pub async fn command(args: &[String]) -> Result<()> {
    match args {
        [] => bail!("{USAGE}"),
        [verb] if verb == "presets" => presets(),
        [verb, rest @ ..] if verb == "add" => add(rest),
        [verb, id, key] if verb == "key" => change_key(id, key),
        [verb, rest @ ..] if verb == "keys" => keys_command(rest),
        [verb, id, selected @ ..] if verb == "routing" => set_routing(id, selected),
        [verb, id, selected @ ..] if verb == "affinity" || verb == "stays" => {
            set_affinity(id, selected)
        }
        [verb, id, fallback @ ..] if verb == "fallback" => set_fallback(id, fallback),
        [verb, id, rest @ ..] if verb == "models" => models_command(id, rest).await,
        [verb, id] if verb == "test" => test_provider_command(id).await,
        [verb, id, value] if verb == "icon" => set_icon(id, value),
        [verb, id] if verb == "rm" => remove(id),
        [id] => show(id).await,
        _ => bail!("{USAGE}"),
    }
}

fn set_icon(id: &str, value: &str) -> Result<()> {
    let mut file = load()?;
    let provider = file
        .providers
        .iter_mut()
        .find(|provider| provider.id == id || provider.name.eq_ignore_ascii_case(id))
        .with_context(|| format!("no provider {id:?}"))?;
    ensure!(
        provider.preset.is_empty(),
        "{} has its own icon; only a custom provider takes one",
        provider.name
    );
    let icon = if value.is_empty() {
        "generic".to_owned()
    } else {
        icon::from_value(value)?
    };
    provider.icon = if icon.is_empty() {
        "generic".to_owned()
    } else {
        icon
    };
    let provider_name = provider.name.clone();
    let icon_name = provider.icon.clone();
    store(file)?;
    println!("✓ {provider_name} icon {icon_name}");
    Ok(())
}

async fn models_command(id: &str, selected: &[String]) -> Result<()> {
    if !selected.is_empty() {
        let provider = find(id)?;
        ensure!(
            provider.account.is_none(),
            "signed-in account models follow the account's available model list and cannot be selected manually"
        );
        let mut file = load()?;
        let provider = file
            .providers
            .iter_mut()
            .find(|provider| provider.id == id || provider.name.eq_ignore_ascii_case(id))
            .with_context(|| format!("no provider {id:?}"))?;
        provider.models = if selected.len() == 1 && matches!(selected[0].as_str(), "-" | "all") {
            Vec::new()
        } else {
            clean_list(&selected.join(","))
        };
        let name = provider.name.clone();
        let provider_id = provider.id.clone();
        store(file)?;
        println!("✓ updated {name} ({provider_id}) model selection");
        return show(&provider_id).await;
    }

    let provider = find(id)?;
    let count = refresh_models(&provider, true).await?;
    println!("✓ fetched {count} models from {}", provider.host());
    show(&provider.id).await
}

async fn test_provider_command(id: &str) -> Result<()> {
    let provider = find(id)?;
    if provider.account.is_some() {
        let count = refresh_models(&provider, true).await?;
        println!("✓ connected to {} · fetched {count} models", provider.name);
        return Ok(());
    }
    test::test_provider(id).await
}

async fn refresh_models(provider: &Provider, report_failures: bool) -> Result<usize> {
    match provider.account.as_ref() {
        Some(ProviderAccount::Codex { auth_file }) => {
            return crate::codex::refresh_models(auth_file).await;
        }
        Some(ProviderAccount::Copilot { account }) => {
            return crate::copilot::refresh_models(account).await;
        }
        None => {}
    }
    let mut keys = Vec::new();
    if !provider.key.is_empty() {
        keys.push((provider.key.clone(), provider.key_protocol.clone()));
    }
    keys.extend(
        provider
            .keys
            .iter()
            .filter(|key| !key.key.is_empty())
            .map(|key| (key.key.clone(), key.protocol.clone())),
    );
    if keys.is_empty() {
        keys.push((String::new(), String::new()));
    }

    let old_models = crate::catalog::live_models(&provider.id);
    let provider_ref = provider;
    let provider_headers = &provider.headers;
    let fetched = futures_util::future::join_all(keys.iter().map(|(key, protocol)| async move {
        let endpoints = model_endpoints(provider_ref, protocol)?;
        crate::catalog::fetch_models(&endpoints, key, provider_headers).await
    }))
    .await;

    let mut base = String::new();
    let mut models: Vec<crate::catalog::Model> = Vec::new();
    let mut model_indices: HashMap<String, usize> = HashMap::new();
    let track_key_access = keys.len() > 1;
    let mut merge_model = |mut model: crate::catalog::Model, key: Option<String>| {
        if let Some(index) = model_indices.get(&model.id).copied() {
            let existing: &mut crate::catalog::Model = &mut models[index];
            existing.images &= model.images;
            existing.image_input = match (existing.image_input, model.image_input) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            if let Some(key) = key
                && !existing.keys.contains(&key)
            {
                existing.keys.push(key);
            }
        } else {
            model.keys = key.into_iter().collect();
            model_indices.insert(model.id.clone(), models.len());
            models.push(model);
        }
    };

    let mut last_error = None;
    for ((key, _), result) in keys.iter().zip(fetched) {
        let fingerprint = (track_key_access && !key.is_empty()).then(|| key_id(key));
        match result {
            Ok((found_base, fetched_models)) => {
                if base.is_empty() {
                    base = found_base;
                }
                for model in fetched_models {
                    merge_model(model, fingerprint.clone());
                }
            }
            Err(error) => {
                if report_failures {
                    eprintln!(
                        "magpie: model list fetch failed for key {}: {error:#}",
                        fingerprint.as_deref().unwrap_or("primary")
                    );
                }
                last_error = Some(error);
                if let Some(fingerprint) = fingerprint {
                    for model in old_models
                        .iter()
                        .filter(|model| model.keys.contains(&fingerprint))
                    {
                        merge_model(model.clone(), Some(fingerprint.clone()));
                    }
                }
            }
        }
    }
    if base.is_empty() {
        if let Some(error) = last_error {
            return Err(error);
        }
        bail!("provider has no endpoint to ask for models");
    }
    let count = models.len();
    crate::catalog::save_live(&provider.id, &base, models)?;
    Ok(count)
}

fn model_endpoints<'a>(provider: &'a Provider, protocol: &str) -> Result<Vec<(&'a str, bool)>> {
    let endpoints = match protocol {
        "" => vec![
            (provider.chat.as_str(), false),
            (provider.responses.as_str(), false),
            (provider.anthropic.as_str(), true),
        ],
        "chat" => vec![(provider.chat.as_str(), false)],
        "responses" => vec![(provider.responses.as_str(), false)],
        "anthropic" => vec![(provider.anthropic.as_str(), true)],
        _ => bail!("unknown key protocol {protocol:?}"),
    };
    Ok(endpoints)
}

fn prepare_provider(mut provider: Provider) -> Result<Provider> {
    provider.name = provider.name.trim().to_owned();
    ensure!(!provider.name.is_empty(), "provider name is empty");
    if provider.id.trim().is_empty() {
        provider.id = slug(&provider.name);
    } else {
        let id = provider.id.trim().to_ascii_lowercase();
        ensure!(
            id == slug(&id),
            "provider id must use lowercase letters, digits, and dashes"
        );
        provider.id = id;
    }
    ensure!(!provider.id.is_empty(), "provider id is empty");
    ensure!(
        provider.id != "magpie" && provider.id != "group",
        "provider id is reserved"
    );
    provider.chat = normalize_url(&provider.chat)?;
    provider.responses = normalize_url(&provider.responses)?;
    provider.anthropic = normalize_url(&provider.anthropic)?;
    provider.website = normalize_url(&provider.website)?;
    provider.keys_url = normalize_url(&provider.keys_url)?;
    ensure!(
        !provider.chat.is_empty()
            || !provider.responses.is_empty()
            || !provider.anthropic.is_empty(),
        "provider needs a url, chat, responses, or anthropic base URL"
    );
    ensure!(
        !provider.key.is_empty() || provider.is_local(),
        "provider needs an API key unless it runs locally"
    );
    Ok(provider)
}

fn add(args: &[String]) -> Result<()> {
    let [name, assignments @ ..] = args else {
        bail!("{USAGE}");
    };
    let preset = find_preset(name);
    let mut provider = preset.map_or_else(
        || Provider {
            name: name.trim().to_owned(),
            ..Provider::default()
        },
        Preset::provider,
    );
    let mut assignments = assignments;
    if preset.is_some()
        && let [key] = assignments
        && !is_provider_assignment(key)
    {
        provider.key = key.trim().to_owned();
        assignments = &[];
    }

    for assignment in assignments {
        let (key, value) = assignment
            .split_once('=')
            .with_context(|| format!("expected key=value, got {assignment:?}"))?;
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "id" => provider.id = value.to_owned(),
            "name" => provider.name = value.to_owned(),
            "url" | "chat" => provider.chat = value.to_owned(),
            "responses" => provider.responses = value.to_owned(),
            "anthropic" => provider.anthropic = value.to_owned(),
            "key" => provider.key = value.to_owned(),
            "icon" => provider.icon = icon::from_value(value)?,
            "catalog" => provider.catalog = value.to_owned(),
            "website" => provider.website = value.to_owned(),
            "keysurl" => provider.keys_url = value.to_owned(),
            "balance" => provider.balance_url = value.to_owned(),
            "balance.path" => provider.balance_path = value.to_owned(),
            "models" => provider.models = clean_list(value),
            "fallback" => provider.fallback = clean_list(value),
            "routing" => provider.routing = normalize_routing(value)?,
            "affinity" | "stays" => provider.affinity = normalize_affinity(value)?,
            header if header.starts_with("header.") && header.len() > "header.".len() => {
                let (_, name) = key.split_once('.').context("invalid header assignment")?;
                let name = name.trim();
                ensure!(!name.is_empty(), "header name is empty");
                provider.headers.insert(name.to_owned(), value.to_owned());
            }
            _ => bail!("unsupported provider field {key:?}"),
        }
    }

    let provider = store_new_provider(provider)?;
    let key = if provider.key.is_empty() {
        "no key".to_owned()
    } else {
        mask(&provider.key)
    };
    println!("✓ added {} ({}) · {key}", provider.name, provider.id);
    Ok(())
}

fn store_new_provider(mut provider: Provider) -> Result<Provider> {
    provider = prepare_provider(provider)?;
    let mut file = load()?;
    if let Some(existing) = file.providers.iter().find(|existing| {
        existing.chat == provider.chat
            && existing.responses == provider.responses
            && existing.anthropic == provider.anthropic
            && existing.key == provider.key
            && existing.headers == provider.headers
    }) {
        bail!("{} is already added as {}", existing.name, existing.id);
    }
    let base_id = provider.id.clone();
    let base_name = provider.name.clone();
    let mut suffix = 2;
    while file.providers.iter().any(|existing| {
        existing.id == provider.id || existing.name.eq_ignore_ascii_case(&provider.name)
    }) {
        provider.id = format!("{base_id}-{suffix}");
        provider.name = format!("{base_name} {suffix}");
        suffix += 1;
    }
    file.providers.push(provider.clone());
    store(file)?;
    Ok(provider)
}

async fn show(id: &str) -> Result<()> {
    let provider = find(id)?;
    println!("{} ({})", provider.name, provider.id);
    println!("  host: {}", provider.host());
    println!(
        "  protocols: {}",
        [
            ("chat", &provider.chat),
            ("responses", &provider.responses),
            ("anthropic", &provider.anthropic),
        ]
        .into_iter()
        .filter_map(|(protocol, url)| (!url.is_empty()).then_some(protocol))
        .collect::<Vec<_>>()
        .join(", ")
    );
    println!(
        "  key: {}",
        if let Some(account) = provider.account.as_ref() {
            account_label(account)
        } else if provider.key.is_empty() {
            "no key".to_owned()
        } else {
            mask(&provider.key)
        }
    );
    let active_keys = usize::from(!provider.key.is_empty())
        + provider
            .keys
            .iter()
            .filter(|key| !key.key.is_empty() && !key.off)
            .count();
    if active_keys > 1 || !provider.keys.is_empty() {
        println!(
            "  keys: {active_keys} on · magpie provider keys {}",
            provider.id
        );
    }
    if let Some(balance) = balance::fetch(&provider).await {
        let mut source = String::new();
        if !provider.balance_url.is_empty() {
            source.push_str(" · from ");
            source.push_str(&provider.balance_url);
            if !provider.balance_path.is_empty() {
                source.push(' ');
                source.push_str(&provider.balance_path);
            }
        }
        match balance {
            Ok(amount) => println!("  balance: {amount}{source}"),
            Err(error) => println!("  balance: unavailable · {error:#}{source}"),
        }
    }
    if !provider.routing.is_empty() {
        println!("  key routing: {}", provider.routing);
    }
    println!("  stays: {}", affinity_name(&provider.affinity));
    if !provider.models.is_empty() {
        println!("  models: {}", provider.models.join(", "));
    } else {
        let available = crate::catalog::available_models(&provider.id, provider.catalog_id());
        if available.is_empty() {
            println!(
                "  models: not fetched · magpie provider models {}",
                provider.id
            );
        } else {
            let exposed = crate::catalog::exposed_models(
                &provider.id,
                provider.catalog_id(),
                &provider.models,
            );
            println!(
                "  models: {} exposed of {} available",
                exposed.len(),
                available.len()
            );
        }
    }
    if !provider.fallback.is_empty() {
        println!("  fallback: {}", provider.fallback.join(" → "));
    }
    Ok(())
}

fn set_fallback(id: &str, selected: &[String]) -> Result<()> {
    let mut file = load()?;
    let index = file
        .providers
        .iter()
        .position(|provider| provider.id == id || provider.name.eq_ignore_ascii_case(id))
        .with_context(|| format!("no provider {id:?}"))?;
    if selected.is_empty() {
        let provider = &file.providers[index];
        if provider.fallback.is_empty() {
            println!(
                "{} has no fallback · magpie provider fallback {} <provider/model>…",
                provider.name, provider.id
            );
        } else {
            println!(
                "{} falls back to {} when its API is unavailable",
                provider.name,
                provider.fallback.join(" → ")
            );
        }
        return Ok(());
    }

    let fallbacks = normalize_fallbacks(selected, &file.providers)?;

    let provider = &mut file.providers[index];
    provider.fallback = fallbacks;
    let name = provider.name.clone();
    let summary = if provider.fallback.is_empty() {
        "cleared fallbacks".to_owned()
    } else {
        format!("falls back to {}", provider.fallback.join(" → "))
    };
    store(file)?;
    println!("✓ {name} {summary}");
    Ok(())
}

fn normalize_fallbacks(selected: &[String], providers: &[Provider]) -> Result<Vec<String>> {
    if selected.len() == 1 && selected[0] == "none" {
        return Ok(Vec::new());
    }
    ensure!(
        selected.iter().all(|target| target != "none"),
        "use `none` by itself to clear fallbacks"
    );
    let mut fallbacks = Vec::new();
    for target in selected {
        let (provider_id, model) = target
            .split_once('/')
            .with_context(|| format!("fallback {target:?} must be provider/model"))?;
        ensure!(!model.is_empty(), "fallback model is empty in {target:?}");
        let provider = providers
            .iter()
            .find(|provider| {
                !provider.hidden
                    && (provider.id == provider_id
                        || provider.name.eq_ignore_ascii_case(provider_id))
            })
            .with_context(|| format!("fallback provider {provider_id:?} was not found"))?;
        ensure!(
            !provider.chat.is_empty()
                || !provider.responses.is_empty()
                || !provider.anthropic.is_empty(),
            "fallback provider {provider_id:?} has no API endpoint"
        );
        let canonical = format!("{}/{model}", provider.id);
        if !fallbacks.contains(&canonical) {
            fallbacks.push(canonical);
        }
    }
    Ok(fallbacks)
}

fn change_key(id: &str, key: &str) -> Result<()> {
    ensure!(!key.trim().is_empty(), "provider key is empty");
    let mut file = load()?;
    let provider = file
        .providers
        .iter_mut()
        .find(|provider| provider.id == id || provider.name.eq_ignore_ascii_case(id))
        .with_context(|| format!("no provider {id:?}"))?;
    provider.key = key.trim().to_owned();
    let name = provider.name.clone();
    let masked = mask(&provider.key);
    store(file)?;
    println!("✓ {name} key {masked}");
    Ok(())
}

fn keys_command(args: &[String]) -> Result<()> {
    let [id, action @ ..] = args else {
        bail!("{USAGE}");
    };
    if action.is_empty() {
        return list_keys(id);
    }

    match action {
        [verb, key, options @ ..] if verb == "add" => add_key(id, key, options),
        [verb, key_id] if matches!(verb.as_str(), "use" | "on" | "off" | "rm") => {
            update_key(id, verb, key_id)
        }
        [verb, key_id, value] if verb == "rename" => rename_key(id, key_id, value),
        [verb, key_id, protocol] if verb == "protocol" => set_key_protocol(id, key_id, protocol),
        _ => bail!("{USAGE}"),
    }
}

fn list_keys(id: &str) -> Result<()> {
    let provider = find(id)?;
    println!("{} ({}) API keys", provider.name, provider.id);
    let mut count = 0;
    if !provider.key.is_empty() {
        count += 1;
        print_key(
            &provider.key,
            &provider.key_name,
            true,
            true,
            &provider.key_protocol,
        );
    }
    for key in &provider.keys {
        if key.key.is_empty() {
            continue;
        }
        count += 1;
        print_key(&key.key, &key.name, !key.off, false, &key.protocol);
    }
    if count == 0 {
        println!("  no keys · magpie provider key {} <key>", provider.id);
    }
    Ok(())
}

fn print_key(key: &str, name: &str, on: bool, primary: bool, protocol: &str) {
    let label = if name.is_empty() { "unnamed" } else { name };
    let status = if on { "on" } else { "off" };
    let primary = if primary { " · primary" } else { "" };
    let protocol = if protocol.is_empty() { "any" } else { protocol };
    println!(
        "  {}  {label} · {status}{primary} · {protocol} · {}",
        key_id(key),
        mask(key)
    );
}

fn add_key(id: &str, key: &str, options: &[String]) -> Result<()> {
    let key = key.trim();
    ensure!(!key.is_empty(), "provider key is empty");
    let mut name = String::new();
    let mut protocol = String::new();
    for option in options {
        let (field, value) = option.split_once('=').with_context(|| {
            format!("expected name=<name> or protocol=<protocol>, got {option:?}")
        })?;
        match field.to_ascii_lowercase().as_str() {
            "name" => name = value.trim().to_owned(),
            "protocol" => protocol = parse_key_protocol(value)?,
            _ => bail!("unknown key option {field:?}; use name= or protocol="),
        }
    }

    let mut file = load()?;
    let provider = find_provider_mut(&mut file, id)?;
    let provider_name = provider.name.clone();
    let provider_id = provider.id.clone();
    let fingerprint = add_key_to_provider(provider, key, name, protocol)?;
    store(file)?;
    println!("✓ added API key to {provider_name} ({provider_id}) · {fingerprint}");
    Ok(())
}

fn add_key_to_provider(
    provider: &mut Provider,
    key: &str,
    name: String,
    protocol: String,
) -> Result<String> {
    let key = key.trim();
    ensure!(!key.is_empty(), "provider key is empty");
    let fingerprint = key_id(key);
    ensure!(
        provider.key.is_empty() || fingerprint != key_id(&provider.key),
        "key is already configured"
    );
    ensure!(
        !provider
            .keys
            .iter()
            .any(|saved| key_id(&saved.key) == fingerprint),
        "key is already configured"
    );

    if provider.key.is_empty() {
        provider.key = key.to_owned();
        provider.key_name = name;
        provider.key_protocol = protocol;
    } else {
        provider.keys.push(KeyAccount {
            name,
            key: key.to_owned(),
            off: false,
            protocol,
            ..KeyAccount::default()
        });
    }
    Ok(fingerprint)
}

fn update_key(id: &str, action: &str, reference: &str) -> Result<()> {
    update_key_for(id, action, reference, true)
}

fn update_key_for(id: &str, action: &str, reference: &str, announce: bool) -> Result<()> {
    let mut file = load()?;
    let provider = find_provider_mut(&mut file, id)?;
    let provider_name = provider.name.clone();
    apply_key_action(provider, action, reference)?;
    store(file)?;
    if announce {
        println!("✓ {provider_name} key {reference} {action}");
    }
    Ok(())
}

fn apply_key_action(provider: &mut Provider, action: &str, reference: &str) -> Result<()> {
    let index = locate_key(provider, reference)
        .with_context(|| format!("{} has no key {reference:?}", provider.name))?;

    match action {
        "use" if index > 0 => {
            let selected = provider.keys.remove(index - 1);
            let mut selected = selected;
            selected.off = false;
            let previous = KeyAccount {
                name: std::mem::take(&mut provider.key_name),
                key: std::mem::replace(&mut provider.key, selected.key),
                off: false,
                protocol: std::mem::take(&mut provider.key_protocol),
                ..KeyAccount::default()
            };
            provider.key_name = selected.name;
            provider.key_protocol = selected.protocol;
            if !previous.key.is_empty() {
                provider.keys.insert(0, previous);
            }
        }
        "use" | "on" if index == 0 => {}
        "on" => provider.keys[index - 1].off = false,
        "off" if index > 0 => provider.keys[index - 1].off = true,
        "off" => {
            let Some(next) = provider
                .keys
                .iter()
                .position(|key| !key.off && !key.key.is_empty())
            else {
                bail!("that's the only key in use; turn another on first");
            };
            let previous = KeyAccount {
                name: std::mem::take(&mut provider.key_name),
                key: std::mem::take(&mut provider.key),
                off: true,
                protocol: std::mem::take(&mut provider.key_protocol),
                ..KeyAccount::default()
            };
            let mut promoted = provider.keys.remove(next);
            promoted.off = false;
            provider.key = promoted.key;
            provider.key_name = promoted.name;
            provider.key_protocol = promoted.protocol;
            provider.keys.insert(0, previous);
        }
        "rm" if index > 0 => {
            provider.keys.remove(index - 1);
        }
        "rm" => {
            let Some(next) = provider
                .keys
                .iter()
                .position(|key| !key.off && !key.key.is_empty())
            else {
                bail!("that's the only key in use; turn another on before removing it");
            };
            let promoted = provider.keys.remove(next);
            provider.key = promoted.key;
            provider.key_name = promoted.name;
            provider.key_protocol = promoted.protocol;
        }
        _ => bail!("unsupported key action {action:?}"),
    }
    Ok(())
}

fn rename_key(id: &str, reference: &str, name: &str) -> Result<()> {
    let mut file = load()?;
    let provider = find_provider_mut(&mut file, id)?;
    let index = locate_key(provider, reference)
        .with_context(|| format!("{} has no key {reference:?}", provider.name))?;
    let name = name.trim().to_owned();
    if index == 0 {
        provider.key_name = name;
    } else {
        provider.keys[index - 1].name = name;
    }
    let provider_name = provider.name.clone();
    store(file)?;
    println!("✓ renamed {provider_name} key {reference}");
    Ok(())
}

fn set_key_protocol(id: &str, reference: &str, protocol: &str) -> Result<()> {
    let protocol = parse_key_protocol(protocol)?;
    let mut file = load()?;
    let provider = find_provider_mut(&mut file, id)?;
    let index = locate_key(provider, reference)
        .with_context(|| format!("{} has no key {reference:?}", provider.name))?;
    if index == 0 {
        provider.key_protocol = protocol;
    } else {
        provider.keys[index - 1].protocol = protocol;
    }
    let provider_name = provider.name.clone();
    store(file)?;
    println!("✓ changed {provider_name} key {reference} protocol");
    Ok(())
}

fn parse_key_protocol(protocol: &str) -> Result<String> {
    let protocol = protocol.trim().to_ascii_lowercase();
    match protocol.as_str() {
        "" | "any" | "*" => Ok(String::new()),
        "chat" | "responses" | "anthropic" => Ok(protocol),
        _ => bail!("protocol must be any, chat, responses or anthropic"),
    }
}

fn locate_key(provider: &Provider, reference: &str) -> Option<usize> {
    if !provider.key.is_empty() && key_id(&provider.key) == reference {
        return Some(0);
    }
    provider
        .keys
        .iter()
        .position(|key| key_id(&key.key) == reference)
        .map(|index| index + 1)
}

fn find_provider_mut<'a>(file: &'a mut ProviderFile, id: &str) -> Result<&'a mut Provider> {
    file.providers
        .iter_mut()
        .find(|provider| provider.id == id || provider.name.eq_ignore_ascii_case(id))
        .with_context(|| format!("no provider {id:?}"))
}

fn set_routing(id: &str, selected: &[String]) -> Result<()> {
    let mut file = load()?;
    let provider = find_provider_mut(&mut file, id)?;
    if selected.is_empty() {
        println!(
            "{} key routing: {}",
            provider.name,
            if provider.routing.is_empty() {
                "smart"
            } else {
                &provider.routing
            }
        );
        return Ok(());
    }
    ensure!(selected.len() == 1, "choose one key routing strategy");
    provider.routing = normalize_routing(&selected[0])?;
    let provider_name = provider.name.clone();
    let routing = if provider.routing.is_empty() {
        "smart"
    } else {
        &provider.routing
    };
    let message = format!("✓ {provider_name} key routing: {routing}");
    store(file)?;
    println!("{message}");
    Ok(())
}

fn set_affinity(id: &str, selected: &[String]) -> Result<()> {
    let mut file = load()?;
    let provider = find_provider_mut(&mut file, id)?;
    if selected.is_empty() {
        println!(
            "{} stays {} · magpie provider affinity {} <auto|session|turn|off>",
            provider.name,
            affinity_name(&provider.affinity),
            provider.id
        );
        return Ok(());
    }
    ensure!(selected.len() == 1, "choose one conversation affinity mode");
    provider.affinity = normalize_affinity(&selected[0])?;
    let provider_name = provider.name.clone();
    let provider_id = provider.id.clone();
    let mode = affinity_name(&provider.affinity).to_owned();
    store(file)?;
    println!("✓ {provider_name} stays {mode} · magpie provider affinity {provider_id}");
    Ok(())
}

fn normalize_routing(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "smart" | "default" => Ok(String::new()),
        "order" => Ok("order".to_owned()),
        "rotate" | "round-robin" => Ok("rotate".to_owned()),
        "usage" | "least-used" => Ok("usage".to_owned()),
        value => bail!("unknown key routing {value:?}; use smart, order, rotate or usage"),
    }
}

fn normalize_affinity(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" | "automatic" => Ok(String::new()),
        "session" | "always" => Ok("session".to_owned()),
        "turn" | "within-a-turn" => Ok("turn".to_owned()),
        "off" | "never" => Ok("off".to_owned()),
        value => bail!("unknown conversation affinity {value:?}; use auto, session, turn or off"),
    }
}

fn affinity_name(value: &str) -> &str {
    if value.is_empty() { "auto" } else { value }
}

fn remove(id: &str) -> Result<()> {
    let provider = remove_provider_data(id)?;
    println!("✓ removed {} ({})", provider.name, provider.id);
    Ok(())
}

fn remove_provider_data(id: &str) -> Result<Provider> {
    let mut file = load()?;
    let Some(index) = file
        .providers
        .iter()
        .position(|provider| provider.id == id || provider.name.eq_ignore_ascii_case(id))
    else {
        bail!("no provider {id:?}");
    };
    let provider = file.providers.remove(index);
    store(file)?;
    Ok(provider)
}

fn find(id: &str) -> Result<Provider> {
    providers_with_local_accounts(load()?.providers)
        .into_iter()
        .find(|provider| provider.id == id || provider.name.eq_ignore_ascii_case(id))
        .with_context(|| format!("no provider {id:?}; magpie providers lists them"))
}

fn load() -> Result<ProviderFile> {
    let path = settings::providers_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProviderFile::default());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn store(file: ProviderFile) -> Result<()> {
    let path = settings::providers_path();
    let parent = path
        .parent()
        .context("provider path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&file).context("serialize providers")?;
    bytes.push(b'\n');
    crate::config::atomic_write_secret_for_settings(&path, &bytes)
        .with_context(|| format!("write {}", path.display()))
}

const MAX_ICON_BYTES: usize = 1 << 20;

pub(crate) struct BackupSnapshot {
    pub(crate) providers: Vec<Provider>,
    pub(crate) icons: BTreeMap<String, Vec<u8>>,
}

pub(crate) fn backup_snapshot(include_keys: bool) -> Result<BackupSnapshot> {
    let mut providers = Vec::new();
    let mut icons = BTreeMap::new();
    for mut provider in load()?.providers {
        if let Some(name) = provider.icon.strip_prefix("file:")
            && let Some(path) = backup_icon_path(name)
            && let Ok(data) = fs::read(path)
            && data.len() <= MAX_ICON_BYTES
        {
            icons.insert(name.to_owned(), data);
        }
        if !include_keys {
            provider.key.clear();
            provider.key_name.clear();
            provider.keys.clear();
            provider.key_protocol.clear();
            provider.headers.retain(|name, _| !looks_secret(name));
            provider.extra.retain(|name, _| !looks_secret(name));
            for value in provider.extra.values_mut() {
                redact_secret_properties(value);
            }
        }
        providers.push(provider);
    }
    Ok(BackupSnapshot { providers, icons })
}

pub(crate) fn restore_backup_icons(icons: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    for (name, data) in icons {
        if data.len() > MAX_ICON_BYTES {
            continue;
        }
        let Some(path) = backup_icon_path(name) else {
            continue;
        };
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create provider icon directory {}", parent.display()))?;
        }
        crate::config::atomic_write_for_settings(&path, data)
            .with_context(|| format!("restore provider icon {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn restore_backup_providers(
    incoming: &[Provider],
) -> Result<(usize, usize, Vec<String>)> {
    let mut file = load()?;
    let mut included = HashSet::new();
    let mut added = 0;
    let mut replaced = 0;
    for mut provider in incoming.iter().cloned() {
        if provider.id.is_empty() || provider.id != slug(&provider.id) || provider.id == "magpie" {
            continue;
        }
        included.insert(provider.id.clone());
        if let Some(index) = file
            .providers
            .iter()
            .position(|existing| existing.id == provider.id)
        {
            if provider.key.is_empty() && provider.keys.is_empty() {
                let existing = &file.providers[index];
                provider.key.clone_from(&existing.key);
                provider.key_name.clone_from(&existing.key_name);
                provider.keys.clone_from(&existing.keys);
                provider.key_protocol.clone_from(&existing.key_protocol);
            }
            file.providers[index] = provider;
            replaced += 1;
        } else {
            file.providers.push(provider);
            added += 1;
        }
    }
    let need_key = file
        .providers
        .iter()
        .filter(|provider| included.contains(&provider.id))
        .filter(|provider| provider.key.is_empty() && !provider.is_local())
        .map(|provider| provider.name.clone())
        .collect();
    if added + replaced > 0 {
        store(file)?;
    }
    Ok((added, replaced, need_key))
}

fn backup_icon_path(name: &str) -> Option<PathBuf> {
    let (digest, extension) = name.split_once('.')?;
    let valid_digest = digest.len() == 16
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid_digest || !matches!(extension, "png" | "jpg" | "gif" | "webp" | "ico" | "svg") {
        return None;
    }
    let directory = settings::providers_path().parent()?.to_owned();
    Some(directory.join("icons").join(name))
}

fn looks_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "auth", "key", "token", "secret", "cookie", "session", "password",
    ]
    .iter()
    .any(|part| name.contains(part))
}

fn redact_secret_properties(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|name, _| !looks_secret(name));
            for value in fields.values_mut() {
                redact_secret_properties(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                redact_secret_properties(value);
            }
        }
        _ => {}
    }
}

fn find_preset(query: &str) -> Option<&'static Preset> {
    let query = query.trim();
    PRESETS.iter().find(|preset| {
        preset.id.eq_ignore_ascii_case(query) || preset.name.eq_ignore_ascii_case(query)
    })
}

fn is_provider_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    let normalized = name.to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "id" | "name"
            | "url"
            | "chat"
            | "responses"
            | "anthropic"
            | "key"
            | "icon"
            | "catalog"
            | "website"
            | "keysurl"
            | "balance"
            | "balance.path"
            | "models"
            | "fallback"
            | "routing"
            | "affinity"
            | "stays"
    ) || normalized.starts_with("header.")
}

fn normalize_url(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let mut url =
        Url::from_str(&value).with_context(|| format!("invalid provider URL {value:?}"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "provider URLs must use http:// or https://"
    );
    url.set_fragment(None);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn clean_list(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let item = item.to_owned();
        if !values.contains(&item) {
            values.push(item);
        }
    }
    values
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

fn mask(secret: &str) -> String {
    let chars = secret.chars().collect::<Vec<_>>();
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    format!(
        "{}…{}",
        chars[..4].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Provider {
    fn catalog_id(&self) -> &str {
        if self.catalog.is_empty() {
            &self.id
        } else {
            &self.catalog
        }
    }

    fn host(&self) -> String {
        [&self.chat, &self.responses, &self.anthropic]
            .into_iter()
            .find_map(|url| Url::parse(url).ok()?.host_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn is_local(&self) -> bool {
        [&self.chat, &self.responses, &self.anthropic]
            .into_iter()
            .filter_map(|url| Url::parse(url).ok())
            .any(|url| {
                url.host_str().is_some_and(|host| {
                    matches!(
                        host,
                        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "[::1]"
                    )
                })
            })
    }
}

fn model_entries(providers: &[Provider]) -> Vec<ModelEntry> {
    let mut entries = Vec::new();
    for provider in providers.iter().filter(|provider| {
        !provider.hidden
            && has_provider_credential(provider)
            && (!provider.chat.is_empty()
                || !provider.responses.is_empty()
                || !provider.anthropic.is_empty())
    }) {
        for model in
            crate::catalog::exposed_models(&provider.id, provider.catalog_id(), &provider.models)
        {
            entries.push(ModelEntry {
                id: format!("{}/{}", provider.id, model.id),
                model,
                provider_id: provider.id.clone(),
                provider_name: provider.name.clone(),
                icon: provider.icon.clone(),
            });
        }
    }
    entries
}

pub(crate) fn catalog_id_for_usage(provider_id: &str) -> Option<String> {
    providers_with_local_accounts(load().ok()?.providers)
        .into_iter()
        .find(|provider| provider.id == provider_id)
        .map(|provider| provider.catalog_id().to_owned())
}

fn has_ready_member(member: &str, providers: &[Provider]) -> bool {
    let Some((provider_ref, model)) = member.split_once('/') else {
        return false;
    };
    !model.is_empty()
        && providers.iter().any(|provider| {
            !provider.hidden
                && (provider.id == provider_ref || provider.name.eq_ignore_ascii_case(provider_ref))
                && has_provider_credential(provider)
                && (!provider.chat.is_empty()
                    || !provider.responses.is_empty()
                    || !provider.anthropic.is_empty())
        })
}

fn groups_in(saved: &[Group], entries: &[ModelEntry]) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut visible_ids = HashSet::new();
    let mut hidden_ids = HashSet::new();
    for group in saved {
        if group.hidden {
            hidden_ids.insert(group.id.clone());
        } else {
            visible_ids.insert(group.id.clone());
            groups.push(group.clone());
        }
    }
    for mut group in auto_groups(entries) {
        if visible_ids.contains(&group.id) {
            continue;
        }
        group.hidden = hidden_ids.contains(&group.id);
        groups.push(group);
    }
    groups
}

fn auto_groups(entries: &[ModelEntry]) -> Vec<Group> {
    let mut positions = HashMap::new();
    let mut groups = Vec::<Group>::new();
    let mut providers = Vec::<HashSet<String>>::new();
    let mut first_model_ids = Vec::<String>::new();
    for entry in entries {
        let key = same_model(&entry.model.id);
        let position = *positions.entry(key.clone()).or_insert_with(|| {
            groups.push(Group {
                id: format!("auto-{}", slug(&key)),
                name: if entry.model.name.is_empty() {
                    entry.model.id.clone()
                } else {
                    entry.model.name.clone()
                },
                auto: true,
                ..Group::default()
            });
            providers.push(HashSet::new());
            first_model_ids.push(entry.model.id.clone());
            groups.len() - 1
        });
        let group = &mut groups[position];
        if providers[position].insert(entry.provider_id.clone()) {
            group.members.push(entry.id.clone());
            if group.name == first_model_ids[position]
                && !entry.model.name.is_empty()
                && entry.model.name != entry.model.id
            {
                group.name.clone_from(&entry.model.name);
            }
        }
    }
    groups
        .into_iter()
        .filter(|group| !group.id.ends_with('-') && group.members.len() > 1)
        .collect()
}

fn same_model(id: &str) -> String {
    let mut bytes = id
        .rsplit('/')
        .next()
        .unwrap_or(id)
        .to_ascii_lowercase()
        .into_bytes();
    for index in 1..bytes.len().saturating_sub(1) {
        if bytes[index] == b'.'
            && bytes[index - 1].is_ascii_digit()
            && bytes[index + 1].is_ascii_digit()
        {
            bytes[index] = b'-';
        }
    }
    let mut key = String::from_utf8(bytes).expect("ASCII substitutions preserve UTF-8");
    let base_len = key.rsplit_once('-').and_then(|(base, snapshot)| {
        (snapshot.len() == 8
            && snapshot.starts_with("20")
            && snapshot.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(base.len())
    });
    if let Some(base_len) = base_len {
        key.truncate(base_len);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, name: &str) -> Provider {
        Provider {
            id: id.to_owned(),
            name: name.to_owned(),
            chat: "https://api.example/v1".to_owned(),
            ..Provider::default()
        }
    }

    fn model_entry(provider_id: &str, model_id: &str, model_name: &str) -> ModelEntry {
        ModelEntry {
            id: format!("{provider_id}/{model_id}"),
            model: crate::catalog::Model {
                id: model_id.to_owned(),
                name: model_name.to_owned(),
                ..crate::catalog::Model::default()
            },
            provider_id: provider_id.to_owned(),
            provider_name: provider_id.to_owned(),
            icon: String::new(),
        }
    }

    #[test]
    fn presets_build_providers_with_protocol_specific_endpoints() {
        let provider = find_preset("OpenAI")
            .expect("OpenAI should be a built-in preset")
            .provider();
        assert_eq!(provider.id, "openai");
        assert_eq!(provider.chat, "https://api.openai.com/v1");
        assert_eq!(provider.responses, provider.chat);
        assert!(provider.key.is_empty());
    }

    #[test]
    fn local_presets_are_keyless_and_recognized_as_local() {
        let provider = find_preset("ollama")
            .expect("Ollama should be a built-in preset")
            .provider();
        assert!(provider.key.is_empty());
        assert!(provider.is_local());
    }

    #[test]
    fn raw_preset_keys_can_contain_equals_signs() {
        assert!(!is_provider_assignment("sk-example=="));
        assert!(is_provider_assignment("key=sk-example"));
        assert!(is_provider_assignment("header.Authorization=Token"));
    }

    #[test]
    fn fallback_models_are_canonicalized_and_deduplicated() {
        let providers = [provider("backup", "Backup")];
        let selected = vec!["Backup/model-v2".to_owned(), "backup/model-v2".to_owned()];

        assert_eq!(
            normalize_fallbacks(&selected, &providers).expect("valid fallback models"),
            vec!["backup/model-v2".to_owned()]
        );
        assert!(
            normalize_fallbacks(&["none".to_owned()], &providers)
                .expect("none clears fallbacks")
                .is_empty()
        );
    }

    #[test]
    fn fallback_models_require_a_known_provider_and_nonempty_model() {
        let providers = [provider("backup", "Backup")];

        assert!(normalize_fallbacks(&["missing/model".to_owned()], &providers).is_err());
        assert!(normalize_fallbacks(&["backup/".to_owned()], &providers).is_err());
        assert!(
            normalize_fallbacks(&["none".to_owned(), "backup/model".to_owned()], &providers)
                .is_err()
        );
    }

    #[test]
    fn model_names_normalize_vendor_prefixes_versions_and_snapshots() {
        for (input, expected) in [
            ("claude-opus-5-5", "claude-opus-5-5"),
            ("anthropic/claude-opus-5.5", "claude-opus-5-5"),
            ("Claude-Opus-5.5", "claude-opus-5-5"),
            ("claude-opus-5-5-20260801", "claude-opus-5-5"),
            ("anthropic/claude-opus-5.5:batch", "claude-opus-5-5:batch"),
            ("gpt-5.1-codex", "gpt-5-1-codex"),
            ("v1.beta", "v1.beta"),
        ] {
            assert_eq!(same_model(input), expected, "{input}");
        }
    }

    #[test]
    fn automatic_groups_keep_provider_order_and_prefer_catalog_names() {
        let groups = auto_groups(&[
            model_entry(
                "openrouter",
                "anthropic/claude-opus-5.5",
                "anthropic/claude-opus-5.5",
            ),
            model_entry("copilot", "claude-opus-5.5", "claude-opus-5.5"),
            model_entry("claude", "claude-opus-5-5", "Claude Opus 5.5"),
        ]);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "auto-claude-opus-5-5");
        assert_eq!(groups[0].name, "Claude Opus 5.5");
        assert_eq!(
            groups[0]
                .members
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "openrouter/anthropic/claude-opus-5.5",
                "copilot/claude-opus-5.5",
                "claude/claude-opus-5-5",
            ]
        );
    }

    #[test]
    fn saved_groups_override_auto_groups_and_hidden_ones_stay_hidden() {
        let entries = [
            model_entry("first", "model-1", "Model One"),
            model_entry("second", "model-1", "Model One"),
        ];
        let auto_id = "auto-model-1";
        let hidden = Group {
            id: auto_id.to_owned(),
            hidden: true,
            ..Group::default()
        };

        let hidden_groups = groups_in(&[hidden], &entries);
        assert_eq!(hidden_groups.len(), 1);
        assert!(hidden_groups[0].auto);
        assert!(hidden_groups[0].hidden);

        let custom = Group {
            id: auto_id.to_owned(),
            name: "My model pool".to_owned(),
            members: vec!["first/model-1".to_owned()],
            ..Group::default()
        };
        let custom_groups = groups_in(&[custom], &entries);
        assert_eq!(custom_groups.len(), 1);
        assert_eq!(custom_groups[0].name, "My model pool");
        assert!(!custom_groups[0].auto);
    }

    #[test]
    fn provider_file_keeps_unknown_group_fields_when_saved() {
        let value = serde_json::json!({
            "providers": [],
            "groups": [{
                "id": "shared-model",
                "name": "Shared model",
                "members": ["first/model", "second/model"],
                "futureOption": {"enabled": true}
            }]
        });
        let file: ProviderFile = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(file).unwrap(), value);
    }

    #[test]
    fn key_accounts_keep_unknown_fields_when_saved() {
        let value = serde_json::json!({
            "name": "Team",
            "key": "secret",
            "off": true,
            "protocol": "anthropic",
            "futureField": {"enabled": true}
        });
        let key: KeyAccount = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(key).unwrap(), value);
    }

    #[test]
    fn key_ids_match_go_gateway_fingerprints() {
        assert_eq!(key_id("k"), "8254c329a9");
    }

    #[test]
    fn provider_key_routing_accepts_documented_aliases() {
        assert_eq!(normalize_routing("smart").unwrap(), "");
        assert_eq!(normalize_routing("round-robin").unwrap(), "rotate");
        assert_eq!(normalize_routing("least-used").unwrap(), "usage");
        assert!(normalize_routing("random").is_err());
    }

    #[test]
    fn provider_affinity_accepts_documented_modes_and_aliases() {
        assert_eq!(normalize_affinity("auto").unwrap(), "");
        assert_eq!(normalize_affinity("ALWAYS").unwrap(), "session");
        assert_eq!(normalize_affinity("within-a-turn").unwrap(), "turn");
        assert_eq!(normalize_affinity("never").unwrap(), "off");
        assert!(normalize_affinity("sticky").is_err());
    }
}
