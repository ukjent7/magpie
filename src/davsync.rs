// davsync keeps magpie's setup the same on every computer through a WebDAV
// folder. What goes there is a backup, sealed with a passphrase before it
// leaves the computer, so the server only ever holds a file it can't read.
//
// The magpie serving the gateway syncs every few minutes. The setup is
// taken in four parts: providers (with their pictures and groups), settings,
// profiles and the agents' models. A part changed only here is pushed; one
// changed only on the server is brought in; one changed on both since the
// last sync keeps the newer, and the copy it replaced is saved in the sync
// folder beside magpie's files and named in a notice. What the server holds
// is mirrored: a provider removed on one computer goes from the others.

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Client, Method, Response, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::Mutex;
use url::Url;

use crate::{agent, backup, config, profile, provider, settings};

// folder and file are where the backup lives under the address given.
const FOLDER: &str = "magpie";
const FILE: &str = "magpie.magpie-backup";

// EVERY is how often the gateway's magpie syncs; FIRST the grace before the
// first try, so starting the app isn't held up; ONE_ROUND all a single sync
// may take, however far the server is.
pub const EVERY: Duration = Duration::from_secs(3 * 60);
const FIRST: Duration = Duration::from_secs(20);
const ONE_ROUND: Duration = Duration::from_secs(2 * 60);

// PARTS, in the order they are told.
pub const PARTS: [&str; 4] = ["providers", "settings", "profiles", "agents"];

// The file on the server is one sealed backup; this is well past any size
// magpie writes, and what a mistaken address must not be read into memory.
const MAX_REMOTE_BYTES: usize = 64 << 20;

static CHANGES: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

// Changed is a PUT the server refused because the file changed since it was
// read: another computer synced in between.
#[derive(Debug)]
pub struct Changed;

impl fmt::Display for Changed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the file on the server changed meanwhile")
    }
}

impl std::error::Error for Changed {}

// Config is the sync's setup, kept in sync.json beside magpie's other files,
// readable by the user alone, as provider keys are.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password: String,
    pub passphrase: String,
    // keys: providers carry their API keys
    pub keys: bool,
    // agents: the agents' models go too
    pub agents: bool,
}

// Notice says what a sync replaced when a part had changed on both sides, or
// when this computer first joined a folder with a setup in it.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Notice {
    pub at: String,
    // here: this computer's parts, replaced by the server's
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub here: Vec<String>,
    // there: the server's parts, replaced by this computer's
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub there: Vec<String>,
    // saved: the folder the replaced copies are kept in
    #[serde(skip_serializing_if = "String::is_empty")]
    pub saved: String,
}

// View is sync as the Settings page shows it: never the secrets.
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub on: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(skip_serializing_if = "is_false")]
    pub password_set: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub passphrase_set: bool,
    pub keys: bool,
    pub agents: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub notice: Option<Notice>,
}

// state is what the last sync saw: the parts here and on the server, by
// hash, and the server's file.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct State {
    // key names the address, user and passphrase it was for
    key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    last: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
    notice: Option<Notice>,
    // sum is the server's file, hashed
    #[serde(skip_serializing_if = "String::is_empty")]
    sum: String,
    local: BTreeMap<String, String>,
    remote: BTreeMap<String, String>,
}

fn file(name: &str) -> PathBuf {
    settings::dir().join(name)
}

// load reads the setup; nothing when sync is off.
pub fn load() -> Option<Config> {
    let contents = fs::read_to_string(file("sync.json")).ok()?;
    let config: Config = serde_json::from_str(&contents).ok()?;
    (!config.url.is_empty()).then_some(config)
}

// configure turns sync on, or changes it. A password or passphrase left
// empty keeps the one set before.
pub fn configure(mut config: Config) -> Result<()> {
    config.url = config.url.trim().to_owned();
    config.user = config.user.trim().to_owned();
    Dav::new(&config).context("check the WebDAV address")?;
    if let Some(old) = load() {
        if config.password.is_empty() {
            config.password.clone_from(&old.password);
        }
        if config.passphrase.is_empty() {
            config.passphrase.clone_from(&old.passphrase);
        }
    }
    ensure!(
        !config.passphrase.is_empty(),
        "sync needs a passphrase: the file is sealed with it before it leaves this computer"
    );
    ensure!(
        config.password.is_empty() || config.passphrase != config.password,
        // the server is sent the password: with it, it could open the file
        "the passphrase is the server's password: the server is sent the password, and could open \
         the file with it. Pick a passphrase of its own"
    );
    let bytes = serde_json::to_vec_pretty(&config).context("write sync setup")?;
    create_dir(settings::dir())?;
    config::atomic_write_secret_for_settings(&file("sync.json"), &bytes)
}

