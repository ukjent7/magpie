use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, IsTerminal, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use jsonc_parser::{ParseOptions, cst::CstRootNode};
use serde::Deserialize;
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table};
use url::Url;

use super::{PRESETS, PresetKind, Provider, prepare_provider};

const USAGE: &str = "usage: magpie import apps [claude|codex] [--yes]";

#[derive(Clone)]
struct Candidate {
    source: String,
    provider: Provider,
    skipped: Option<String>,
}

#[derive(Default)]
struct Endpoints {
    chat: String,
    responses: String,
    anthropic: String,
}

pub(super) fn command(args: &[String]) -> Result<()> {
    let mut automatic = false;
    let mut sources = Vec::new();
    for argument in args {
        match argument.as_str() {
            "apps" => {}
            "-y" | "--yes" => automatic = true,
            "claude" | "claude-code" => push_unique(&mut sources, "claude"),
            "codex" => push_unique(&mut sources, "codex"),
            value if value.starts_with('-') => bail!("unknown flag {value} ({USAGE})"),
            _ => bail!(USAGE),
        }
    }
    if sources.is_empty() {
        sources.extend(["claude", "codex"]);
    }

    let mut candidates = Vec::new();
    let mut found_source = false;
    for source in sources {
        let path = source_path(source);
        if !path.is_file() {
            println!("  {} not found: {}", source_name(source), path.display());
            continue;
        }
        found_source = true;
        let contents = fs::read_to_string(&path).with_context(|| {
            format!(
                "read {} configuration {}",
                source_name(source),
                path.display()
            )
        })?;
        let mut imported = match source {
            "claude" => claude_imports(&contents)?,
            "codex" => codex_imports(&contents, &path)?,
            _ => unreachable!("source names are validated above"),
        };
        candidates.append(&mut imported);
    }

    merge_candidates(&mut candidates);
    if !found_source {
        println!("No Claude Code or Codex provider configuration was found.");
        return Ok(());
    }

    let imports = candidates
        .iter()
        .filter(|candidate| candidate.skipped.is_none())
        .cloned()
        .collect::<Vec<_>>();
    let skipped = candidates
        .iter()
        .filter(|candidate| candidate.skipped.is_some())
        .collect::<Vec<_>>();
    if imports.is_empty() {
        println!("No importable providers were found.");
    }

    for candidate in skipped {
        println!(
            "  {} · {} · skipped: {}",
            display_text(&candidate.source),
            display_text(&candidate.provider.name),
            candidate
                .skipped
                .as_deref()
                .unwrap_or("unsupported configuration")
        );
    }
    if imports.is_empty() {
        return Ok(());
    }

    let existing = super::load()?.providers;
    let mut planned = existing.clone();
    for candidate in &imports {
        show_candidate(candidate, &planned);
        let _ = apply_candidate(&mut planned, candidate.provider.clone())?;
    }

    if !automatic {
        ensure!(
            io::stdin().is_terminal(),
            "not a terminal: use `magpie import apps --yes` to import without asking"
        );
        print!("Import these providers? [y/N] ");
        io::stdout().flush().context("write import prompt")?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("read import confirmation")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("nothing imported");
        }
    }

    let mut file = super::load()?;
    let mut added = Vec::new();
    let mut keys = Vec::new();
    let mut updated = Vec::new();
    let mut unchanged = Vec::new();
    for candidate in imports {
        match apply_candidate(&mut file.providers, candidate.provider)? {
            Applied::Provider(name, id) => added.push(format!("{name} ({id})")),
            Applied::Key(name, id) => keys.push(format!("{name} ({id})")),
            Applied::Updated(name) => updated.push(name),
            Applied::Unchanged(name) => unchanged.push(name),
        }
    }
    if !added.is_empty() || !keys.is_empty() || !updated.is_empty() {
        super::store(file)?;
    }
    if !added.is_empty() {
        println!("✓ added {}", added.join(", "));
    }
    if !keys.is_empty() {
        println!("✓ added keys to {}", keys.join(", "));
    }
    if !updated.is_empty() {
        println!("✓ updated {}", updated.join(", "));
    }
    if !unchanged.is_empty() {
        println!("Already configured: {}", unchanged.join(", "));
    }
    Ok(())
}

