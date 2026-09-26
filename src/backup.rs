use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, IsTerminal},
    path::Path,
};

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use pbkdf2::pbkdf2_hmac;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Sha256;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{agent, profile, provider, settings};

pub const EXTENSION: &str = ".magpie-backup";

const FORMAT: &str = "magpie-backup";
const ENVELOPE_VERSION: u32 = 1;
const KDF: &str = "pbkdf2-sha256";
const ITERATIONS: u32 = 600_000;
const KEY_BYTES: usize = 32;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;
const MIN_ITERATIONS: u32 = 100_000;
const MAX_ITERATIONS: u32 = 10_000_000;
const WRONG_PASSPHRASE: &str = "wrong passphrase, or the file was changed";

// WrongPassphrase marks the one failure that is not a broken file: a
// passphrase that doesn't open it. Sync tells the user that in its own
// words, since the file on the server is fine and only this computer has
// the wrong passphrase for it.
#[derive(Debug)]
pub struct WrongPassphrase;

impl std::fmt::Display for WrongPassphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(WRONG_PASSPHRASE)
    }
}

impl std::error::Error for WrongPassphrase {}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Bundle {
    pub(crate) version: u32,
    pub(crate) created: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) app: String,
    pub(crate) keys: bool,
    pub(crate) providers: Vec<provider::Provider>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", with = "base64_map")]
    pub(crate) icons: BTreeMap<String, Vec<u8>>,
    // groups are the user's model groups, the routing groups they made.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) groups: Vec<provider::Group>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) settings: Option<settings::Settings>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) profiles: profile::Profiles,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) agents: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
struct Envelope {
    format: String,
    version: u32,
    kdf: String,
    iterations: u32,
    #[serde(with = "base64_bytes")]
    salt: Vec<u8>,
    #[serde(with = "base64_bytes")]
    nonce: Vec<u8>,
    #[serde(with = "base64_bytes")]
    data: Vec<u8>,
}

// Parts picks what a restore puts back.
#[derive(Clone, Copy, Default)]
pub(crate) struct Parts {
    pub(crate) providers: bool,
    pub(crate) settings: bool,
    pub(crate) profiles: bool,
    pub(crate) agents: bool,
}

// ALL is every part.
pub(crate) const ALL: Parts = Parts {
    providers: true,
    settings: true,
    profiles: true,
    agents: true,
};

// RestoreResult says what a restore did.
pub(crate) struct RestoreResult {
    pub(crate) added: usize,
    pub(crate) replaced: usize,
    // need_key lists the providers that came without a key and have none here.
    pub(crate) need_key: Vec<String>,
    pub(crate) settings: bool,
    pub(crate) profiles: usize,
    pub(crate) agents: usize,
    // skipped are the agent fields left alone: the agent is not on this
    // machine, or the value could not be written.
    pub(crate) skipped: Vec<String>,
}

pub fn backup_command(args: &[String]) -> Result<()> {
    let mut include_keys = true;
    let mut destination = None;
    for argument in args {
        match argument.as_str() {
            "--no-keys" => include_keys = false,
            value if value.starts_with('-') => {
                bail!("unknown flag {value} (magpie backup [--no-keys] [file])")
            }
            _ if destination.is_none() => destination = Some(argument.as_str()),
            _ => bail!("usage: magpie backup [--no-keys] [file]"),
        }
    }

    let destination = destination.unwrap_or("magpie.magpie-backup");
    let bundle = collect(include_keys, crate::VERSION)?;
    let passphrase = read_passphrase("Passphrase for the backup: ", true)?;
    let data = seal(&bundle, &passphrase)?;
    crate::config::atomic_write_secret_for_settings(Path::new(destination), &data)
        .with_context(|| format!("write backup {}", Path::new(destination).display()))?;

    let key_note = if include_keys {
        "with their keys"
    } else {
        "without keys"
    };
    println!(
        "saved {}: {} providers {key_note}, {} profiles, {} agent settings",
        Path::new(destination).display(),
        bundle.providers.len(),
        bundle.profiles.len(),
        bundle.agents.len()
    );
    println!(
        "Subscriptions are not in it: sign in to them on the other machine. Keep the passphrase: without it the file can't be opened."
    );
    Ok(())
}

