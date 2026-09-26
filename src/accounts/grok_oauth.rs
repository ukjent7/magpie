use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{
    SavedLogin, grok_active_user, grok_identity, oauth::random_token, read_saved_logins,
    write_saved_logins,
};

pub(super) async fn command(args: &[String]) -> Result<()> {
    let [action, agent] = args else {
        bail!("usage: magpie accounts add grok");
    };
    ensure!(action.eq_ignore_ascii_case("add"), "expected add");
    ensure!(
        agent.eq_ignore_ascii_case("grok"),
        "usage: magpie accounts add grok"
    );
    add_grok_account().await
}

async fn add_grok_account() -> Result<()> {
    let executable =
        grok_identity::executable().context("install the xAI Grok CLI first: https://x.ai/cli")?;
    let own_home = grok_identity::home_dir().context("cannot locate the Grok CLI home")?;
    let own_login = grok_identity::read_credential(&own_home);
    let using_own_home = own_login.is_none();
    let home = if using_own_home {
        own_home
    } else {
        new_grok_home()?
    };

    println!("Follow the Grok CLI's device sign-in instructions below.");
    if let Err(error) = run_grok_login(&executable, &home).await {
        if !using_own_home {
            let _ = grok_identity::remove_login_home(&home);
        }
        return Err(error);
    }

    let Some(credential) = grok_identity::read_credential(&home) else {
        if !using_own_home {
            let _ = grok_identity::remove_login_home(&home);
        }
        bail!("Grok login finished without a saved account");
    };
    if credential.email.is_empty() {
        if !using_own_home {
            let _ = grok_identity::remove_login_home(&home);
        }
        bail!("Grok login finished without an account email");
    }

    if using_own_home {
        println!("✓ grok is signed in as {}", credential.email);
        return Ok(());
    }
    if let Err(error) = save_additional_login(&home, credential.email) {
        let _ = grok_identity::remove_login_home(&home);
        return Err(error);
    }
    Ok(())
}

async fn run_grok_login(executable: &Path, home: &Path) -> Result<()> {
    let mut command = crate::proc::async_command(executable);
    command
        .args(["login", "--device-auth"])
        .env("GROK_HOME", home)
        .env_remove("GROK_AUTH_PROVIDER_COMMAND")
        .env_remove("GROK_AUTH_EXPIRED")
        .kill_on_drop(true);
    crate::netproxy::configure_process_proxy(&mut command, "api.x.ai");
    let status = tokio::time::timeout(Duration::from_secs(10 * 60), command.status())
        .await
        .context("Grok sign-in timed out; start it again")?
        .context("run the Grok CLI sign-in")?;
    ensure!(status.success(), "Grok CLI sign-in exited with {status}");
    Ok(())
}

fn new_grok_home() -> Result<PathBuf> {
    let directory = grok_identity::accounts_dir()?;
    fs::create_dir_all(&directory)
        .with_context(|| format!("create Grok account directory at {}", directory.display()))?;
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("inspect Grok account directory at {}", directory.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "Grok account path must be a directory owned by Magpie"
    );
    set_private_directory(&directory)?;

    for _ in 0..16 {
        let home = directory.join(random_token(18)?);
        match fs::create_dir(&home) {
            Ok(()) => {
                if let Err(error) = set_private_directory(&home) {
                    let _ = fs::remove_dir(&home);
                    return Err(error);
                }
                return fs::canonicalize(&home)
                    .with_context(|| format!("resolve Grok account home at {}", home.display()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create Grok account home at {}", home.display()));
            }
        }
    }
    bail!("could not create a unique Grok account home")
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect Grok account directory at {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn save_additional_login(home: &Path, user: String) -> Result<()> {
    let own_user = grok_identity::live_login().map(|login| login.user);
    if own_user
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(&user))
    {
        let _ = grok_identity::remove_login_home(home);
        println!("✓ Grok is already signed in as {user}");
        return Ok(());
    }

    let mut logins = read_saved_logins()?;
    let seen = Value::String(
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format account timestamp")?,
    );
    let active = grok_active_user(&logins, own_user.as_deref());
    let existing = logins
        .iter()
        .position(|login| login.agent == "grok" && login.user.eq_ignore_ascii_case(&user));
    let mut login = SavedLogin {
        agent: "grok".to_owned(),
        user: user.clone(),
        plan: None,
        home: Some(home.to_owned()),
        seen: Some(seen),
        on: existing.is_none(),
        first: active.is_none(),
        ..SavedLogin::default()
    };
    let previous_home = if let Some(index) = existing {
        login.on = logins[index].on;
        login.first = logins[index].first;
        login.extra = std::mem::take(&mut logins[index].extra);
        let previous_home = logins[index].home.clone();
        logins[index] = login;
        previous_home
    } else {
        logins.push(login);
        None
    };
    write_saved_logins(&mut logins)?;
    if let Some(previous_home) = previous_home
        && previous_home != home
    {
        let _ = grok_identity::remove_login_home(&previous_home);
    }
    println!("✓ added {user} · use it with: magpie accounts switch grok {user}");
    Ok(())
}