fn source_path(source: &str) -> PathBuf {
    match source {
        "claude" => env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(|directory| PathBuf::from(directory).join("settings.json"))
            .or_else(|| home_dir().map(|home| home.join(".claude/settings.json")))
            .unwrap_or_else(|| PathBuf::from(".claude/settings.json")),
        "codex" => env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|home| home.join(".codex")))
            .unwrap_or_else(|| PathBuf::from(".codex"))
            .join("config.toml"),
        _ => unreachable!("source names are validated above"),
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn source_name(source: &str) -> &'static str {
    match source {
        "claude" => "Claude Code",
        "codex" => "Codex",
        _ => "unknown app",
    }
}

fn claude_imports(contents: &str) -> Result<Vec<Candidate>> {
    let root = CstRootNode::parse(contents, &ParseOptions::default())
        .context("parse Claude Code's settings.json")?;
    let Some(settings) = root.value().and_then(|node| node.to_serde_value()) else {
        bail!("Claude Code's settings.json is not a JSON object");
    };
    let environment = settings.get("env").and_then(Value::as_object);
    let get = |name: &str| {
        environment
            .and_then(|values| values.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("")
    };
    let mut name = get("ANTHROPIC_BASE_URL").to_owned();
    let mut key = [get("ANTHROPIC_AUTH_TOKEN"), get("ANTHROPIC_API_KEY")]
        .into_iter()
        .find(|value| !value.is_empty() && !is_placeholder(value))
        .unwrap_or_default()
        .to_owned();
    let models = [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
    ]
    .into_iter()
    .map(get)
    .filter(|model| !model.is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let mut models = models;
    dedup(&mut models);

    if name.is_empty() && key.is_empty() {
        return Ok(vec![Candidate {
            source: "Claude Code settings.json".to_owned(),
            provider: Provider {
                name: "Claude Code sign-in".to_owned(),
                ..Provider::default()
            },
            skipped: Some("Claude Code's own sign-in is not an API provider".to_owned()),
        }]);
    }
    if is_placeholder(&key) {
        key.clear();
    }
    if name.is_empty() {
        name = "https://api.anthropic.com".to_owned();
    }
    if is_magpie_gateway(&name) {
        return Ok(vec![Candidate {
            source: "Claude Code settings.json".to_owned(),
            provider: Provider {
                name: "Magpie gateway".to_owned(),
                ..Provider::default()
            },
            skipped: Some("it points at Magpie's own gateway".to_owned()),
        }]);
    }

    Ok(vec![make_candidate(
        "Claude Code settings.json",
        "Claude Code",
        &key,
        Endpoints {
            anthropic: name,
            ..Endpoints::default()
        },
        models,
        BTreeMap::new(),
    )])
}

fn codex_imports(contents: &str, config_path: &Path) -> Result<Vec<Candidate>> {
    let document = contents
        .parse::<DocumentMut>()
        .context("parse Codex config.toml")?;
    let active_provider = document
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or_default();
    let active_model = document
        .get("model")
        .and_then(Item::as_str)
        .unwrap_or_default();
    let active_catalog = document
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .unwrap_or_default();
    let model_providers = document.get("model_providers").and_then(Item::as_table);
    let Some(model_providers) = model_providers else {
        return Ok(Vec::new());
    };
    let profiles = document.get("profiles").and_then(Item::as_table);
    let mut candidates = Vec::new();

    for (id, item) in model_providers.iter() {
        let Some(table) = item.as_table() else {
            continue;
        };
        let name = string(table, "name").unwrap_or(id);
        let base = string(table, "base_url").unwrap_or_default();
        let token = string(table, "experimental_bearer_token")
            .filter(|token| !is_placeholder(token))
            .unwrap_or_default();
        let mut models = Vec::new();
        if id == active_provider {
            push_model(&mut models, active_model);
            models.extend(codex_catalog(config_path, active_catalog));
        }
        if let Some(profiles) = profiles {
            for (_, profile) in profiles.iter() {
                let Some(profile) = profile.as_table() else {
                    continue;
                };
                let profile_provider = string(profile, "model_provider").unwrap_or(active_provider);
                if profile_provider != id {
                    continue;
                }
                if let Some(model) = string(profile, "model") {
                    push_model(&mut models, model);
                }
                let catalog = string(profile, "model_catalog_json")
                    .or_else(|| (id == active_provider).then_some(active_catalog))
                    .unwrap_or_default();
                models.extend(codex_catalog(config_path, catalog));
            }
        }
        dedup(&mut models);

        if id == "magpie" || is_magpie_gateway(base) {
            candidates.push(Candidate {
                source: format!("Codex config.toml · {id}"),
                provider: Provider {
                    name: name.to_owned(),
                    ..Provider::default()
                },
                skipped: Some("it points at Magpie's own gateway".to_owned()),
            });
            continue;
        }
        if base.is_empty() && token.is_empty() {
            candidates.push(Candidate {
                source: format!("Codex config.toml · {id}"),
                provider: Provider {
                    name: name.to_owned(),
                    ..Provider::default()
                },
                skipped: Some(if string(table, "env_key").is_some() {
                    "it names an env_key; Magpie imports only credentials written in the config"
                        .to_owned()
                } else {
                    "it has no base_url or explicit bearer token".to_owned()
                }),
            });
            continue;
        }

        let base = if base.is_empty() {
            "https://api.openai.com/v1"
        } else {
            base
        };
        let endpoints = if string(table, "wire_api") == Some("chat") {
            Endpoints {
                chat: base.to_owned(),
                ..Endpoints::default()
            }
        } else {
            Endpoints {
                responses: base.to_owned(),
                ..Endpoints::default()
            }
        };
        let headers = table
            .get("http_headers")
            .and_then(Item::as_table)
            .map(read_headers)
            .unwrap_or_default();
        candidates.push(make_candidate(
            &format!("Codex config.toml · {id}"),
            name,
            token,
            endpoints,
            models,
            headers,
        ));
    }
    Ok(candidates)
}

fn make_candidate(
    source: &str,
    name: &str,
    key: &str,
    endpoints: Endpoints,
    models: Vec<String>,
    headers: BTreeMap<String, String>,
) -> Candidate {
    let rejected = if key.chars().any(char::is_control) {
        Some("its API key contains control characters".to_owned())
    } else if [&endpoints.chat, &endpoints.responses, &endpoints.anthropic]
        .into_iter()
        .filter(|endpoint| !endpoint.is_empty())
        .any(contains_url_credentials)
    {
        Some("its base URL embeds credentials; use an explicit API key instead".to_owned())
    } else {
        None
    };
    let mut provider = Provider {
        id: super::slug(name),
        name: name.trim().to_owned(),
        key: key.to_owned(),
        chat: clean_base(&endpoints.chat),
        responses: clean_base(&endpoints.responses),
        anthropic: clean_base(&endpoints.anthropic),
        models,
        headers,
        ..Provider::default()
    };
    if let Some(skipped) = rejected {
        return Candidate {
            source: source.to_owned(),
            provider: Provider {
                name: name.to_owned(),
                ..Provider::default()
            },
            skipped: Some(skipped),
        };
    }
    if provider.key.is_empty() && !provider.is_local() {
        return Candidate {
            source: source.to_owned(),
            provider: Provider {
                name: name.to_owned(),
                ..Provider::default()
            },
            skipped: Some("it has no explicit API key".to_owned()),
        };
    }
    apply_preset(&mut provider);
    match prepare_provider(provider) {
        Ok(provider) => Candidate {
            source: source.to_owned(),
            provider,
            skipped: None,
        },
        Err(_) => Candidate {
            source: source.to_owned(),
            provider: Provider {
                name: name.to_owned(),
                ..Provider::default()
            },
            skipped: Some("its provider URL or configuration is invalid".to_owned()),
        },
    }
}

fn contains_url_credentials(value: &str) -> bool {
    Url::parse(&clean_base(value))
        .is_ok_and(|url| !url.username().is_empty() || url.password().is_some())
}

fn apply_preset(provider: &mut Provider) {
    for preset in PRESETS {
        if preset.kind == PresetKind::Local {
            continue;
        }
        let given = [&provider.chat, &provider.responses, &provider.anthropic];
        let preset_urls = [preset.chat, preset.responses, preset.anthropic];
        let preset_hosts = preset_urls
            .iter()
            .filter_map(|url| host(url))
            .collect::<Vec<_>>();
        let hit = given
            .iter()
            .filter_map(|url| host(url))
            .any(|given_host| preset_hosts.contains(&given_host));
        if !hit {
            continue;
        }

        let exact = given
            .iter()
            .zip(preset_urls)
            .filter(|(url, _)| !url.is_empty())
            .all(|(url, expected)| url.trim_end_matches('/') == expected.trim_end_matches('/'));
        provider.id = preset.id.to_owned();
        provider.icon = preset.icon.to_owned();
        provider.catalog = preset.catalog.to_owned();
        if exact {
            provider.preset = preset.id.to_owned();
            provider.chat = preset.chat.to_owned();
            provider.responses = preset.responses.to_owned();
            provider.anthropic = preset.anthropic.to_owned();
            provider.website = preset.website.to_owned();
            provider.keys_url = preset.keys_url.to_owned();
        }
        break;
    }
}

fn codex_catalog(config_path: &Path, raw_path: &str) -> Vec<String> {
    if raw_path.trim().is_empty() {
        return Vec::new();
    }
    let path = if let Some(relative) = raw_path.strip_prefix("~/") {
        let Some(home) = home_dir() else {
            return Vec::new();
        };
        home.join(relative)
    } else {
        let raw_path = Path::new(raw_path);
        if raw_path.is_absolute() {
            raw_path.to_owned()
        } else {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(raw_path)
        }
    };
    if path
        .file_name()
        .is_some_and(|name| name == "magpie-models.json")
    {
        return Vec::new();
    }
    let Ok(contents) = fs::read(path) else {
        return Vec::new();
    };
    #[derive(Deserialize)]
    struct Catalog {
        #[serde(default)]
        models: Vec<CatalogModel>,
    }
    #[derive(Deserialize)]
    struct CatalogModel {
        #[serde(default)]
        slug: String,
        #[serde(default)]
        visibility: String,
    }
    let Ok(catalog) = serde_json::from_slice::<Catalog>(&contents) else {
        return Vec::new();
    };
    catalog
        .models
        .into_iter()
        .filter(|model| !model.slug.is_empty() && model.visibility != "hide")
        .map(|model| model.slug)
        .collect()
}

fn read_headers(table: &Table) -> BTreeMap<String, String> {
    table
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.to_owned(), value.trim().to_owned()))
        })
        .filter(|(_, value)| !value.is_empty())
        .collect()
}

