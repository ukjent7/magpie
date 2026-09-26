// An agent's config is a file anyone can write: another switcher, an
// installer, the agent's own setup. One that takes magpie out leaves the
// row showing a magpie model while the agent asks its own vendor for it,
// and the user blames magpie. So magpie remembers what it last set on each
// agent (applied.json, beside the stash) and says when that no longer
// holds — Drift — with the way to set it again.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{LazyLock, Mutex, MutexGuard, PoisonError},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use super::Agent;

static APPLIED_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    APPLIED_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

// started is when this process — and the gateway in it — came up: before
// then, a request the gateway missed says nothing about the agent.
static STARTED: LazyLock<OffsetDateTime> = LazyLock::new(OffsetDateTime::now_utc);

// Applied is what magpie last set on one agent, and when.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Applied {
    // at is when magpie last set the fields, RFC 3339.
    at: String,
    // fields maps a field key to the value magpie set it to.
    fields: HashMap<String, String>,
}

impl Applied {
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    pub fn at(&self) -> Option<OffsetDateTime> {
        OffsetDateTime::parse(&self.at, &Rfc3339).ok()
    }
}

fn applied_path() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(path) = super::testing::applied_path() {
            return path;
        }
    }
    crate::settings::providers_path().with_file_name("applied.json")
}

