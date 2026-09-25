use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{OnceLock, RwLock},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};

use crate::{config, settings};

const MAX_MODEL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 64 * 1024 * 1024;
const MAX_EXPOSED_MODELS: usize = 24;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Model {
    #[serde(alias = "ID")]
    pub id: String,
    #[serde(alias = "Name")]
    pub name: String,
    #[serde(alias = "Provider")]
    pub provider: String,
    #[serde(alias = "Released")]
    pub released: String,
    #[serde(alias = "Efforts")]
    pub efforts: Vec<String>,
    #[serde(default, alias = "APIs", skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<String>,
    #[serde(alias = "Temperature")]
    pub temperature: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(alias = "Images")]
    pub images: bool,
    #[serde(rename = "imageInput", alias = "ImageInput")]
    pub image_input: Option<bool>,
    #[serde(alias = "Context")]
    pub context: usize,
    #[serde(alias = "Keys", skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Price {
    pub fn cost(self, input: usize, output: usize, cache_read: usize, cache_write: usize) -> f64 {
        (input as f64 * self.input
            + output as f64 * self.output
            + cache_read as f64 * self.cache_read
            + cache_write as f64 * self.cache_write)
            / 1_000_000.0
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct LiveCache {
    #[serde(rename = "base")]
    _base: String,
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<LiveModel>,
    #[serde(default)]
    models: Vec<LiveModel>,
}

#[derive(Deserialize)]
struct LiveModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, alias = "displayName")]
    display_name: String,
    #[serde(default)]
    modalities: Modalities,
}

