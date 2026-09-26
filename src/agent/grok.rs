// Grok Build, xAI's grok CLI, keeps its settings in ~/.grok/config.toml (or
// $GROK_HOME's): the model new sessions start with under [models] default,
// and models of the user's own as [model."<id>"] tables. magpie adds one
// such table per catalog model, named "magpie/<provider>/<model>", pointing
// at the gateway with magpie's own key — a model with no key of its own
// would be sent the user's xAI sign-in — so the catalog joins Grok's own
// models in its /model picker.
//
// A signed-in Grok also takes remote "campaign" patches from xAI, applied
// above config.toml, and a launch campaign sets models.default: a default
// picked in magpie was ignored and every new session started on the
// campaign's model. So a default picked here turns campaigns off
// ([features] campaigns = false), and clearing it gives them back.

use std::{fs, path::Path};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table};

use super::{MAGPIE_PREFIX, magpie_models};
use crate::config;

// efforts are the reasoning efforts Grok knows, in its own order.
const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

// wired reports whether the config has a magpie model table: an ordinary
// one, not a string that merely mentions the header or an array table.
pub(crate) fn wired(path: &Path) -> Result<bool> {
    Ok(document(path)?
        .get("model")
        .and_then(Item::as_table)
        .is_some_and(|model| {
            model
                .iter()
                .any(|(name, item)| name.starts_with(MAGPIE_PREFIX) && item.is_table())
        }))
}

// set_model points Grok's default model at value: one of magpie's brings
// the catalog's tables along, and clearing it gives the campaigns back.
pub(crate) fn set_model(path: &Path, value: &str) -> Result<()> {
    let mut doc = document(path)?;
    // read everything before the first write: a config magpie can't parse
    // is left alone
    let effort = doc
        .get("models")
        .and_then(Item::as_table)
        .and_then(|models| models.get("default_reasoning_effort"))
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let campaigns_on = !campaigns_off(&doc);

    if value.is_empty() {
        // back to Grok's own: xAI's campaigns come back
        {
            let model = model_table_mut(&mut doc)?;
            remove_magpie_tables(model);
        }
        remove_key(&mut doc, "models", "default");
        remove_key(&mut doc, "features", "campaigns");
        return save(path, &doc);
    }

    let routed = value.strip_prefix(MAGPIE_PREFIX).is_some_and(|model| {
        magpie_models()
            .unwrap_or_default()
            .iter()
            .any(|entry| entry.id == model)
    });
    {
        let model = model_table_mut(&mut doc)?;
        remove_magpie_tables(model);
        if routed {
            for (name, table) in magpie_tables()? {
                model.insert(name.as_str(), Item::Table(table));
            }
        }
    }
    if !effort.is_empty()
        && let Some(known) = efforts_of(value)
        && !known.contains(&effort)
    {
        remove_key(&mut doc, "models", "default_reasoning_effort");
    }
    if campaigns_on {
        // a default picked here turns xAI's remote campaigns off, which
        // would set models.default over it
        set_key(&mut doc, "features", "campaigns", toml_edit::value(false))?;
    }
    set_key(&mut doc, "models", "default", toml_edit::value(value))?;
    save(path, &doc)
}

// sync rewrites magpie's model tables as the catalog is now, where magpie
// wrote them: nothing else changes.
pub(crate) fn sync(path: &Path) -> Result<()> {
    if !wired(path)? {
        return Ok(());
    }
    let default =
        config::get(path, config::ConfigFormat::Toml, "models.default")?.unwrap_or_default();
    let mut doc = document(path)?;
    if !default.is_empty() && !campaigns_off(&doc) {
        set_key(&mut doc, "features", "campaigns", toml_edit::value(false))?;
    }
    {
        let model = model_table_mut(&mut doc)?;
        remove_magpie_tables(model);
        for (name, table) in magpie_tables()? {
            model.insert(name.as_str(), Item::Table(table));
        }
    }
    save(path, &doc)
}