pub fn restore_command(args: &[String]) -> Result<()> {
    let mut include_agents = true;
    let mut source = None;
    for argument in args {
        match argument.as_str() {
            "--no-agents" => include_agents = false,
            value if value.starts_with('-') => {
                bail!("unknown flag {value} (magpie restore [--no-agents] <file>)")
            }
            _ if source.is_none() => source = Some(argument.as_str()),
            _ => bail!("usage: magpie restore [--no-agents] <file>"),
        }
    }
    let source = source.context("usage: magpie restore [--no-agents] <file>")?;
    let data =
        fs::read(source).with_context(|| format!("read backup {}", Path::new(source).display()))?;
    let passphrase = read_passphrase("Passphrase: ", false)?;
    let bundle = open(&data, &passphrase)?;
    let result = restore(
        &bundle,
        Parts {
            agents: include_agents,
            ..ALL
        },
    )?;

    println!(
        "restored providers: {} added, {} replaced{}; {} profiles; {} agent settings changed",
        result.added,
        result.replaced,
        if result.settings { "; settings" } else { "" },
        result.profiles,
        result.agents
    );
    if !result.need_key.is_empty() {
        println!(
            "Needs a key (magpie provider key <id>): {}",
            result.need_key.join(", ")
        );
    }
    if !result.skipped.is_empty() {
        println!(
            "Left as they are (agent not here, or its model can't be reached yet): {}",
            result.skipped.join(", ")
        );
    }
    Ok(())
}

// collect gathers the bundle; without keys the providers carry none, nor
// any header that looks like one. app names the magpie that made it.
pub(crate) fn collect(include_keys: bool, app: &str) -> Result<Bundle> {
    let provider::BackupSnapshot {
        providers,
        icons,
        groups,
    } = provider::backup_snapshot(include_keys)?;
    let profiles = profile::backup_entries()?;
    let agents = profile::snapshot_entries()?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let settings = settings::path().is_file().then(settings::load);
    let created = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format backup creation time")?;

    Ok(Bundle {
        version: 1,
        created,
        app: app.to_owned(),
        keys: include_keys,
        providers,
        icons,
        groups,
        settings,
        profiles,
        agents,
    })
}

pub(crate) fn seal(bundle: &Bundle, passphrase: &str) -> Result<Vec<u8>> {
    ensure!(!passphrase.is_empty(), "a backup needs a passphrase");
    let plaintext = serde_json::to_vec(bundle).context("serialize backup contents")?;
    let mut envelope = Envelope {
        format: FORMAT.to_owned(),
        version: ENVELOPE_VERSION,
        kdf: KDF.to_owned(),
        iterations: ITERATIONS,
        salt: vec![0; SALT_BYTES],
        nonce: vec![0; NONCE_BYTES],
        data: Vec::new(),
    };
    getrandom::fill(&mut envelope.salt).context("generate backup salt")?;
    getrandom::fill(&mut envelope.nonce).context("generate backup nonce")?;
    let key = derive_key(passphrase, &envelope.salt, envelope.iterations);
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("invalid backup key"))?;
    let nonce = nonce(&envelope.nonce)?;
    envelope.data = cipher
        .encrypt(
            &nonce,
            aes_gcm::aead::Payload {
                msg: &plaintext,
                aad: &authenticated_header(&envelope),
            },
        )
        .map_err(|_| anyhow::anyhow!("encrypt backup"))?;
    serde_json::to_vec_pretty(&envelope).context("serialize encrypted backup")
}

