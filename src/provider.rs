use std::{collections::BTreeMap, fs, str::FromStr};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::settings;

const USAGE: &str = "usage: magpie presets | magpie providers | magpie provider <id> | magpie provider add <preset> [key] | magpie provider add <name> id=<id> url=<url> key=<key> | magpie provider key <id> <key> | magpie provider rm <id>";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresetKind {
    Vendor,
    Relay,
    Local,
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
        ..Preset::EMPTY
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
        ..Preset::EMPTY
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
    groups: Vec<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct Provider {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    icon: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    preset: String,
    key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    key_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keys: Vec<Value>,
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
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

pub(crate) struct GatewayProvider {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) key: String,
    pub(crate) chat: String,
    pub(crate) responses: String,
    pub(crate) anthropic: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) models: Vec<String>,
    pub(crate) hidden: bool,
}

pub(crate) fn gateway_providers() -> Result<Vec<GatewayProvider>> {
    Ok(load()?
        .providers
        .into_iter()
        .map(|provider| GatewayProvider {
            id: provider.id,
            name: provider.name,
            key: provider.key,
            chat: provider.chat,
            responses: provider.responses,
            anthropic: provider.anthropic,
            headers: provider.headers,
            models: provider.models,
            hidden: provider.hidden,
        })
        .collect())
}

pub fn list() -> Result<()> {
    let providers = load()?.providers;
    if providers.is_empty() {
        println!("no providers yet · magpie provider add <name> url=<url> key=<key>");
        return Ok(());
    }

    for provider in providers {
        let host = provider.host();
        let key = if provider.key.is_empty() {
            "no key".to_owned()
        } else {
            mask(&provider.key)
        };
        println!(
            "  {:20} {:16} {:28} {:20} {} models{}",
            provider.name,
            provider.id,
            host,
            key,
            provider.models.len(),
            if provider.hidden { "  [hidden]" } else { "" }
        );
    }
    Ok(())
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

pub fn command(args: &[String]) -> Result<()> {
    match args {
        [] => bail!("{USAGE}"),
        [verb] if verb == "presets" => presets(),
        [verb, rest @ ..] if verb == "add" => add(rest),
        [verb, id, key] if verb == "key" => change_key(id, key),
        [verb, id] if verb == "rm" => remove(id),
        [id] => show(id),
        _ => bail!("{USAGE}"),
    }
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
    if preset.is_some() {
        if let [key] = assignments {
            if !is_provider_assignment(key) {
                provider.key = key.trim().to_owned();
                assignments = &[];
            }
        }
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
            "icon" => provider.icon = value.to_owned(),
            "catalog" => provider.catalog = value.to_owned(),
            "website" => provider.website = value.to_owned(),
            "keysurl" => provider.keys_url = value.to_owned(),
            "balance" => provider.balance_url = value.to_owned(),
            "balance.path" => provider.balance_path = value.to_owned(),
            "models" => provider.models = clean_list(value),
            "fallback" => provider.fallback = clean_list(value),
            "routing" if matches!(value, "order" | "rotate" | "usage") => {
                provider.routing = value.to_owned();
            }
            "routing" => bail!("routing must be order, rotate, or usage"),
            "affinity" | "stays" if matches!(value, "session" | "turn" | "off") => {
                provider.affinity = value.to_owned();
            }
            "affinity" | "stays" if value.is_empty() || value == "auto" => {
                provider.affinity.clear();
            }
            "affinity" | "stays" => bail!("affinity must be auto, session, turn, or off"),
            header if header.starts_with("header.") && header.len() > "header.".len() => {
                let (_, name) = key.split_once('.').context("invalid header assignment")?;
                let name = name.trim();
                ensure!(!name.is_empty(), "header name is empty");
                provider.headers.insert(name.to_owned(), value.to_owned());
            }
            _ => bail!("unsupported provider field {key:?}"),
        }
    }

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
    let key = if provider.key.is_empty() {
        "no key".to_owned()
    } else {
        mask(&provider.key)
    };
    println!("✓ added {} ({}) · {key}", provider.name, provider.id);
    Ok(())
}

fn show(id: &str) -> Result<()> {
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
        if provider.key.is_empty() {
            "no key".to_owned()
        } else {
            mask(&provider.key)
        }
    );
    if !provider.models.is_empty() {
        println!("  models: {}", provider.models.join(", "));
    }
    if !provider.fallback.is_empty() {
        println!("  fallback: {}", provider.fallback.join(" → "));
    }
    Ok(())
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

fn remove(id: &str) -> Result<()> {
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
    println!("✓ removed {} ({})", provider.name, provider.id);
    Ok(())
}

fn find(id: &str) -> Result<Provider> {
    load()?
        .providers
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

impl Provider {
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