// load is agent id → what magpie set on it.
fn load() -> HashMap<String, Applied> {
    match fs::read_to_string(applied_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn save(applied: &HashMap<String, Applied>) -> Result<()> {
    let path = applied_path();
    let mut bytes = serde_json::to_string_pretty(applied).context("serialize applied record")?;
    bytes.push('\n');
    crate::config::atomic_write_secret_for_settings(&path, bytes.as_bytes())
}

// of is what magpie last set on one agent.
pub fn of(agent_id: &str) -> Applied {
    let _guard = lock();
    load().get(agent_id).cloned().unwrap_or_default()
}

// record keeps what a field reads after magpie set it; a field put back to
// the agent's default is magpie's no longer.
pub fn record(agent_id: &str, key: &str, value: &str) -> Result<()> {
    let _guard = lock();
    let mut applied = load();
    let entry = applied.entry(agent_id.to_owned()).or_default();
    if value.is_empty() {
        entry.fields.remove(key);
    } else {
        entry.fields.insert(key.to_owned(), value.to_owned());
    }
    entry.at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    save(&applied)
}

// forget takes the agent's config as it is now: what magpie set before is
// forgotten, and no longer said to have been changed.
pub fn forget(agent_id: &str) -> Result<()> {
    let _guard = lock();
    let mut applied = load();
    if applied.remove(agent_id).is_none() {
        return Ok(());
    }
    save(&applied)
}

// Drift is how an agent differs from what magpie set on it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Drift {
    // kind says what is off:
    //   "unwired"  the model is magpie's but the config no longer sends it
    //              through magpie (the agent's check);
    //   "replaced" a magpie model magpie set was replaced by the agent's own;
    //   "bypassed" the config is right, yet the agent was used since and
    //              nothing of it reached the gateway — it runs on an old
    //              config, or something outside the file overrides it.
    pub kind: String,
    // field is the field it shows on.
    pub field: String,
    // now is what that field says now; want, what setting it again sets.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub now: String,
    pub want: String,
    // detail is what exactly is off, for a tooltip.
    pub detail: String,
}

// drift says what, if anything, keeps the agent off what magpie set: its
// config first (wiring, then each field against magpie's record), then —
// the config being right — whether its latest use actually came through.
// A field moved from one magpie model to another (the agent's own picker)
// isn't drift; one moved off magpie is.
pub fn drift(agent: &Agent) -> Option<Drift> {
    if agent.spec.fields.is_empty() {
        return None;
    }
    let values = agent.values().unwrap_or_default();
    let value_of = |key: &str| {
        values
            .iter()
            .find(|(field, _)| *field == key)
            .map_or_else(String::new, |(_, value)| value.clone())
    };
    // the field on one of magpie's models: where drift shows, and what
    // setting it again sets
    let mut on = agent.spec.fields[0].key;
    let mut on_magpie = false;
    for field in agent.spec.fields {
        if magpie_value(&value_of(field.key)) {
            on = field.key;
            on_magpie = true;
            break;
        }
    }
    let detail = check(agent);
    if !detail.is_empty() {
        let now = value_of(on);
        return Some(Drift {
            kind: "unwired".to_owned(),
            field: on.to_owned(),
            now: now.clone(),
            want: now,
            detail,
        });
    }
    let record = of(agent.spec.id);
    for field in agent.spec.fields {
        let Some(want) = record.fields.get(field.key) else {
            continue;
        };
        let now = value_of(field.key);
        if now == *want || !magpie_value(want) || magpie_value(&now) {
            continue;
        }
        return Some(Drift {
            kind: "replaced".to_owned(),
            field: field.key.to_owned(),
            now: now.clone(),
            want: want.clone(),
            detail: format!(
                "{}'s config was changed outside magpie: {} is {}, not {} as magpie set it",
                agent.spec.name,
                field.label,
                or_default(&now),
                want
            ),
        });
    }
    if on_magpie
        && let Some(used) = last_used(agent)
        && bypassed(
            Some(used),
            record.at(),
            crate::usage::last_seen(agent.spec.id),
            *STARTED,
            OffsetDateTime::now_utc(),
        )
    {
        let now = value_of(on);
        return Some(Drift {
            kind: "bypassed".to_owned(),
            field: on.to_owned(),
            now: now.clone(),
            want: now,
            detail: format!(
                "{} was used at {:02}:{:02} but none of its requests reached magpie — one started before magpie set it up still runs on its old config: restart it",
                agent.spec.name,
                used.hour(),
                used.minute()
            ),
        });
    }
    None
}

// check says what of the agent's wiring beyond its model field is gone
// while the model is still one of magpie's, or "" when it is all there.
fn check(agent: &Agent) -> String {
    match agent.spec.id {
        "grok" => super::grok::check(&agent.path),
        "alma" => super::alma::check(),
        _ => String::new(),
    }
}

// last_used is when the agent was last used by its own account — each
// prompt a request the gateway should have seen, so one it didn't went
// round magpie. The Go agents keep prompt logs magpie reads here (Codex's
// history.jsonl and the like); no Rust agent's log is read yet, so nothing
// counts as bypassed until that lands.
fn last_used(_agent: &Agent) -> Option<OffsetDateTime> {
    None
}

// bypassed: the agent was used — while this gateway was up and after magpie
// last set it — and no request of it arrived since. A request leaves within
// moments of the prompt; a little grace keeps one in flight from counting.
pub(crate) fn bypassed(
    used: Option<OffsetDateTime>,
    applied: Option<OffsetDateTime>,
    seen: Option<OffsetDateTime>,
    started: OffsetDateTime,
    now: OffsetDateTime,
) -> bool {
    const GRACE: Duration = Duration::seconds(30);
    let Some(used) = used else {
        return false;
    };
    used > started
        && applied.is_none_or(|applied| used > applied)
        && now - used > GRACE
        && seen.is_none_or(|seen| seen < used - Duration::seconds(2))
}

// magpie_value: the value is one of magpie's models as these agents spell
// it, magpie/<model>.
fn magpie_value(value: &str) -> bool {
    value
        .strip_prefix(super::MAGPIE_PREFIX)
        .is_some_and(|model| {
            super::magpie_models()
                .unwrap_or_default()
                .iter()
                .any(|entry| entry.id == model)
        })
}

fn or_default(value: &str) -> &str {
    if value.is_empty() {
        "the agent's default"
    } else {
        value
    }
}

// field_value reads one field of the agent as it is now.
fn field_value(agent: &Agent, key: &str) -> String {
    agent
        .values()
        .ok()
        .and_then(|values| values.into_iter().find(|(field, _)| *field == key))
        .map_or_else(String::new, |(_, value)| value)
}

// reapply sets again what magpie set on the agent: what drifted, else its
// fields as they read now — for a config taken off magpie in a way no check
// catches. A replaced field brings back the others magpie set with it.
// Either way the record is renewed, so a use before now no longer counts.
pub fn reapply(agent: &Agent) -> Result<()> {
    let drift = drift(agent);
    if let Some(found) = &drift
        && found.kind == "replaced"
    {
        let record = of(agent.spec.id);
        // the model first: the others (an effort) are checked against it
        agent.apply(&found.field, &found.want)?;
        for field in agent.spec.fields {
            let Some(value) = record.fields.get(field.key) else {
                continue;
            };
            if field.key == found.field || field_value(agent, field.key) == *value {
                continue;
            }
            agent.apply(field.key, value)?;
        }
        return Ok(());
    }
    let values = agent.values()?;
    for field in agent.spec.fields {
        let Some((_, value)) = values.iter().find(|(key, _)| *key == field.key) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let wanted =
            drift.as_ref().is_some_and(|found| found.field == field.key) || magpie_value(value);
        if wanted {
            agent.apply(field.key, value)?;
        }
    }
    Ok(())
}

// wiring_off checks what magpie wrote into an agent's config to reach the
// gateway — pairs of key and value, read with get — and says which no
// longer holds, "" when all do. A URL is quoted; a key's value never is.
pub(crate) fn wiring_off(
    name: &str,
    file: &str,
    get: impl Fn(&str) -> Option<String>,
    kvs: &[(&str, String)],
) -> String {
    for (key, want) in kvs {
        let value = get(key).unwrap_or_default();
        if value == *want {
            continue;
        }
        let place = format!("{name}'s {key} ({file})");
        return if value.is_empty() {
            format!("{place} is gone, so it no longer reaches magpie")
        } else if key.to_lowercase().contains("url") {
            format!("{place} is {value}, not magpie's gateway at {want}")
        } else {
            format!("{place} was changed, so magpie's gateway won't take its requests")
        };
    }
    String::new()
}

// host_of is the server a URL names, whatever its scheme (Codex asks
// magpie's http gateway over ws) or path.
pub(crate) fn host_of(url: &str) -> String {
    let rest = url.trim();
    let rest = rest.split_once("://").map_or(rest, |(_, rest)| rest);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

// same_host: two URLs name the same server.
pub(crate) fn same_host(left: &str, right: &str) -> bool {
    host_of(left) == host_of(right)
}

#[cfg(test)]
mod tests {
    use time::Date;

    use super::*;

    fn at(day: u8, hour: u8, minute: u8) -> OffsetDateTime {
        let date = Date::from_calendar_date(2026, time::Month::September, day).unwrap();
        date.with_hms(hour, minute, 0).unwrap().assume_utc()
    }

    #[test]
    fn records_what_magpie_set_and_forgets_it() {
        let _home = crate::agent::testing::isolation();
        record("grok", "model", "magpie/deepseek/pro").unwrap();
        record("grok", "effort", "high").unwrap();
        let applied = of("grok");
        assert_eq!(applied.field("model"), Some("magpie/deepseek/pro"));
        assert_eq!(applied.field("effort"), Some("high"));
        assert!(applied.at().is_some());
        // a field put back to the agent's default is magpie's no longer
        record("grok", "effort", "").unwrap();
        assert_eq!(of("grok").field("effort"), None);
        assert_eq!(of("grok").field("model"), Some("magpie/deepseek/pro"));
        forget("grok").unwrap();
        assert_eq!(of("grok").field("model"), None);
        // forgetting an agent magpie never set is fine
        forget("zcode").unwrap();
    }

    #[test]
    fn bypassed_needs_a_use_the_gateway_missed() {
        let now = at(26, 12, 0);
        let started = now - Duration::hours(1);
        let applied = now - Duration::minutes(10);
        let used = now - Duration::minutes(5);
        let seen = now - Duration::minutes(6);
        assert!(bypassed(
            Some(used),
            Some(applied),
            Some(seen),
            started,
            now
        ));
        // a request arrived within moments of the prompt
        assert!(!bypassed(
            Some(used),
            Some(applied),
            Some(used - Duration::seconds(1)),
            started,
            now
        ));
        // used before magpie set it
        assert!(!bypassed(
            Some(applied - Duration::seconds(1)),
            Some(applied),
            None,
            started,
            now
        ));
        // used while the gateway was down
        assert!(!bypassed(
            Some(started - Duration::minutes(1)),
            Some(applied),
            None,
            started,
            now
        ));
        // the prompt is too recent to judge
        assert!(!bypassed(
            Some(now - Duration::seconds(10)),
            Some(applied),
            None,
            started,
            now
        ));
    }

    #[test]
    fn hosts_compare_whatever_their_scheme_or_path() {
        assert!(same_host(
            "http://127.0.0.1:3425/v1",
            "ws://127.0.0.1:3425/backend-api"
        ));
        assert!(same_host("http://localhost:3425", "http://localhost/"));
        assert!(!same_host("http://localhost:3425", "http://127.0.0.1:9"));
    }
}
