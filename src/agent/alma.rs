// Alma (a desktop AI chat app) keeps its providers and settings in the app,
// not in a file magpie can edit: they are reached through its local REST API
// (http://localhost:23001, while Alma runs).
//
//	GET  /api/providers              [{"id","name","type","baseURL","enabled","models":["<id>",…],"availableModels":[{"id","name",…}]},…]
//	POST /api/providers              {"name","type","apiKey","baseURL","enabled"} → the provider
//	PUT  /api/providers/:id          the fields to change
//	PUT  /api/providers/:id/models   {"models":["<id>",…],"availableModels":[{"id","name"}]}: models, kept as
//	                                 given, are the ones Alma offers; its capabilities it works out itself
//	GET  /api/settings, PUT it back  the whole settings; chat.defaultModel is "<providerId>:<model>"
//	GET  /api/models                 every model Alma reaches, "<providerId>:<model>" ids
//
// magpie is one provider there, named magpie, of type openai at the
// gateway's /v1; choosing one of its models makes it Alma's default model.
// Alma not running is no error: there is nothing to read or keep current.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::MAGPIE_ID;

// down is Alma not answering. A local app that doesn't answer at once isn't
// running, or is stuck.
const DOWN: &str = "Alma isn't running — open Alma and try again";

// A local app that doesn't answer at once isn't running, or is stuck.
const TIMEOUT: Duration = Duration::from_millis(1500);

// dir is where Alma keeps its data (~/Library/Application Support/alma on a
// Mac): there once Alma was installed and opened.
pub(crate) fn dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(dir) = crate::agent::testing::alma_dir() {
            return dir;
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    if cfg!(windows) {
        return std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map_or_else(|| home.clone(), PathBuf::from)
            .join("alma");
    }
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/alma");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join(".config"), PathBuf::from)
        .join("alma")
}

// api is where Alma's API answers. Under test it is nowhere, so no test
// reaches a real Alma; a test points magpie at a fake one.
#[cfg(test)]
fn api() -> String {
    crate::agent::testing::alma_api().unwrap_or_default()
}

#[cfg(not(test))]
fn api() -> String {
    std::env::var("MAGPIE_ALMA_API")
        .ok()
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| "http://localhost:23001".to_owned())
}

// get is Alma's default model, one of magpie's as magpie/<model>. With Alma
// not running it is what magpie last set there, so that isn't taken for
// something else having changed it.
pub(crate) fn get() -> String {
    let read = || -> Result<String> {
        let current = read_default()?;
        let Some((provider_id, model)) = current.split_once(':') else {
            return Ok(current);
        };
        if magpie(&providers()?).is_some_and(|provider| provider.id == provider_id) {
            return Ok(format!("{}/{}", super::MAGPIE_ID, model));
        }
        Ok(current)
    };
    read().unwrap_or_else(|_| {
        super::applied::of("alma")
            .field("model")
            .unwrap_or_default()
            .to_owned()
    })
}