// off turns sync off. The file on the server stays.
pub async fn off() -> Result<()> {
    let _guard = CHANGES.lock().await;
    let _ = fs::remove_file(file("sync-state.json"));
    match fs::remove_file(file("sync.json")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("turn sync off"),
    }
}

// status is sync's setup and how the last sync went.
pub fn status() -> View {
    let Some(config) = load() else {
        return View {
            keys: true,
            agents: true,
            ..View::default()
        };
    };
    let state = load_state();
    View {
        on: true,
        url: config.url.clone(),
        user: config.user.clone(),
        password_set: !config.password.is_empty(),
        passphrase_set: !config.passphrase.is_empty(),
        keys: config.keys,
        agents: config.agents,
        last: (state.key == state_key(&config))
            .then(|| state.last.clone())
            .unwrap_or_default(),
        error: state.error.clone(),
        notice: state.notice.clone(),
    }
}

// dismiss clears the notice.
pub async fn dismiss() -> Result<()> {
    let _guard = CHANGES.lock().await;
    let mut state = load_state();
    state.notice = None;
    save_state(&state)
}

fn load_state() -> State {
    fs::read_to_string(file("sync-state.json"))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save_state(state: &State) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("write sync state")?;
    create_dir(settings::dir())?;
    config::atomic_write_for_settings(&file("sync-state.json"), &bytes)
}

fn create_dir(dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))
}

fn state_key(config: &Config) -> String {
    sum(format!("{}\0{}\0{}", config.url, config.user, config.passphrase).as_bytes())
}

fn sum(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn is_false(value: &bool) -> bool {
    !value
}

// empty_object stands for a part a bundle holds nothing of: hashed as an
// empty one, so an absent part and an empty one are the same.
fn empty_object() -> Value {
    json!({})
}

// now syncs once; nothing when sync is off.
pub async fn now() -> Result<()> {
    let _guard = CHANGES.lock().await;
    let Some(config) = load() else {
        return Ok(());
    };
    let key = state_key(&config);
    let mut state = load_state();
    if state.key != key {
        // another folder or passphrase: start afresh
        state = State {
            key,
            ..State::default()
        };
    }
    let mut outcome = sync_once(&config, &mut state).await;
    if matches!(&outcome, Err(error) if error.is::<Changed>()) {
        // another computer got in between: again, over its version
        outcome = sync_once(&config, &mut state).await;
    }
    match &outcome {
        Ok(()) => {
            state.error.clear();
            state.last = stamp();
        }
        Err(error) => state.error = error.to_string(),
    }
    save_state(&state)?;
    outcome
}

// run syncs a little after it starts and every EVERY after that, until the
// task is dropped with the gateway it served. A failure is logged once, not
// on every try.
pub async fn run() {
    let mut waited = FIRST;
    let mut logged = String::new();
    loop {
        tokio::time::sleep(waited).await;
        let outcome = match tokio::time::timeout(ONE_ROUND, now()).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("the WebDAV server did not answer in time")),
        };
        match outcome {
            Ok(()) => logged.clear(),
            Err(error) => {
                let message = error.to_string();
                if message != logged {
                    eprintln!("magpie: syncing through WebDAV: {message}");
                    logged = message;
                }
            }
        }
        waited = EVERY;
    }
}

// ---- the WebDAV file -------------------------------------------------------

struct Dav {
    base: Url,
    user: String,
    password: String,
    client: Client,
}