fn string<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table.get(key).and_then(Item::as_str)
}

fn push_model(models: &mut Vec<String>, model: &str) {
    let model = model.trim();
    if !model.is_empty()
        && !model.chars().any(char::is_control)
        && !models.iter().any(|known| known == model)
    {
        models.push(model.to_owned());
    }
}

fn dedup(models: &mut Vec<String>) {
    let mut seen = Vec::with_capacity(models.len());
    for model in models.drain(..) {
        push_model(&mut seen, &model);
    }
    *models = seen;
}

fn clean_base(value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() || value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    }
}

fn is_magpie_gateway(value: &str) -> bool {
    let Ok(url) = Url::parse(&clean_base(value)) else {
        return false;
    };
    matches!(url.host_str(), Some("127.0.0.1" | "localhost")) && url.port() == Some(3425)
}

fn is_placeholder(value: &str) -> bool {
    let value = value.trim();
    if let Some(variable) = value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
    {
        return !variable.is_empty()
            && variable
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_');
    }
    value.strip_prefix('$').is_some_and(|variable| {
        !variable.is_empty()
            && variable
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    })
}

fn host(value: &str) -> Option<String> {
    let url = Url::parse(&clean_base(value)).ok()?;
    let host = match url.host()? {
        url::Host::Domain(host) => host.to_ascii_lowercase(),
        url::Host::Ipv4(address) => address.to_string(),
        url::Host::Ipv6(address) => format!("[{address}]"),
    };
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn merge_candidates(candidates: &mut Vec<Candidate>) {
    let mut merged: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        if candidate.skipped.is_some() {
            merged.push(candidate);
            continue;
        }
        let existing = merged.iter_mut().find(|known| {
            known.skipped.is_none()
                && known.provider.key == candidate.provider.key
                && known.provider.headers == candidate.provider.headers
                && share_host(&known.provider, &candidate.provider)
        });
        if let Some(existing) = existing {
            merge_endpoints(&mut existing.provider, &candidate.provider);
            existing.source.push_str(" + ");
            existing.source.push_str(&candidate.source);
        } else {
            merged.push(candidate);
        }
    }
    *candidates = merged;
}