#[derive(Default, Deserialize)]
struct Modalities {
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    output: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CatalogProvider {
    models: BTreeMap<String, CatalogModel>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CatalogModel {
    id: String,
    name: String,
    release_date: String,
    temperature: Option<bool>,
    reasoning_options: Vec<ReasoningOption>,
    modalities: Modalities,
    cost: Option<Price>,
    limit: ModelLimit,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ReasoningOption {
    #[serde(rename = "type")]
    kind: String,
    values: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModelLimit {
    context: usize,
    input: usize,
}

static CATALOG: OnceLock<RwLock<HashMap<String, CatalogProvider>>> = OnceLock::new();

pub fn live_models(provider_id: &str) -> Vec<Model> {
    let Some(path) = live_path(provider_id) else {
        return Vec::new();
    };
    let Ok(contents) = fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice::<LiveCache>(&contents)
        .map(|cache| cache.models)
        .unwrap_or_default()
}

pub fn available_models(provider_id: &str, catalog_id: &str) -> Vec<Model> {
    let known = catalog_models(catalog_id);
    let live = live_models(provider_id);
    if live.is_empty() {
        if provider_id == "codex" {
            let cached = crate::codex::cached_models();
            if !cached.is_empty() {
                return cached
                    .into_iter()
                    .map(|id| Model {
                        name: id.clone(),
                        id,
                        provider: "openai".to_owned(),
                        ..Model::default()
                    })
                    .collect();
            }
        }
        return known
            .into_iter()
            .filter(|model| !model.id.contains("-exp") && !model.id.contains("preview"))
            .collect();
    }

    let known_by_id = known
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect::<HashMap<_, _>>();
    live.into_iter()
        .map(|mut model| {
            let known = known_by_id.get(model.id.as_str()).or_else(|| {
                model
                    .id
                    .rsplit_once('/')
                    .and_then(|(_, bare)| known_by_id.get(bare))
            });
            if let Some(known) = known {
                if model.name.is_empty() || model.name == model.id {
                    model.name.clone_from(&known.name);
                }
                model.provider.clone_from(&known.provider);
                model.released.clone_from(&known.released);
                model.efforts.clone_from(&known.efforts);
                if model.image_input.is_none() {
                    model.image_input = known.image_input;
                    model.images |= known.images;
                }
                if model.context == 0 {
                    model.context = known.context;
                }
                if model.price.is_none() {
                    model.price = known.price;
                }
                if known.temperature.is_some() {
                    model.temperature = known.temperature;
                }
            }
            model
        })
        .collect()
}

pub fn exposed_models(provider_id: &str, catalog_id: &str, selected: &[String]) -> Vec<Model> {
    let available = available_models(provider_id, catalog_id);
    if !selected.is_empty() {
        let by_id = available
            .iter()
            .map(|model| (model.id.as_str(), model))
            .collect::<std::collections::HashMap<_, _>>();
        return selected
            .iter()
            .map(|id| {
                by_id.get(id.as_str()).map_or_else(
                    || Model {
                        id: id.clone(),
                        name: id.clone(),
                        ..Model::default()
                    },
                    |model| (*model).clone(),
                )
            })
            .collect();
    }

    available.into_iter().take(MAX_EXPOSED_MODELS).collect()
}

pub fn price_of(provider_id: &str, model_id: &str) -> Option<Price> {
    catalog_models(provider_id)
        .into_iter()
        .find(|model| model.id == model_id)
        .and_then(|model| model.price)
}

pub async fn fetch_models(
    endpoints: &[(&str, bool)],
    key: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<(String, Vec<Model>)> {
    let client = crate::netproxy::builder()
        .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(8))
        .build()
        .context("create model-catalog HTTP client")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut attempted = HashSet::new();
    let mut last_error = None;

    for (base, anthropic) in endpoints
        .iter()
        .copied()
        .filter(|(base, _)| !base.is_empty())
    {
        let base = base.trim_end_matches('/');
        for url in candidate_urls(base) {
            if !attempted.insert((url.clone(), anthropic)) {
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let mut request = client
                .get(&url)
                .timeout(remaining.min(Duration::from_secs(8)))
                .header(header::ACCEPT, "application/json")
                .header(header::USER_AGENT, "magpie");
            if !key.is_empty() {
                request = request
                    .header(header::AUTHORIZATION, format!("Bearer {key}"))
                    .header("x-api-key", key);
            }
            if anthropic {
                request = request.header("anthropic-version", "2023-06-01");
            }
            for (name, value) in headers {
                let name = header::HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid custom header name {name:?}"))?;
                let value = header::HeaderValue::from_str(value)
                    .with_context(|| format!("invalid value for custom header {name}"))?;
                request = request.header(name, value);
            }

            let mut response = match request.send().await {
                Ok(response) if response.status() == StatusCode::OK => response,
                Ok(response) => {
                    last_error = Some(anyhow::anyhow!("{url}: {}", response.status()));
                    continue;
                }
                Err(error) => {
                    last_error = Some(error.into());
                    continue;
                }
            };
            let mut body = Vec::with_capacity(
                response
                    .content_length()
                    .unwrap_or_default()
                    .min(MAX_MODEL_RESPONSE_BYTES as u64) as usize,
            );
            while let Some(chunk) = response.chunk().await.context("read model list")? {
                ensure!(
                    body.len().saturating_add(chunk.len()) <= MAX_MODEL_RESPONSE_BYTES,
                    "{url}: model-list response exceeded 8 MiB"
                );
                body.extend_from_slice(&chunk);
            }
            match parse_model_list(&body) {
                Ok(models) if !models.is_empty() => return Ok((base.to_owned(), models)),
                Ok(_) => last_error = Some(anyhow::anyhow!("{url}: model list is empty")),
                Err(error) => {
                    last_error = Some(error.context(format!("{url}: invalid model list")))
                }
            }
        }
    }

    match last_error {
        Some(error) => Err(error),
        None => bail!("provider has no endpoint to ask for models"),
    }
}

pub async fn sync_models_dev() -> Result<usize> {
    let client = crate::netproxy::builder()
        .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("create models.dev HTTP client")?;
    let mut response = client
        .get("https://models.dev/api.json")
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .context("fetch models.dev catalog")?;
    ensure!(
        response.status() == StatusCode::OK,
        "models.dev returned {}",
        response.status()
    );
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_CATALOG_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.context("read models.dev catalog")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_CATALOG_BYTES,
            "models.dev catalog exceeded 64 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    let catalog = parse_catalog(&bytes)?;
    let provider_count = catalog.len();
    let path = catalog_path();
    let parent = path.parent().context("catalog path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create catalog directory {}", parent.display()))?;
    config::atomic_write_for_settings(&path, &bytes)
        .with_context(|| format!("write models.dev catalog {}", path.display()))?;
    let cache = CATALOG.get_or_init(|| RwLock::new(HashMap::new()));
    *cache
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = catalog;
    Ok(provider_count)
}

pub fn is_stale() -> bool {
    let Ok(metadata) = fs::metadata(catalog_path()) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > Duration::from_secs(7 * 24 * 60 * 60))
}

pub async fn sync_if_stale() -> Result<Option<usize>> {
    if is_stale() {
        sync_models_dev().await.map(Some)
    } else {
        Ok(None)
    }
}

fn catalog_models(provider_id: &str) -> Vec<Model> {
    let providers = CATALOG.get_or_init(|| {
        let providers = fs::read(catalog_path())
            .ok()
            .and_then(|bytes| parse_catalog(&bytes).ok())
            .unwrap_or_default();
        RwLock::new(providers)
    });
    let providers = providers
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(provider) = providers.get(provider_id) else {
        return Vec::new();
    };

    let mut models = provider
        .models
        .iter()
        .filter_map(|(key, raw)| {
            let id = if raw.id.is_empty() {
                key.clone()
            } else {
                raw.id.clone()
            };
            is_text_model(&id, &raw.modalities.output).then(|| {
                let name = if raw.name.is_empty() {
                    id.clone()
                } else {
                    raw.name.clone()
                };
                let efforts = raw
                    .reasoning_options
                    .iter()
                    .filter(|option| option.kind == "effort")
                    .flat_map(|option| option.values.iter().cloned())
                    .collect();
                let images = raw
                    .modalities
                    .input
                    .iter()
                    .any(|modality| modality == "image");
                Model {
                    id,
                    name,
                    provider: provider_id.to_owned(),
                    released: raw.release_date.clone(),
                    efforts,
                    apis: Vec::new(),
                    temperature: raw.temperature,
                    price: raw.cost,
                    images,
                    image_input: (!raw.modalities.input.is_empty()).then_some(images),
                    context: if raw.limit.input > 0 {
                        raw.limit.input
                    } else {
                        raw.limit.context
                    },
                    keys: Vec::new(),
                }
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .released
            .cmp(&left.released)
            .then_with(|| left.id.cmp(&right.id))
    });
    models
}

fn parse_catalog(bytes: &[u8]) -> Result<HashMap<String, CatalogProvider>> {
    let catalog: HashMap<String, CatalogProvider> =
        serde_json::from_slice(bytes).context("models.dev returned an invalid catalog")?;
    ensure!(!catalog.is_empty(), "models.dev returned an empty catalog");
    Ok(catalog)
}

fn catalog_path() -> PathBuf {
    settings::cache_dir().join("magpie/models.json")
}

pub fn save_live(provider_id: &str, base: &str, models: Vec<Model>) -> Result<()> {
    let path = live_path(provider_id).context("provider id is not safe for the model cache")?;
    let parent = path.parent().context("model cache path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create model cache {}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(&LiveCache {
        _base: base.to_owned(),
        models,
    })
    .context("serialize provider model list")?;
    config::atomic_write_for_settings(&path, &bytes)
        .with_context(|| format!("write model cache {}", path.display()))
}

fn live_path(provider_id: &str) -> Option<PathBuf> {
    let safe = !provider_id.is_empty()
        && provider_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    safe.then(|| {
        settings::cache_dir()
            .join("magpie/models")
            .join(format!("{provider_id}.json"))
    })
}

fn candidate_urls(base: &str) -> Vec<String> {
    let mut urls = Vec::with_capacity(4);
    let mut add = |url: String| {
        if !urls.contains(&url) {
            urls.push(url);
        }
    };
    add(format!("{base}/models"));
    add(format!("{base}/v1/models"));

    let suffixes = [
        "/anthropic",
        "/apps/anthropic",
        "/api/anthropic",
        "/v1",
        "/api",
        "/api/v1",
    ];
    let root = suffixes
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base);
    add(format!("{root}/v1/models"));
    add(format!("{root}/models"));
    urls
}

fn parse_model_list(bytes: &[u8]) -> Result<Vec<Model>> {
    let response: ModelList =
        serde_json::from_slice(bytes).context("response is not a model list")?;
    let rows = if response.data.is_empty() {
        response.models
    } else {
        response.data
    };
    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let id = if row.id.is_empty() {
            row.name.clone()
        } else {
            row.id
        };
        if id.is_empty() || !is_text_model(&id, &row.modalities.output) {
            continue;
        }
        let name = if row.display_name.is_empty() {
            if row.name.is_empty() {
                id.clone()
            } else {
                row.name
            }
        } else {
            row.display_name
        };
        let image_input = (!row.modalities.input.is_empty()).then(|| {
            row.modalities
                .input
                .iter()
                .any(|modality| modality == "image")
        });
        models.push(Model {
            id,
            name,
            images: image_input == Some(true),
            image_input,
            ..Model::default()
        });
    }
    Ok(models)
}

fn is_text_model(id: &str, output: &[String]) -> bool {
    if !output.is_empty() && !output.iter().any(|modality| modality == "text") {
        return false;
    }
    let id = id.to_ascii_lowercase();
    ![
        "embed",
        "-tts",
        "image",
        "audio",
        "-live",
        "robotics",
        "computer-use",
        "deep-research",
        "transcribe",
        "realtime",
        "moderation",
        "whisper",
        "dall-e",
        "sora",
    ]
    .iter()
    .any(|excluded| id.contains(excluded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_endpoints_are_deduplicated_and_cover_common_base_paths() {
        assert_eq!(
            candidate_urls("https://example.com/v1"),
            vec![
                "https://example.com/v1/models".to_owned(),
                "https://example.com/v1/v1/models".to_owned(),
                "https://example.com/models".to_owned(),
            ]
        );
    }

    #[test]
    fn model_lists_keep_server_order_and_skip_non_text_products() {
        let models = parse_model_list(
            br#"{"data":[{"id":"latest-chat","display_name":"Latest"},{"id":"model-embed-v1"},{"id":"vision","modalities":{"input":["text","image"],"output":["text"]}},{"id":"image-only","modalities":{"output":["image"]}}]}"#,
        )
        .expect("valid provider model list");
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["latest-chat", "vision"]
        );
        assert_eq!(models[0].name, "Latest");
        assert_eq!(models[1].image_input, Some(true));
    }

    #[test]
    fn provider_cache_paths_reject_traversal() {
        assert!(live_path("../other").is_none());
        assert!(live_path("provider-2").is_some());
    }

    #[test]
    fn models_dev_catalog_keeps_names_capabilities_and_context() {
        let catalog = parse_catalog(
            br#"{"openai":{"models":{"gpt-next":{"id":"gpt-next","name":"GPT Next","release_date":"2026-09-01","temperature":false,"reasoning_options":[{"type":"effort","values":["low","high"]}],"modalities":{"input":["text","image"],"output":["text"]},"limit":{"context":400000,"input":272000}}}}}"#,
        )
        .expect("valid models.dev catalog");
        let model = &catalog["openai"].models["gpt-next"];
        assert_eq!(model.name, "GPT Next");
        assert_eq!(model.temperature, Some(false));
        assert_eq!(
            model.reasoning_options[0].values,
            vec!["low".to_owned(), "high".to_owned()]
        );
        assert_eq!(model.limit.input, 272000);
        assert_eq!(
            model.modalities.input,
            vec!["text".to_owned(), "image".to_owned()]
        );
    }
}