impl Dav {
    fn new(config: &Config) -> Result<Dav> {
        let address = config.url.trim();
        let mut base = match Url::parse(address) {
            Ok(url) if matches!(url.scheme(), "https" | "http") && url.host_str().is_some() => url,
            _ => bail!("{address:?} is not a WebDAV address (https://…)"),
        };
        // the folder and file are put under the path as it was given
        let trimmed = base.path().trim_end_matches('/').to_owned();
        base.set_path(&trimmed);
        Ok(Dav {
            base,
            user: config.user.clone(),
            password: config.password.clone(),
            client: Client::builder()
                .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("create WebDAV client")?,
        })
    }

    fn url(&self, parts: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        let path = format!(
            "{}/{}",
            self.base.path().trim_end_matches('/'),
            parts.join("/")
        );
        url.set_path(&path);
        Ok(url)
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(header::HeaderName, String)],
    ) -> Result<Response> {
        let mut request = self.client.request(method, url.clone());
        if !self.user.is_empty() || !self.password.is_empty() {
            request = request.basic_auth(&self.user, Some(&self.password));
        }
        for (name, value) in headers {
            request = request.header(name.clone(), value.clone());
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("ask the WebDAV server at {url}"))?;
        if response.status() == StatusCode::UNAUTHORIZED
            || response.status() == StatusCode::FORBIDDEN
        {
            bail!(
                "the WebDAV server refused the user name or password (HTTP {})",
                response.status().as_u16()
            );
        }
        Ok(response)
    }

    // get reads the backup; nothing when there is none yet.
    async fn get(&self) -> Result<(Option<Vec<u8>>, String)> {
        let response = self
            .send(Method::GET, self.url(&[FOLDER, FILE])?, None, &[])
            .await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            return Ok((None, String::new()));
        }
        ensure!(
            status == StatusCode::OK,
            "reading {FILE} from the WebDAV server: HTTP {}",
            status.as_u16()
        );
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response.bytes().await.context("read the backup")?;
        ensure!(
            bytes.len() <= MAX_REMOTE_BYTES,
            "the file on the WebDAV server is too big to be magpie's backup"
        );
        Ok((Some(bytes.to_vec()), etag))
    }

    // put writes the backup, only over the version read (etag) when there
    // was one, making the folder if the server has none.
    async fn put(&self, data: &[u8], etag: &str) -> Result<()> {
        let url = self.url(&[FOLDER, FILE])?;
        let mut headers = vec![(header::CONTENT_TYPE, "application/octet-stream".to_owned())];
        if !etag.is_empty() {
            headers.push((header::IF_MATCH, etag.to_owned()));
        }
        let mut made_folder = false;
        loop {
            let status = self
                .send(Method::PUT, url.clone(), Some(data.to_vec()), &headers)
                .await?
                .status();
            if status.is_success() {
                return Ok(());
            }
            if status == StatusCode::PRECONDITION_FAILED {
                return Err(anyhow::Error::new(Changed));
            }
            if !made_folder && (status == StatusCode::NOT_FOUND || status == StatusCode::CONFLICT) {
                made_folder = true;
                self.mkcol().await?;
                continue;
            }
            bail!(
                "writing {FILE} to the WebDAV server: HTTP {}",
                status.as_u16()
            );
        }
    }

    async fn mkcol(&self) -> Result<()> {
        let method = Method::from_bytes(b"MKCOL").context("name the MKCOL request")?;
        let status = self
            .send(method, self.url(&[FOLDER])?, None, &[])
            .await?
            .status();
        // 405: the folder is there already
        ensure!(
            status.is_success() || status == StatusCode::METHOD_NOT_ALLOWED,
            "making the folder {FOLDER} on the WebDAV server: HTTP {}",
            status.as_u16()
        );
        Ok(())
    }
}

// ---- the four parts --------------------------------------------------------

// collect is what this computer would send: its setup, sealed with its keys
// only when told to, and its agents' models only when told to.
fn collect(config: &Config) -> Result<backup::Bundle> {
    let mut bundle = backup::collect(config.keys, "magpie")?;
    if bundle.settings.is_none() {
        bundle.settings = Some(settings::load());
    }
    if !config.agents {
        bundle.agents.clear();
    }
    Ok(bundle)
}

