use std::{collections::BTreeMap, fs, str::FromStr};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::settings;

const USAGE: &str = "usage: magpie providers | magpie provider <id> | magpie provider add <name> id=<id> url=<url> key=<key> [header.X-Name=value] | magpie provider key <id> <key> | magpie provider rm <id>";

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

pub fn command(args: &[String]) -> Result<()> {
    match args {
        [] => bail!("{USAGE}"),
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
    let mut provider = Provider {
        name: name.trim().to_owned(),
        ..Provider::default()
    };

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