// set makes value Alma's default model: one of magpie's wires magpie's
// provider in, Alma's own is spelled providerId:model, and clearing it
// takes magpie's provider out — the default model is Alma's to pick.
pub(crate) fn set(value: &str) -> Result<()> {
    if value.is_empty() {
        let settings = settings()?;
        let list = providers()?;
        let Some(provider) = magpie(&list) else {
            return Ok(());
        };
        let current = settings
            .get("chat")
            .and_then(|chat| chat.get("defaultModel"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if current
            .split_once(':')
            .is_some_and(|(id, _)| id == provider.id)
        {
            set_default("")?;
        }
        return request("DELETE", &format!("/api/providers/{}", provider.id), None).map(|_| ());
    }
    if let Some(model) = value.strip_prefix(super::MAGPIE_PREFIX) {
        let id = wire()?;
        return set_default(&format!("{id}:{model}"));
    }
    anyhow::ensure!(
        value.contains(':'),
        "expected providerId:model or magpie/<model>, got {value:?}"
    );
    set_default(value)
}

// sync keeps magpie's model list in Alma current, where magpie's provider
// already is. Alma not running is no error: nothing to keep current.
pub(crate) fn sync() -> Result<()> {
    if api().is_empty() || !dir().exists() {
        return Ok(());
    }
    let list = match providers() {
        Ok(list) => list,
        Err(_) => return Ok(()),
    };
    let Some(provider) = magpie(&list) else {
        return Ok(());
    };
    sync_models(provider)
}

// check says what of magpie's wiring in Alma is off while its default model
// is one of magpie's, or "" when it is all there — or when Alma isn't
// running, where there is nothing to compare.
pub(crate) fn check() -> String {
    let default = match read_default() {
        Ok(default) => default,
        Err(_) => return String::new(),
    };
    let list = match providers() {
        Ok(list) => list,
        Err(_) => return String::new(),
    };
    let Some(provider) = magpie(&list) else {
        return String::new();
    };
    let Some((id, _)) = default.split_once(':') else {
        return String::new();
    };
    if id != provider.id {
        return String::new();
    }
    if !provider.enabled {
        return "Alma's magpie provider is turned off, so Alma won't use its models".to_owned();
    }
    let base_url = provider.base_url.clone();
    super::applied::wiring_off(
        "Alma",
        "providers",
        |_key| Some(base_url.clone()),
        &[("baseURL", crate::gateway::v1_url())],
    )
}

// read_default is chat.defaultModel, the model Alma starts chats with.
fn read_default() -> Result<String> {
    Ok(settings()?
        .get("chat")
        .and_then(|chat| chat.get("defaultModel"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

// set_default sets chat.defaultModel, putting the rest of the settings back
// as they were: Alma takes only the whole object.
fn set_default(value: &str) -> Result<()> {
    let mut settings = settings()?;
    let current = settings
        .get("chat")
        .and_then(|chat| chat.get("defaultModel"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if current == value {
        return Ok(());
    }
    match settings.get_mut("chat").and_then(Value::as_object_mut) {
        Some(chat) => {
            chat.insert("defaultModel".to_owned(), json!(value));
        }
        None => {
            settings.insert("chat".to_owned(), json!({"defaultModel": value}));
        }
    }
    request("PUT", "/api/settings", Some(Value::Object(settings))).map(|_| ())
}

// wire makes sure Alma has magpie's provider, pointed at the gateway,
// turned on and listing the catalog, and returns its id.
fn wire() -> Result<String> {
    let list = providers()?;
    if let Some(provider) = magpie(&list) {
        if provider.base_url != crate::gateway::v1_url() || !provider.enabled {
            request(
                "PUT",
                &format!("/api/providers/{}", provider.id),
                Some(json!({
                    "baseURL": crate::gateway::v1_url(),
                    "apiKey": crate::gateway::TOKEN,
                    "enabled": true,
                })),
            )?;
        }
        let id = provider.id.clone();
        sync_models(provider)?;
        return Ok(id);
    }
    let mut made: Provider = request(
        "POST",
        "/api/providers",
        Some(json!({
            "name": MAGPIE_ID,
            "type": "openai",
            "apiKey": crate::gateway::TOKEN,
            "baseURL": crate::gateway::v1_url(),
            "enabled": true,
        })),
    )?
    .and_then(|reply| serde_json::from_value::<Provider>(reply).ok())
    .unwrap_or_default();
    if made.id.is_empty() {
        // a reply without the provider: look it up
        let list = providers()?;
        let Some(provider) = magpie(&list) else {
            bail!("Alma didn't keep magpie's provider");
        };
        made = provider.clone();
    }
    sync_models(&made)?;
    Ok(made.id)
}

// sync_models puts magpie's catalog into its provider in Alma, if it isn't
// there already.
fn sync_models(provider: &Provider) -> Result<()> {
    let mut ids = Vec::new();
    let mut known = Vec::new();
    for model in super::magpie_models()? {
        ids.push(model.id.clone());
        known.push(json!({"id": &model.id, "name": &model.name}));
    }
    if same_models(provider, &ids, &known) {
        return Ok(());
    }
    request(
        "PUT",
        &format!("/api/providers/{}/models", provider.id),
        Some(json!({"models": &ids, "availableModels": &known})),
    )
    .map(|_| ())
}

// magpie is magpie's provider among Alma's: the one named magpie, or an
// OpenAI-shaped one at the gateway. None if there is none.
fn magpie(list: &[Provider]) -> Option<&Provider> {
    let shape = |provider: &Provider| provider.kind == "openai" || provider.kind == "custom";
    list.iter()
        .find(|provider| provider.name == MAGPIE_ID && shape(provider))
        .or_else(|| {
            list.iter().find(|provider| {
                shape(provider)
                    && !provider.base_url.is_empty()
                    && super::applied::same_host(&provider.base_url, &crate::gateway::v1_url())
            })
        })
}

// same_models reports whether Alma already offers these models, in this
// order, each under its name.
fn same_models(provider: &Provider, ids: &[String], known: &[Value]) -> bool {
    provider.models == ids
        && provider.known.len() == known.len()
        && provider.known.iter().zip(known.iter()).all(|(have, want)| {
            have.get("id") == want.get("id") && have.get("name") == want.get("name")
        })
}

// settings reads Alma's whole settings, every key kept as it is.
fn settings() -> Result<Map<String, Value>> {
    let Some(reply) = request("GET", "/api/settings", None)? else {
        return Ok(Map::new());
    };
    Ok(
        match serde_json::from_value::<Option<Map<String, Value>>>(reply) {
            Ok(Some(map)) => map,
            _ => Map::new(),
        },
    )
}

// almaProvider is what magpie reads of one of Alma's providers.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Provider {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "baseURL")]
    base_url: String,
    enabled: bool,
    models: Vec<String>,
    #[serde(rename = "availableModels")]
    known: Vec<Value>,
}

fn providers() -> Result<Vec<Provider>> {
    let Some(reply) = request("GET", "/api/providers", None)? else {
        return Ok(Vec::new());
    };
    Ok(
        match serde_json::from_value::<Option<Vec<Provider>>>(reply) {
            Ok(list) => list.unwrap_or_default(),
            Err(_) => Vec::new(),
        },
    )
}

// request sends one request to Alma's API and decodes its reply. It runs on
// its own thread with its own runtime: callers may be anywhere, including
// inside the gateway's async runtime.
fn request(method: &str, path: &str, body: Option<Value>) -> Result<Option<Value>> {
    let base = api();
    if base.is_empty() {
        bail!("{DOWN}");
    }
    let method = method.to_owned();
    let path = path.to_owned();
    std::thread::spawn(move || -> Result<Option<Value>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build HTTP runtime")?;
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .context("build HTTP client")?;
            let url = format!("{}{}", base.trim_end_matches('/'), path);
            let send = match method.as_str() {
                "GET" => client.get(&url),
                "POST" => client.post(&url),
                "PUT" => client.put(&url),
                "DELETE" => client.delete(&url),
                other => bail!("Alma: unsupported request {other:?}"),
            };
            let send = match &body {
                Some(body) => send.json(body),
                None => send,
            };
            let response = send.send().await.map_err(|_| anyhow::anyhow!("{DOWN}"))?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                let message = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|reply| {
                        reply
                            .get("error")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| text.trim().to_owned());
                bail!("Alma: {} {}: {} {}", method, path, status.as_u16(), message);
            }
            if text.trim().is_empty() {
                return Ok(None);
            }
            serde_json::from_str(&text)
                .with_context(|| format!("parse Alma's reply to {method} {path}"))
                .map(Some)
        })
    })
    .join()
    .map_err(|_| anyhow::anyhow!("{DOWN}"))?
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use super::*;

    // fakeAlma is Alma's API as its spec has it: providers and settings in
    // memory, and every write counted.
    struct Fake {
        providers: Vec<Value>,
        settings: Value,
        keys: Vec<String>,
        writes: Vec<String>,
        next: usize,
    }

    // server serves a fake Alma, with a provider of the user's and settings
    // magpie doesn't know; dropping it takes Alma down.
    struct Server {
        state: Arc<Mutex<Fake>>,
        base: String,
        stop: Arc<AtomicBool>,
    }

    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let state = Arc::new(Mutex::new(Fake {
                providers: vec![json!({
                    "id": "own", "name": "My OpenAI", "type": "openai",
                    "baseURL": "https://api.openai.com/v1", "enabled": true,
                    "apiKey": "encrypted", "models": ["gpt-4o"],
                    "availableModels": [
                        {"id": "gpt-4o", "name": "GPT-4o", "capabilities": {"vision": true}}],
                })],
                settings: json!({
                    "general": {"theme": "dark", "language": "en"},
                    "chat": {"defaultModel": "own:gpt-4o", "temperature": 0.7},
                    "memory": {"enabled": true},
                }),
                keys: Vec::new(),
                writes: Vec::new(),
                next: 0,
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let server_state = Arc::clone(&state);
            let server_stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                loop {
                    if server_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let state = Arc::clone(&server_state);
                            std::thread::spawn(move || serve(&state, stream));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                state,
                base: format!("http://127.0.0.1:{port}"),
                stop,
            }
        }

        // repoint is something else changing a provider's baseURL in Alma.
        fn repoint(&self, from: &str, to: &str) {
            let mut fake = self.state.lock().unwrap();
            for provider in &mut fake.providers {
                if let Some(base) = provider.get("baseURL").and_then(Value::as_str)
                    && base.contains(from)
                {
                    provider["baseURL"] = json!(base.replace(from, to));
                }
            }
        }

        fn take_writes(&self) -> Vec<String> {
            std::mem::take(&mut self.state.lock().unwrap().writes)
        }

        fn take_keys(&self) -> Vec<String> {
            std::mem::take(&mut self.state.lock().unwrap().keys)
        }

        fn provider(&self, name: &str) -> Option<Value> {
            self.state
                .lock()
                .unwrap()
                .providers
                .iter()
                .find(|provider| provider["name"] == json!(name))
                .cloned()
        }

        fn settings(&self) -> Value {
            self.state.lock().unwrap().settings.clone()
        }

        fn count(&self) -> usize {
            self.state.lock().unwrap().providers.len()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn serve(state: &Arc<Mutex<Fake>>, stream: TcpStream) {
        let Ok(reader) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader);
        let mut stream = stream;
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return;
        };
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let (code, reply) = handle(state, method, target, body);
        let payload = if code == 204 {
            String::new()
        } else {
            serde_json::to_string(&reply).unwrap()
        };
        let status = match code {
            200 => "200 OK",
            201 => "201 Created",
            _ => "204 No Content",
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
    }

    fn handle(state: &Arc<Mutex<Fake>>, method: &str, target: &str, body: Value) -> (u16, Value) {
        let path = target.split('?').next().unwrap_or(target);
        let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
        let mut fake = state.lock().unwrap();
        if method != "GET" {
            fake.writes.push(format!("{method} {path}"));
        }
        match (method, path, parts.as_slice()) {
            ("GET", "/api/providers", _) => (200, Value::Array(fake.providers.clone())),
            ("POST", "/api/providers", _) => {
                fake.next += 1;
                let mut made = body.as_object().cloned().unwrap_or_default();
                if let Some(key) = made.get("apiKey").and_then(Value::as_str) {
                    fake.keys.push(key.to_owned());
                }
                made.insert("id".to_owned(), json!(format!("p{}", fake.next)));
                made.insert("models".to_owned(), json!([]));
                made.insert("availableModels".to_owned(), json!([]));
                made.insert("apiKey".to_owned(), json!("encrypted"));
                let made = Value::Object(made);
                fake.providers.push(made.clone());
                (201, made)
            }
            ("PUT", _, ["api", "providers", id]) => {
                let mut made = Value::Null;
                if let Some(fields) = body.as_object() {
                    if let Some(key) = fields.get("apiKey").and_then(Value::as_str) {
                        fake.keys.push(key.to_owned());
                    }
                    for provider in &mut fake.providers {
                        if provider["id"] == json!(id) {
                            for (key, value) in fields {
                                if key != "apiKey" {
                                    provider[key.as_str()] = value.clone();
                                }
                            }
                            made = provider.clone();
                        }
                    }
                }
                (200, made)
            }
            ("DELETE", _, ["api", "providers", id]) => {
                fake.providers
                    .retain(|provider| provider["id"] != json!(id));
                (204, Value::Null)
            }
            ("PUT", _, ["api", "providers", id, "models"]) => {
                let mut made = Value::Null;
                for provider in &mut fake.providers {
                    if provider["id"] == json!(id) {
                        // as Alma: models kept as given, availableModels
                        // without what capabilities were sent
                        provider["models"] = body["models"].clone();
                        if let Some(available) = body["availableModels"].as_array() {
                            let available = available
                                .iter()
                                .map(|model| {
                                    let mut model = model.as_object().cloned().unwrap_or_default();
                                    model.remove("capabilities");
                                    Value::Object(model)
                                })
                                .collect::<Vec<_>>();
                            provider["availableModels"] = json!(available);
                        }
                        made = provider.clone();
                    }
                }
                (200, made)
            }
            ("GET", "/api/settings", _) => (200, fake.settings.clone()),
            ("PUT", "/api/settings", _) => {
                fake.settings = body.clone();
                (200, body)
            }
            ("GET", "/api/models", _) => {
                let mut models = Vec::new();
                for provider in &fake.providers {
                    for model in provider["models"].as_array().cloned().unwrap_or_default() {
                        models.push(json!({
                            "id": format!(
                                "{}:{}",
                                provider["id"].as_str().unwrap_or_default(),
                                model.as_str().unwrap_or_default()
                            ),
                            "name": model,
                            "provider": provider["name"],
                            "providerId": provider["id"],
                        }));
                    }
                }
                (200, Value::Array(models))
            }
            _ => (200, Value::Null),
        }
    }

    fn agent() -> crate::agent::Agent {
        crate::agent::Agent {
            spec: crate::agent::ALL_AGENTS
                .iter()
                .find(|spec| spec.id == "alma")
                .unwrap(),
            path: dir(),
        }
    }

    #[test]
    fn alma_models_come_and_go() {
        let _home = crate::agent::testing::isolation();
        let server = Server::start();
        crate::agent::testing::set_alma_api(&server.base);
        let alma = agent();
        std::fs::create_dir_all(dir()).unwrap();
        assert!(alma.is_detected());
        assert_eq!(
            alma.values().unwrap(),
            vec![("model", "own:gpt-4o".to_owned())]
        );

        // nothing of magpie's in Alma: sync adds nothing
        alma.sync().unwrap();
        assert!(server.take_writes().is_empty());

        alma.apply("model", "magpie/deepseek/pro").unwrap();
        let mine = server.provider("magpie").expect("magpie's provider");
        assert_eq!(mine["type"], json!("openai"));
        assert_eq!(mine["baseURL"], json!(crate::gateway::v1_url()));
        assert_eq!(mine["enabled"], json!(true));
        assert_eq!(server.take_keys(), ["magpie"]);
        let models = mine["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], json!("deepseek/pro"));
        let available = mine["availableModels"].as_array().unwrap();
        assert_eq!(available.len(), models.len());
        assert_eq!(available[0]["id"], json!("deepseek/pro"));
        assert!(!available[0]["name"].as_str().unwrap().is_empty());
        let settings = server.settings();
        let id = mine["id"].as_str().unwrap();
        assert_eq!(
            settings["chat"]["defaultModel"],
            json!(format!("{id}:deepseek/pro"))
        );
        assert_eq!(settings["chat"]["temperature"], json!(0.7));
        assert_eq!(settings["general"]["theme"], json!("dark"));
        assert!(settings["memory"].is_object());
        assert_eq!(
            alma.values().unwrap(),
            vec![("model", "magpie/deepseek/pro".to_owned())]
        );
        // the user's own provider is as it was
        assert!(server.provider("My OpenAI").is_none());
        assert!(alma.drift().is_none());
        server.take_writes();

        // nothing changed: nothing written
        alma.sync().unwrap();
        assert!(server.take_writes().is_empty());
        alma.apply("model", "magpie/deepseek/pro").unwrap();
        assert!(server.take_writes().is_empty());

        // a model gone from the catalog: sync puts the list right
        {
            let mut fake = server.state.lock().unwrap();
            let at = fake
                .providers
                .iter()
                .position(|provider| provider["name"] == json!("magpie"))
                .unwrap();
            fake.providers[at]["models"] = json!(["old"]);
        }
        alma.sync().unwrap();
        assert_eq!(
            server.take_writes(),
            [format!("PUT /api/providers/{id}/models")]
        );
        assert_eq!(
            server.provider("magpie").unwrap()["models"][0],
            json!("deepseek/pro")
        );

        // Alma's own model again: the default is Alma's, magpie stays
        alma.apply("model", "own:gpt-4o").unwrap();
        assert_eq!(
            server.settings()["chat"]["defaultModel"],
            json!("own:gpt-4o")
        );
        assert!(server.provider("magpie").is_some());

        // the gateway moved: choosing magpie's model points it back
        server.repoint(&crate::gateway::url(), "http://127.0.0.1:9");
        alma.apply("model", "magpie/deepseek/pro").unwrap();
        assert_eq!(server.count(), 2);
        assert_eq!(
            server.provider("magpie").unwrap()["baseURL"],
            json!(crate::gateway::v1_url())
        );

        // back to Alma's default: magpie's provider comes out, the user's
        // stays
        alma.apply("model", "").unwrap();
        assert_eq!(server.count(), 1);
        assert_eq!(server.settings()["chat"]["defaultModel"], json!(""));
    }

    // Alma not running is no error for sync, and no drift: it only can't be
    // set, and what magpie set last is what reads.
    #[test]
    fn alma_down_is_no_error() {
        let _home = crate::agent::testing::isolation();
        let server = Server::start();
        crate::agent::testing::set_alma_api(&server.base);
        let alma = agent();
        std::fs::create_dir_all(dir()).unwrap();
        alma.apply("model", "magpie/deepseek/pro").unwrap();
        drop(server);
        crate::agent::testing::set_alma_api("http://127.0.0.1:1");

        alma.sync().unwrap();
        assert_eq!(
            alma.values().unwrap(),
            vec![("model", "magpie/deepseek/pro".to_owned())]
        );
        assert!(alma.drift().is_none());
        let error = alma.set("model", "magpie/deepseek/pro").unwrap_err();
        assert!(error.to_string().contains("isn't running"), "{error}");
    }
}