// hashes is each part of a bundle, hashed: what is compared to tell a
// change.
fn hashes(bundle: &backup::Bundle) -> Result<BTreeMap<String, String>> {
    // the parts are hashed as the bundle writes them, so that a picture and
    // a key hash the same on every computer.
    let mut value = serde_json::to_value(bundle).context("read the setup apart to hash it")?;
    if let Some(own) = value.get_mut("settings").and_then(Value::as_object_mut) {
        // this computer's own: never synced
        own.remove("proxy");
        own.remove("window");
        own.remove("dock");
    }
    let hash = |piece: Value| -> Result<String> {
        Ok(sum(
            &serde_json::to_vec(&piece).context("hash a part of the setup")?
        ))
    };
    Ok(BTreeMap::from([
        (
            "providers".to_owned(),
            hash(json!([
                value.get("providers").cloned().unwrap_or_else(empty_object),
                value.get("icons").cloned().unwrap_or_else(empty_object),
                value.get("groups").cloned().unwrap_or_else(empty_object)
            ]))?,
        ),
        (
            "settings".to_owned(),
            hash(value.get("settings").cloned().unwrap_or_else(empty_object))?,
        ),
        (
            "profiles".to_owned(),
            hash(value.get("profiles").cloned().unwrap_or_else(empty_object))?,
        ),
        (
            "agents".to_owned(),
            hash(value.get("agents").cloned().unwrap_or_else(empty_object))?,
        ),
    ]))
}

// take puts local's part in merged.
fn take(merged: &mut backup::Bundle, local: &backup::Bundle, part: &str) {
    match part {
        "providers" => {
            // a setup sent without keys keeps the ones the server has
            merged.providers = if local.keys || !merged.keys {
                local.providers.clone()
            } else {
                provider::with_saved_keys(&local.providers, &merged.providers)
            };
            merged.icons.clone_from(&local.icons);
            merged.groups.clone_from(&local.groups);
            merged.keys |= local.keys;
        }
        "settings" => merged.settings.clone_from(&local.settings),
        "profiles" => merged.profiles.clone_from(&local.profiles),
        "agents" => merged.agents.clone_from(&local.agents),
        _ => {}
    }
}

// bring puts the server's part in here: providers and profiles mirrored, so
// what went elsewhere goes here too.
fn bring(bundle: &backup::Bundle, part: &str) -> Result<()> {
    match part {
        "providers" => {
            provider::restore_backup_icons(&bundle.icons)?;
            provider::mirror_backup(&bundle.providers, &bundle.groups)
        }
        "settings" => backup::restore(
            bundle,
            backup::Parts {
                settings: true,
                ..Default::default()
            },
        )
        .map(|_| ()),
        "profiles" => {
            for (name, _) in profile::list_entries()? {
                if !bundle.profiles.contains_key(&name) {
                    profile::delete_named(&name)?;
                }
            }
            backup::restore(
                bundle,
                backup::Parts {
                    profiles: true,
                    ..Default::default()
                },
            )
            .map(|_| ())
        }
        "agents" => backup::restore(
            bundle,
            backup::Parts {
                agents: true,
                ..Default::default()
            },
        )
        .map(|_| ()),
        _ => Ok(()),
    }
}

// changed is when a part was last changed here, as its files say.
fn changed(part: &str) -> Option<OffsetDateTime> {
    fn mtime(path: &Path) -> Option<OffsetDateTime> {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| OffsetDateTime::try_from(modified).ok())
    }
    match part {
        "providers" => mtime(&settings::providers_path()),
        "settings" => mtime(&settings::path()),
        "profiles" => mtime(&settings::profiles_path()),
        _ => agent::all()
            .iter()
            .filter(|current| current.is_detected())
            .filter_map(|current| mtime(&current.path))
            .max(),
    }
}

fn stamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn parse(stored: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(stored, &Rfc3339).ok()
}

// kept is the folder the copies a sync replaced are saved in.
fn kept_dir() -> Result<PathBuf> {
    let dir = file("sync");
    create_dir(dir.clone())?;
    Ok(dir)
}

