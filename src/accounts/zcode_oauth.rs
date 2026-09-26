use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{SavedLogin, nonempty, oauth::open_browser, read_saved_logins, write_saved_logins};
use crate::provider::zcode::{self, ZcodeKey};

pub(super) async fn command(args: &[String]) -> Result<()> {
    let [action, agent] = args else {
        bail!("usage: magpie accounts add zcode");
    };
    ensure!(action.eq_ignore_ascii_case("add"), "expected add");
    ensure!(
        agent.eq_ignore_ascii_case("zcode"),
        "usage: magpie accounts add zcode"
    );
    add_zcode_account().await
}

async fn add_zcode_account() -> Result<()> {
    let (user, plan, key) = zcode::sign_in(|url| {
        println!("Open {url} and sign in to Z.ai.");
        if !open_browser(url) {
            println!("could not open a browser; visit the page yourself");
        }
    })
    .await?;
    save_login(&user, &plan, &key)
}

fn save_login(user: &str, plan: &str, key: &ZcodeKey) -> Result<()> {
    let own_user = zcode::own().map(|own| own.user);
    if own_user
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(user))
    {
        println!("✓ ZCode is already signed in as {user}");
        return Ok(());
    }

    let mut logins = read_saved_logins()?;
    let seen = Value::String(
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format account timestamp")?,
    );
    let active = logins
        .iter()
        .rfind(|login| login.agent == "zcode" && login.first)
        .map(|login| login.user.clone())
        .or_else(|| own_user.clone())
        .or_else(|| {
            logins
                .iter()
                .find(|login| login.agent == "zcode")
                .map(|login| login.user.clone())
        });
    let existing = logins
        .iter()
        .position(|login| login.agent == "zcode" && login.user.eq_ignore_ascii_case(user));
    let mut login = SavedLogin {
        agent: "zcode".to_owned(),
        user: user.to_owned(),
        plan: nonempty(plan.to_owned()),
        seen: Some(seen),
        on: existing.is_none(),
        first: active.is_none(),
        auth: Some(serde_json::to_value(key).context("serialize the ZCode key")?),
        ..SavedLogin::default()
    };
    if let Some(index) = existing {
        login.on = logins[index].on;
        login.first = logins[index].first;
        login.extra = std::mem::take(&mut logins[index].extra);
        logins[index] = login;
    } else {
        logins.push(login);
    }
    write_saved_logins(&mut logins)?;
    println!("✓ added {user} · use it with: magpie accounts switch zcode {user}");
    Ok(())
}