fn show_candidate(candidate: &Candidate, existing: &[Provider]) {
    let provider = &candidate.provider;
    let key = if provider.key.is_empty() {
        "no key needed".to_owned()
    } else {
        super::mask(&provider.key)
    };
    let state = match matching_provider(existing, provider) {
        Some(existing) if has_key(existing, &provider.key) => {
            format!("already configured as {}", existing.name)
        }
        Some(existing) if provider.key.is_empty() => format!("merge into {}", existing.name),
        Some(existing) => format!("add key to {}", existing.name),
        None if existing.iter().any(|saved| saved.id == provider.id) => {
            format!("new provider as {}", available_id(existing, &provider.id))
        }
        None => "new provider".to_owned(),
    };
    let models = if provider.models.is_empty() {
        String::new()
    } else {
        format!(" · models {}", provider.models.join(", "))
    };
    println!(
        "  {} · {} ({}) · {} · key {}{}",
        display_text(&candidate.source),
        display_text(&provider.name),
        display_text(&provider.id),
        endpoint_hosts(provider),
        key,
        models
    );
    println!("    {state}");
}

fn matching_provider<'a>(providers: &'a [Provider], imported: &Provider) -> Option<&'a Provider> {
    providers
        .iter()
        .find(|provider| provider.headers == imported.headers && share_host(provider, imported))
}