fn folder_stamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:04}-{:02}-{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

// ---- one round -------------------------------------------------------------

async fn sync_once(config: &Config, state: &mut State) -> Result<()> {
    let dav = Dav::new(config)?;
    let (data, etag) = dav.get().await?;
    let local = collect(config)?;
    let here = hashes(&local)?;

    let Some(data) = data else {
        // nothing there yet: this computer's setup is the first
        push(&dav, config, state, &local, String::new()).await?;
        state.local = here;
        return Ok(());
    };
    if sum(&data) == state.sum && here == state.local {
        return Ok(()); // nothing changed on either side
    }
    let remote = match backup::open(&data, &config.passphrase) {
        Ok(bundle) => bundle,
        Err(error) if error.is::<backup::WrongPassphrase>() => bail!(
            "the passphrase doesn't open the file on the server: it was sealed with another one; \
             use the passphrase set on your other computers"
        ),
        Err(error) => return Err(error),
    };
    let there = hashes(&remote)?;
    let first = state.local.is_empty();
    let mut merged = remote.clone();
    let mut bring_in: Vec<&str> = Vec::new();
    let mut replaced_here: Vec<String> = Vec::new();
    let mut replaced_there: Vec<String> = Vec::new();
    let on_server = parse(&remote.created);

    for part in PARTS {
        if here[part] == there[part] || (part == "agents" && !config.agents) {
            continue;
        }
        let changed_here = !first && here[part] != stored(&state.local, part);
        let changed_there = first || there[part] != stored(&state.remote, part);
        match (changed_here, changed_there) {
            (true, false) => take(&mut merged, &local, part),
            (false, true) => {
                bring_in.push(part);
                if first {
                    replaced_here.push(part.to_owned());
                }
            }
            // both: the newer stays
            (true, true) if changed(part) > on_server => {
                take(&mut merged, &local, part);
                replaced_there.push(part.to_owned());
            }
            (true, true) => {
                bring_in.push(part);
                replaced_here.push(part.to_owned());
            }
            (false, false) => {}
        }
    }

    let mut saved = String::new();
    if !(replaced_here.is_empty() && replaced_there.is_empty()) {
        // the copies are a courtesy: a sync that can't keep them goes ahead
        let dir = kept_dir()?;
        let stamp = folder_stamp();
        if !replaced_here.is_empty() {
            if let Ok(sealed) = backup::seal(&local, &config.passphrase) {
                let _ = config::atomic_write_secret_for_settings(
                    &dir.join(format!("{stamp}-this-computer{}", backup::EXTENSION)),
                    &sealed,
                );
            }
        }
        if !replaced_there.is_empty() {
            let _ = config::atomic_write_secret_for_settings(
                &dir.join(format!("{stamp}-server{}", backup::EXTENSION)),
                &data,
            );
        }
        saved = dir.display().to_string();
    }

    for part in &bring_in {
        bring(&remote, part).with_context(|| format!("bringing in the {part}"))?;
    }

    // the server's file is in; what was brought in is this computer's own
    // now, and counts as changed here until the push is done
    let now = if bring_in.is_empty() {
        here
    } else {
        hashes(&collect(config)?)?
    };
    let merged_hashes = hashes(&merged)?;
    let mut pending = BTreeMap::new();
    for part in PARTS {
        pending.insert(
            part.to_owned(),
            if merged_hashes[part] != there[part] && !first {
                stored(&state.local, part)
            } else {
                now[part].clone()
            },
        );
    }
    state.local = pending;
    state.sum = sum(&data);
    state.remote = there.clone();
    if !(replaced_here.is_empty() && replaced_there.is_empty()) {
        state.notice = Some(Notice {
            at: stamp(),
            here: replaced_here,
            there: replaced_there,
            saved,
        });
    }
    if merged_hashes != there {
        push(&dav, config, state, &merged, etag).await?;
    }
    state.local = now;
    Ok(())
}

// push seals a bundle and writes it over the version read, and remembers
// what it sent.
async fn push(
    dav: &Dav,
    config: &Config,
    state: &mut State,
    bundle: &backup::Bundle,
    etag: String,
) -> Result<()> {
    let mut bundle = bundle.clone();
    bundle.created = stamp();
    bundle.app = "magpie".to_owned();
    let sealed = backup::seal(&bundle, &config.passphrase)?;
    dav.put(&sealed, &etag).await?;
    state.sum = sum(&sealed);
    state.remote = hashes(&bundle)?;
    Ok(())
}

fn stored(part: &BTreeMap<String, String>, name: &str) -> String {
    part.get(name).cloned().unwrap_or_default()
}