pub(crate) fn open(data: &[u8], passphrase: &str) -> Result<Bundle> {
    let envelope: Envelope =
        serde_json::from_slice(data).map_err(|_| anyhow::anyhow!("not a magpie backup"))?;
    ensure!(envelope.format == FORMAT, "not a magpie backup");
    ensure!(
        envelope.version == ENVELOPE_VERSION && envelope.kdf == KDF,
        "this backup was made by a newer magpie; update magpie to open it"
    );
    ensure!(
        (MIN_ITERATIONS..=MAX_ITERATIONS).contains(&envelope.iterations)
            && envelope.nonce.len() == NONCE_BYTES
            && envelope.salt.len() >= SALT_BYTES,
        "not a magpie backup"
    );

    let key = derive_key(passphrase, &envelope.salt, envelope.iterations);
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("invalid backup key"))?;
    let nonce = nonce(&envelope.nonce)?;
    let plaintext = cipher
        .decrypt(
            &nonce,
            aes_gcm::aead::Payload {
                msg: &envelope.data,
                aad: &authenticated_header(&envelope),
            },
        )
        .map_err(|_| anyhow::Error::new(WrongPassphrase))?;
    serde_json::from_slice(&plaintext).context("parse backup contents")
}

fn nonce(bytes: &[u8]) -> Result<Nonce<aes_gcm::aead::consts::U12>> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("not a magpie backup"))
}

fn derive_key(passphrase: &str, salt: &[u8], iterations: u32) -> [u8; KEY_BYTES] {
    let mut key = [0; KEY_BYTES];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, iterations, &mut key);
    key
}

fn authenticated_header(envelope: &Envelope) -> Vec<u8> {
    let mut salt_hex = String::with_capacity(envelope.salt.len() * 2);
    for byte in &envelope.salt {
        use std::fmt::Write as _;
        let _ = write!(salt_hex, "{byte:02x}");
    }
    format!(
        "{}/{}/{}/{}/{}",
        envelope.format, envelope.version, envelope.kdf, envelope.iterations, salt_hex
    )
    .into_bytes()
}

// restore puts the chosen parts of the bundle in. Providers come first, so
// the agents' models that go through them resolve. Only agents on this
// machine are set.
pub(crate) fn restore(bundle: &Bundle, parts: Parts) -> Result<RestoreResult> {
    let mut added = 0;
    let mut replaced = 0;
    let mut need_key = Vec::new();
    if parts.providers {
        provider::restore_backup_icons(&bundle.icons)?;
        let result = provider::restore_backup_providers(&bundle.providers)?;
        added = result.0;
        replaced = result.1;
        need_key = result.2;
        provider::restore_backup_groups(&bundle.groups)?;
    }

    let mut settings_restored = false;
    if parts.settings {
        if let Some(saved) = bundle.settings.as_ref() {
            // the proxy and the window are this computer's own
            let here = settings::load();
            let mut saved = saved.clone();
            saved.proxy.clone_from(&here.proxy);
            saved.window.clone_from(&here.window);
            saved.dock = here.dock;
            settings::write_json(&settings::path(), &saved)?;
            settings_restored = true;
        }
    }

    let profiles = if parts.profiles {
        profile::restore_entries(&bundle.profiles)?
    } else {
        0
    };

    let mut agents_changed = 0;
    let mut skipped = Vec::new();
    if parts.agents {
        let mut agents = agent::all();
        agents.sort_by_key(|agent| agent.spec.id);
        let mut entries = bundle.agents.iter().collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| {
            agent_field_rank(left)
                .cmp(&agent_field_rank(right))
                .then_with(|| left.cmp(right))
        });
        for (key, value) in entries {
            let Some((agent_id, field)) = key.split_once('.') else {
                skipped.push(key.clone());
                continue;
            };
            let Some(current) = agents
                .iter()
                .find(|current| current.spec.id == agent_id)
                .filter(|current| current.is_detected())
            else {
                skipped.push(key.clone());
                continue;
            };
            let Ok(values) = current.values() else {
                skipped.push(key.clone());
                continue;
            };
            let Some((_, previous)) = values.iter().find(|(name, _)| *name == field) else {
                skipped.push(key.clone());
                continue;
            };
            if previous == value {
                continue;
            }
            if current.set(field, value).is_ok() {
                agents_changed += 1;
            } else {
                skipped.push(key.clone());
            }
        }
    }

    Ok(RestoreResult {
        added,
        replaced,
        need_key,
        settings: settings_restored,
        profiles,
        agents: agents_changed,
        skipped,
    })
}