fn share_host(left: &Provider, right: &Provider) -> bool {
    let left = [&left.chat, &left.responses, &left.anthropic]
        .into_iter()
        .filter_map(|value| host(value))
        .collect::<Vec<_>>();
    [&right.chat, &right.responses, &right.anthropic]
        .into_iter()
        .filter_map(|value| host(value))
        .any(|right| left.contains(&right))
}

fn endpoint_hosts(provider: &Provider) -> String {
    let mut hosts = Vec::new();
    for endpoint in [&provider.chat, &provider.responses, &provider.anthropic] {
        if let Some(host) = host(endpoint)
            && !hosts.contains(&host)
        {
            hosts.push(host);
        }
    }
    hosts.join(", ")
}

fn has_key(provider: &Provider, key: &str) -> bool {
    key.is_empty() && provider.key.is_empty()
        || !key.is_empty()
            && (provider.key == key || provider.keys.iter().any(|saved| saved.key == key))
}

fn merge_endpoints(target: &mut Provider, source: &Provider) -> bool {
    let mut changed = false;
    for (target, source) in [
        (&mut target.chat, &source.chat),
        (&mut target.responses, &source.responses),
        (&mut target.anthropic, &source.anthropic),
    ] {
        if target.is_empty() && !source.is_empty() {
            target.clone_from(source);
            changed = true;
        }
    }
    for model in &source.models {
        if !target.models.contains(model) {
            target.models.push(model.clone());
            changed = true;
        }
    }
    changed
}

enum Applied {
    Provider(String, String),
    Key(String, String),
    Updated(String),
    Unchanged(String),
}

fn apply_candidate(providers: &mut Vec<Provider>, mut imported: Provider) -> Result<Applied> {
    if let Some(target) = providers
        .iter_mut()
        .find(|provider| provider.headers == imported.headers && share_host(provider, &imported))
    {
        let provider_name = target.name.clone();
        let provider_id = target.id.clone();
        let mut changed = merge_endpoints(target, &imported);
        let mut added_key = false;
        if !imported.key.is_empty() && !has_key(target, &imported.key) {
            if target.key.is_empty() {
                target.key.clone_from(&imported.key);
            } else {
                target.keys.push(super::KeyAccount {
                    key: imported.key,
                    ..super::KeyAccount::default()
                });
            }
            changed = true;
            added_key = true;
        }
        return Ok(if changed {
            if added_key {
                Applied::Key(provider_name, provider_id)
            } else {
                Applied::Updated(provider_name)
            }
        } else {
            Applied::Unchanged(provider_name)
        });
    }

    let base_id = imported.id.clone();
    let mut suffix = 2;
    while providers.iter().any(|provider| provider.id == imported.id) || imported.id == "group" {
        imported.id = format!("{base_id}-{suffix}");
        suffix += 1;
    }
    let base_name = imported.name.clone();
    suffix = 2;
    while providers
        .iter()
        .any(|provider| provider.name.eq_ignore_ascii_case(&imported.name))
    {
        imported.name = format!("{base_name} {suffix}");
        suffix += 1;
    }
    let name = imported.name.clone();
    let id = imported.id.clone();
    providers.push(imported);
    Ok(Applied::Provider(name, id))
}