// check says what of magpie's wiring is gone while the default model is
// still one of magpie's, or "" when it is all there. A config magpie can't
// parse says so, with where it happened.
pub(crate) fn check(path: &Path) -> String {
    let doc = match document(path) {
        Ok(doc) => doc,
        Err(error) => return error.to_string(),
    };
    let default = doc
        .get("models")
        .and_then(Item::as_table)
        .and_then(|models| models.get("default"))
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    if !default.starts_with(MAGPIE_PREFIX) {
        return String::new();
    }
    let table = doc
        .get("model")
        .and_then(Item::as_table)
        .and_then(|model| model.get(default.as_str()))
        .and_then(Item::as_table);
    let Some(table) = table else {
        return format!(
            "Grok Build's [model.{default:?}] (config.toml) is gone, so it no longer reaches magpie"
        );
    };
    let wants = [
        ("base_url", crate::gateway::v1_url()),
        ("api_key", crate::gateway::TOKEN.to_owned()),
    ];
    super::applied::wiring_off(
        "Grok Build",
        "config.toml",
        |key| table.get(key).and_then(Item::as_str).map(str::to_owned),
        &wants,
    )
}

fn document(path: &Path) -> Result<DocumentMut> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DocumentMut::new());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    std::str::FromStr::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn save(path: &Path, doc: &DocumentMut) -> Result<()> {
    crate::config::atomic_write_for_settings(path, doc.to_string().as_bytes())
}

// model_table_mut is the config's [model] table, made implicit when new —
// magpie only adds [model."magpie/…"] tables to it, never a bare header.
fn model_table_mut(doc: &mut DocumentMut) -> Result<&mut Table> {
    let item = doc.entry("model").or_insert_with(|| {
        let mut model = Table::new();
        model.set_implicit(true);
        Item::Table(model)
    });
    item.as_table_mut()
        .with_context(|| "[model] is not a TOML table".to_owned())
}

fn remove_magpie_tables(model: &mut Table) {
    let owned = model
        .iter()
        .filter(|(name, _)| name.starts_with(MAGPIE_PREFIX))
        .map(|(name, _)| name.to_owned())
        .collect::<Vec<_>>();
    for name in owned {
        model.remove(&name);
    }
}

fn remove_key(doc: &mut DocumentMut, table: &str, key: &str) {
    if let Some(item) = doc.get_mut(table)
        && let Some(inner) = item.as_table_mut()
    {
        inner.remove(key);
    }
}

// set_key sets one key of a top-level table, making the table a real one
// when it is missing — magpie's values belong in ordinary tables, not
// inline ones.
fn set_key(doc: &mut DocumentMut, table: &str, key: &str, value: Item) -> Result<()> {
    let item = doc.entry(table).or_insert(Item::Table(Table::new()));
    item.as_table_mut()
        .with_context(|| format!("[{table}] is not a TOML table"))?
        .insert(key, value);
    Ok(())
}

// campaigns_off reports whether [features] campaigns is already false: a
// default picked here leaves the campaigns off, and clearing the default
// removes the key to give them back.
fn campaigns_off(doc: &DocumentMut) -> bool {
    doc.get("features")
        .and_then(Item::as_table)
        .and_then(|features| features.get("campaigns"))
        .is_some_and(|campaigns| {
            campaigns.as_bool() == Some(false) || campaigns.as_str() == Some("false")
        })
}

// magpie_tables is one [model."magpie/<id>"] table per catalog model,
// pointing at the gateway with magpie's own key.
fn magpie_tables() -> Result<Vec<(String, Table)>> {
    let mut tables = Vec::new();
    for model in magpie_models()? {
        let mut table = Table::new();
        table.insert("model", toml_edit::value(model.id.as_str()));
        table.insert("name", toml_edit::value(model.name.as_str()));
        table.insert("base_url", toml_edit::value(crate::gateway::v1_url()));
        table.insert("api_key", toml_edit::value(crate::gateway::TOKEN));
        table.insert("api_backend", toml_edit::value("chat_completions"));
        if model.context > 0 {
            table.insert("context_window", toml_edit::value(model.context as i64));
        }
        let efforts = EFFORTS
            .iter()
            .filter(|effort| model.efforts.iter().any(|known| known == **effort))
            .copied()
            .collect::<Vec<_>>();
        if !efforts.is_empty() {
            let mut array = toml_edit::Array::new();
            for effort in efforts {
                array.push(effort);
            }
            table.insert(
                "reasoning_efforts",
                Item::Value(toml_edit::Value::Array(array)),
            );
        }
        tables.push((format!("{MAGPIE_PREFIX}{}", model.id), table));
    }
    Ok(tables)
}