fn agent_field_rank(key: &str) -> u8 {
    if key.ends_with(".provider") {
        0
    } else if key.ends_with(".model") {
        1
    } else {
        2
    }
}

fn read_passphrase(prompt: &str, confirm: bool) -> Result<String> {
    if !io::stdin().is_terminal() {
        let mut line = String::new();
        let bytes = io::stdin()
            .lock()
            .read_line(&mut line)
            .context("read passphrase from stdin")?;
        ensure!(bytes > 0, "no passphrase on stdin");
        let passphrase = line.trim_end_matches(['\r', '\n']).to_owned();
        ensure!(!passphrase.is_empty(), "the passphrase is empty");
        return Ok(passphrase);
    }

    let passphrase = rpassword::prompt_password(prompt).context("read passphrase")?;
    ensure!(!passphrase.is_empty(), "the passphrase is empty");
    if confirm {
        let repeated = rpassword::prompt_password("Again: ").context("confirm passphrase")?;
        ensure!(passphrase == repeated, "the passphrases differ");
    }
    Ok(passphrase)
}

mod base64_bytes {
    use super::*;

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> std::result::Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        STANDARD
            .decode(encoded)
            .map_err(<D::Error as serde::de::Error>::custom)
    }
}

mod base64_map {
    use super::*;

    pub fn serialize<S>(
        values: &BTreeMap<String, Vec<u8>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        values
            .iter()
            .map(|(name, bytes)| (name.clone(), STANDARD.encode(bytes)))
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> std::result::Result<BTreeMap<String, Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        BTreeMap::<String, String>::deserialize(deserializer)?
            .into_iter()
            .map(|(name, encoded)| {
                STANDARD
                    .decode(encoded)
                    .map(|bytes| (name, bytes))
                    .map_err(<D::Error as serde::de::Error>::custom)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn encrypted_backup_round_trips_without_exposing_contents() {
        let mut bundle = Bundle {
            version: 1,
            created: "2026-09-25T00:00:00Z".to_owned(),
            app: "test".to_owned(),
            keys: true,
            ..Bundle::default()
        };
        bundle
            .agents
            .insert("claude.model".to_owned(), "private-provider-key".to_owned());
        bundle
            .icons
            .insert("0011223344556677.png".to_owned(), vec![0, 1, 2]);

        let sealed = seal(&bundle, "correct horse").expect("seal bundle");
        let text = String::from_utf8(sealed.clone()).expect("JSON envelope");
        assert!(!text.contains("private-provider-key"));
        let envelope: Value = serde_json::from_slice(&sealed).expect("parse envelope");
        assert!(envelope["salt"].is_string());
        assert!(envelope["nonce"].is_string());
        assert!(envelope["data"].is_string());

        let opened = open(&sealed, "correct horse").expect("open bundle");
        assert_eq!(opened.created, bundle.created);
        assert_eq!(opened.agents, bundle.agents);
        assert_eq!(opened.icons, bundle.icons);
        assert!(open(&sealed, "wrong horse").is_err());
    }

    #[test]
    fn authentication_covers_the_key_derivation_parameters() {
        let sealed = seal(&Bundle::default(), "passphrase").expect("seal bundle");
        let mut envelope: Value = serde_json::from_slice(&sealed).expect("parse envelope");
        envelope["iterations"] = MIN_ITERATIONS.into();
        let modified = serde_json::to_vec(&envelope).expect("serialize envelope");
        let error = open(&modified, "passphrase")
            .err()
            .expect("modified header must fail authentication");
        assert!(error.to_string().contains(WRONG_PASSPHRASE));
    }

    #[test]
    fn rejects_non_backup_json_before_attempting_decryption() {
        assert!(open(b"{}", "passphrase").is_err());
        assert!(seal(&Bundle::default(), "").is_err());
    }
}