fn available_id(providers: &[Provider], base: &str) -> String {
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if candidate != "group" && !providers.iter().any(|provider| provider.id == candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn push_unique(values: &mut Vec<&'static str>, value: &'static str) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_explicit_codex_token_and_models_without_reading_environment_keys() {
        let config = r#"
model_provider = "relay"
model = "gpt-5.6"

[model_providers.magpie]
base_url = "http://127.0.0.1:3425/v1"
experimental_bearer_token = "magpie"

[model_providers.relay]
name = "Example Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-explicit"
env_key = "RELAY_SECRET"

[model_providers.relay.http_headers]
X-Org = " team-a "

[profiles.review]
model_provider = "relay"
model = "claude-sonnet-5"

[model_providers.unkeyed]
base_url = "https://unkeyed.example/v1"
env_key = "UNREAD_SECRET"
"#;
        let candidates = codex_imports(config, Path::new("/tmp/config.toml")).unwrap();
        let relay = candidates
            .iter()
            .find(|candidate| candidate.provider.name == "Example Relay")
            .unwrap();
        assert_eq!(relay.provider.responses, "https://relay.example/v1");
        assert_eq!(relay.provider.key, "sk-explicit");
        assert_eq!(relay.provider.headers.get("X-Org").unwrap(), "team-a");
        assert_eq!(
            relay.provider.models,
            vec!["gpt-5.6".to_owned(), "claude-sonnet-5".to_owned()]
        );
        assert!(candidates.iter().any(|candidate| {
            candidate.provider.name == "magpie" && candidate.skipped.is_some()
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.provider.name == "unkeyed"
                && candidate
                    .skipped
                    .as_deref()
                    .is_some_and(|reason| reason.contains("env_key"))
        }));
    }

    #[test]
    fn reads_claude_jsonc_without_expanding_secret_placeholders() {
        let settings = r#"{
  // comments remain valid in Claude's settings
  "env": {
    "ANTHROPIC_BASE_URL": "relay.example/anthropic",
    "ANTHROPIC_AUTH_TOKEN": "sk-claude",
    "ANTHROPIC_MODEL": "claude-sonnet-5"
  }
}"#;
        let candidates = claude_imports(settings).unwrap();
        let provider = &candidates[0].provider;
        assert_eq!(provider.anthropic, "https://relay.example/anthropic");
        assert_eq!(provider.key, "sk-claude");
        assert_eq!(provider.models, vec!["claude-sonnet-5".to_owned()]);

        let placeholder = r#"{"env":{"ANTHROPIC_BASE_URL":"relay.example","ANTHROPIC_AUTH_TOKEN":"${ANTHROPIC_TOKEN}"}}"#;
        let candidate = claude_imports(placeholder).unwrap().remove(0);
        assert!(candidate.skipped.is_some());
    }

    #[test]
    fn merges_two_protocol_endpoints_for_the_same_configured_account() {
        let codex = make_candidate(
            "Codex",
            "Relay",
            "sk-same",
            Endpoints {
                chat: "https://relay.example/v1".to_owned(),
                ..Endpoints::default()
            },
            vec!["chat-model".to_owned()],
            BTreeMap::new(),
        );
        let claude = make_candidate(
            "Claude",
            "Relay",
            "sk-same",
            Endpoints {
                anthropic: "https://relay.example/anthropic".to_owned(),
                ..Endpoints::default()
            },
            vec!["sonnet".to_owned()],
            BTreeMap::new(),
        );
        let mut candidates = vec![codex, claude];
        merge_candidates(&mut candidates);
        assert_eq!(candidates.len(), 1);
        let merged = candidates.remove(0);
        assert_eq!(merged.provider.chat, "https://relay.example/v1");
        assert_eq!(merged.provider.anthropic, "https://relay.example/anthropic");
        assert_eq!(
            merged.provider.models,
            vec!["chat-model".to_owned(), "sonnet".to_owned()]
        );
    }
}
