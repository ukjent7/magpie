use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::SavedLogin;

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct Credential {
    pub(super) key: String,
    pub(super) email: String,
}

pub(super) fn live_login() -> Option<SavedLogin> {
    let home = home_dir()?;
    let credential = read_credential(&home)?;
    (!credential.email.is_empty()).then_some(SavedLogin {
        agent: "grok".to_owned(),
        user: credential.email,
        ..SavedLogin::default()
    })
}

pub(super) fn read_credential(home: &Path) -> Option<Credential> {
    let credentials: BTreeMap<String, Credential> =
        serde_json::from_slice(&fs::read(home.join("auth.json")).ok()?).ok()?;
    credentials
        .into_values()
        .find(|credential| !credential.key.is_empty())
}

pub(super) fn home_dir() -> Option<PathBuf> {
    env::var_os("GROK_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home().map(|home| home.join(".grok")))
}

pub(super) fn accounts_dir() -> Result<PathBuf> {
    let providers = crate::settings::providers_path();
    providers
        .parent()
        .map(|parent| parent.join("grok"))
        .context("cannot locate the Grok account directory")
}

pub(super) fn executable() -> Option<PathBuf> {
    if let Some(home) = home_dir() {
        let bundled = home
            .join("bin")
            .join(format!("grok{}", env::consts::EXE_SUFFIX));
        if is_file(&bundled) {
            return Some(bundled);
        }
    }

    let names = [
        format!("grok{}", env::consts::EXE_SUFFIX),
        "grok".to_owned(),
    ];
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            for name in &names {
                let candidate = directory.join(name);
                if is_grok_build(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    user_home()
        .map(|home| home.join(".local/bin").join(&names[0]))
        .filter(|path| is_grok_build(path))
}

pub(super) fn remove_login_home(home: &Path) -> Result<()> {
    if !home.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(home)
        .with_context(|| format!("inspect Grok account home at {}", home.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("refusing to remove a Grok home that is not a managed directory");
    }
    let root = accounts_dir()?;
    let root = fs::canonicalize(&root).with_context(|| {
        format!(
            "resolve the managed Grok account directory at {}",
            root.display()
        )
    })?;
    let home = fs::canonicalize(home).context("resolve the saved Grok account home")?;
    if home == root || !home.starts_with(&root) {
        bail!("refusing to remove a Grok home outside Magpie's account directory");
    }
    fs::remove_dir_all(&home).with_context(|| format!("remove Grok account at {}", home.display()))
}

fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn is_grok_build(path: &Path) -> bool {
    fs::canonicalize(path).is_ok_and(|resolved| {
        resolved
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
            .contains("/.grok/")
    })
}

fn user_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
}