// efforts_of is the efforts of a model through magpie, as its catalog entry
// has them. None when value is not one of magpie's models.
fn efforts_of(value: &str) -> Option<Vec<String>> {
    let model = value.strip_prefix(MAGPIE_PREFIX)?;
    magpie_models()
        .unwrap_or_default()
        .into_iter()
        .find(|entry| entry.id == model)
        .filter(|entry| !entry.efforts.is_empty())
        .map(|entry| entry.efforts)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::{ALL_AGENTS, Agent, applied, testing};

    fn agent(path: &Path) -> Agent {
        Agent {
            spec: ALL_AGENTS.iter().find(|spec| spec.id == "grok").unwrap(),
            path: path.to_owned(),
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn table(path: &Path, keys: &[&str]) -> Option<Table> {
        let doc: DocumentMut = std::str::FromStr::from_str(&read(path)).unwrap();
        let (last, parents) = keys.split_last().unwrap();
        let mut item = doc.as_table();
        for key in parents {
            item = item.get(key)?.as_table()?;
        }
        item.get(last)?.as_table().cloned()
    }

    #[test]
    fn magpie_models_come_and_go_without_touching_the_rest() {
        let home = testing::isolation();
        let path = home.path().join("grok/config.toml");
        write(
            &path,
            "# mine\n[ui]\ntheme = \"dark\"\n\n[model.my-own]\nmodel = \"x\"\nbase_url = \"https://x/v1\"\n\n[models]\ndefault = \"grok-4.6\"\n",
        );
        let grok = agent(&path);
        assert!(grok.is_detected());
        assert_eq!(
            grok.values().unwrap(),
            vec![("model", "grok-4.6".to_owned()), ("effort", String::new())]
        );

        grok.apply("model", "magpie/deepseek/pro").unwrap();
        let raw = read(&path);
        let magpie_table = table(&path, &["model", "magpie/deepseek/pro"]).expect("magpie's table");
        assert_eq!(
            magpie_table.get("model").unwrap().as_str(),
            Some("deepseek/pro")
        );
        assert_eq!(
            magpie_table.get("api_key").unwrap().as_str(),
            Some("magpie")
        );
        assert!(
            magpie_table
                .get("base_url")
                .unwrap()
                .as_str()
                .unwrap()
                .ends_with("/v1")
        );
        assert_eq!(
            magpie_table.get("api_backend").unwrap().as_str(),
            Some("chat_completions")
        );
        assert!(table(&path, &["model", "magpie/deepseek/flash"]).is_some());
        // a campaign of xAI's would set the default over it
        assert_eq!(
            table(&path, &["features"])
                .unwrap()
                .get("campaigns")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        assert_eq!(grok.values().unwrap()[0].1, "magpie/deepseek/pro");
        assert!(raw.contains("# mine"));
        assert!(raw.contains("[model.my-own]"));
        assert_eq!(
            table(&path, &["ui"])
                .unwrap()
                .get("theme")
                .unwrap()
                .as_str(),
            Some("dark")
        );
        assert_eq!(
            applied::of("grok").field("model"),
            Some("magpie/deepseek/pro")
        );
        // the config is all there: no drift
        assert!(grok.drift().is_none());

        // picked again, and synced: the tables are replaced, never repeated
        grok.set("model", "magpie/deepseek/flash").unwrap();
        sync(&path).unwrap();
        let raw = read(&path);
        assert_eq!(
            raw.matches("[model.\"magpie/deepseek/pro\"]").count(),
            1,
            "{raw}"
        );

        // a model of Grok's own: magpie steps out, the user's own stays
        grok.set("model", "grok-4.6").unwrap();
        let raw = read(&path);
        assert_eq!(grok.values().unwrap()[0].1, "grok-4.6");
        assert!(!raw.contains("magpie"), "{raw}");
        assert!(raw.contains("[model.my-own]"));
        // nothing through magpie: sync leaves the file alone
        let before = read(&path);
        sync(&path).unwrap();
        assert_eq!(read(&path), before);

        grok.set("model", "magpie/deepseek/flash").unwrap();
        grok.set("effort", "high").unwrap();
        assert_eq!(grok.values().unwrap()[1].1, "high");
        grok.set("model", "").unwrap();
        let raw = read(&path);
        assert_eq!(grok.values().unwrap()[0].1, String::new());
        assert!(!raw.contains("magpie"), "{raw}");
        assert!(!raw.contains("campaigns"), "{raw}");
        assert_eq!(grok.values().unwrap()[1].1, "high");
    }

    // Something else rewrote the config so the model is magpie's while the
    // gateway is not: unwired, and setting it again wires it back. A magpie
    // model replaced by Grok's own is replaced, and keep forgets it.
    #[test]
    fn tampering_is_drift_and_reapply_sets_it_back() {
        let home = testing::isolation();
        let path = home.path().join("grok/config.toml");
        write(&path, "[models]\ndefault = \"grok-4.6\"\n");
        let grok = agent(&path);
        grok.apply("model", "magpie/deepseek/pro").unwrap();
        assert!(grok.drift().is_none());

        write(&path, "[models]\ndefault = \"magpie/deepseek/pro\"\n");
        let drift = grok.drift().expect("unwired");
        assert_eq!(drift.kind, "unwired");
        assert_eq!(drift.field, "model");
        assert_eq!(drift.want, "magpie/deepseek/pro");
        assert!(drift.detail.contains("base_url"), "{}", drift.detail);
        grok.reapply().unwrap();
        assert!(grok.drift().is_none());
        assert!(
            table(&path, &["model", "magpie/deepseek/pro"])
                .expect("wired back")
                .contains_key("base_url")
        );

        write(&path, "[models]\ndefault = \"grok-4.6\"\n");
        let drift = grok.drift().expect("replaced");
        assert_eq!(drift.kind, "replaced");
        assert_eq!(drift.want, "magpie/deepseek/pro");
        assert_eq!(drift.now, "grok-4.6");
        grok.reapply().unwrap();
        assert_eq!(grok.values().unwrap()[0].1, "magpie/deepseek/pro");
        assert!(grok.drift().is_none());
        write(&path, "[models]\ndefault = \"grok-4.6\"\n");
        grok.keep().unwrap();
        assert!(grok.drift().is_none());
    }

    // A config magpie can't parse stops every change and stays as it was.
    #[test]
    fn a_broken_config_stops_changes() {
        let home = testing::isolation();
        let path = home.path().join("config.toml");
        let wired = "[model.\"magpie/keep\"]\nmodel = \"keep\"\n\n";
        for contents in [
            format!("{wired}[models]\ndefault = [\n"),
            format!("[models]\ndefault = \"magpie/keep\"\n\n{wired}[features]\ncampaigns = [\n"),
        ] {
            write(&path, &contents);
            assert!(set_model(&path, "grok-native").is_err());
            assert!(set_model(&path, "").is_err());
            assert!(sync(&path).is_err());
            assert_eq!(read(&path), contents);
        }
    }

    // Only an ordinary magpie model table counts as wired: one inside a
    // multiline string, or an array table, is the user's own writing.
    #[test]
    fn sync_requires_an_ordinary_model_table() {
        let home = testing::isolation();
        let path = home.path().join("config.toml");
        for body in [
            "note = '''\n[model.\"magpie/fake\"]\n'''\n".to_owned(),
            "[[model.\"magpie/array\"]]\nmodel = \"array\"\n".to_owned(),
        ] {
            let contents = format!("[models]\ndefault = \"grok-native\"\n{body}");
            write(&path, &contents);
            sync(&path).unwrap();
            assert_eq!(read(&path), contents);
        }
    }

    // A check on a config magpie can't parse reports the parse, with where
    // it happened.
    #[test]
    fn check_reports_parse_errors() {
        let home = testing::isolation();
        let path = home.path().join("config.toml");
        write(
            &path,
            "[models]\ndefault = \"magpie/fake\"\n\n[model.\"magpie/fake\"]\ninvalid = [\n",
        );
        let message = check(&path);
        assert!(
            message.contains(&path.display().to_string()) && message.contains("parse"),
            "{message}"
        );
    }
}
