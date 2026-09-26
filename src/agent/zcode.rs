// ZCode (Zhipu's desktop app) keeps its model providers in
// ~/.zcode/v2/config.json, OpenCode's provider shape with a kind of its own:
//
//	{"provider":{"magpie":{"name":"magpie","kind":"anthropic",
//	  "options":{"apiKey":"magpie","baseURL":"http://127.0.0.1:3425"},
//	  "enabled":true,"source":"custom",
//	  "models":{"<id>":{"name":…,"limit":{"context":…},"modalities":{…}}}}}}
//
// An anthropic provider is asked at baseURL + /v1/messages. The model is
// picked per task in ZCode's own picker and kept in its window, not in a
// file, so what magpie sets is whether its models are in that picker.
//
// ZCode 3.14 moved its providers to ~/.zcode/v2/provider_config.json and
// reads config.json's only once, to import them, so a provider added there
// later never reached its picker. There a provider is a rule, and a model's
// context window and inputs are rules of their own:
//
//	{"schemaVersion":1,"config":{
//	  "providerConfigRules":{"providerRules":[{"providerId":"magpie",
//	    "providerName":"magpie","enabled":true,"config":{
//	      "group":"standard-personal",
//	      "access":{"type":"api-key","apiKey":"magpie"},
//	      "api":{"type":"anthropic-messages","baseUrl":"http://127.0.0.1:3425"},
//	      "personalModelIds":[…],"modelOrder":[…]}}]},
//	  "modelConfigRules":{"providerModelRules":[{"providerId":"magpie",
//	    "modelId":…,"config":{"properties":{"contextWindow":…,
//	      "inputFormat":{"supportsImage":…}}}}],
//	    "manualProviderModelRules":[…]}}}
//
// magpie writes both files, so an older ZCode sees its models too. A model
// the user set by hand in ZCode (a manual rule) keeps what they set, and
// what the user did there stays: a provider they turned off stays off, and
// one they removed isn't put back when magpie syncs.

use std::{fs, path::Path, path::PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::{MAGPIE_ID, magpie_models};
use crate::config::{self, ConfigFormat};

// wired reports whether magpie's provider is in either of ZCode's files.
pub(crate) fn wired(path: &Path) -> Result<bool> {
    Ok(config::exists(path, ConfigFormat::Jsonc, "provider.magpie")? || ruled(&rules_path(path)))
}

// set points ZCode's picker at magpie's models (value "magpie") or takes
// them out again.
pub(crate) fn set(path: &Path, value: &str) -> Result<()> {
    let on = !value.is_empty();
    rules_write(&rules_path(path), on)?;
    if on {
        config::set_jsonc_value(path, "provider.magpie", &provider_json(path)?)
    } else {
        config::delete(path, ConfigFormat::Jsonc, "provider.magpie")
    }
}

// sync refreshes magpie's provider and its models' rules, where magpie
// wrote them: a config that has no magpie provider is left alone.
pub(crate) fn sync(path: &Path) -> Result<()> {
    if !wired(path)? {
        return Ok(());
    }
    let rules = rules_path(path);
    // a ZCode that keeps its providers as rules, with magpie's taken out
    // there, had it removed in ZCode: it isn't put back
    if !rules.exists() || ruled(&rules) {
        rules_write(&rules, true)?;
    }
    config::set_jsonc_value(path, "provider.magpie", &provider_json(path)?)
}

fn rules_path(path: &Path) -> PathBuf {
    path.with_file_name("provider_config.json")
}

// ruled reports whether provider_config.json has magpie's provider.
fn ruled(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    list(&doc, &["config", "providerConfigRules", "providerRules"])
        .iter()
        .any(|rule| rule.get("providerId").and_then(Value::as_str) == Some(MAGPIE_ID))
}

// provider_json is magpie's provider in config.json at path; one turned off
// in ZCode stays off.
fn provider_json(path: &Path) -> Result<Value> {
    let on = config::get(path, ConfigFormat::Jsonc, "provider.magpie.enabled")?.as_deref()
        != Some("false");
    Ok(json!({
        "name": MAGPIE_ID,
        "kind": "anthropic",
        "enabled": on,
        "source": "custom",
        "options": {
            "apiKey": crate::gateway::TOKEN,
            "baseURL": crate::gateway::url(),
        },
        "models": models_json()?,
    }))
}

// models_json is magpie's catalog as ZCode's picker is shown it.
fn models_json() -> Result<Map<String, Value>> {
    let mut models = Map::new();
    for model in magpie_models()? {
        let window = if model.context == 0 {
            200_000
        } else {
            model.context
        };
        let mut input = vec![json!("text")];
        if model.images {
            input.push(json!("image"));
        }
        models.insert(
            model.id.clone(),
            json!({
                "name": &model.name,
                "limit": {"context": window},
                "modalities": {"input": input, "output": ["text"]},
            }),
        );
    }
    Ok(models)
}

// rules_write puts magpie's provider and its models' rules into ZCode's
// provider_config.json (on) or takes them out, leaving every other rule and
// key as ZCode wrote it.
fn rules_write(path: &Path, on: bool) -> Result<()> {
    let original = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if original.is_none() && !on {
        return Ok(());
    }
    let mut doc: Value = match original.as_deref() {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str(text).with_context(|| format!("parse {}", path.display()))?
        }
        _ => json!({}),
    };
    if !doc.is_object() {
        doc = json!({});
    }
    if doc.get("schemaVersion").is_none() {
        doc["schemaVersion"] = json!(1);
    }
    let config = object_at(
        doc.as_object_mut()
            .expect("provider_config.json holds a JSON object"),
        "config",
    );
    let mut providers_config = take_object(config, "providerConfigRules");
    let mut models_config = take_object(config, "modelConfigRules");

    // at is where magpie's provider rule was, which it keeps
    let mut at = None;
    let mut old_enabled = None;
    let mut providers = Vec::new();
    for rule in providers_config
        .get("providerRules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if rule.get("providerId").and_then(Value::as_str) == Some(MAGPIE_ID) {
            old_enabled = rule.get("enabled").and_then(Value::as_bool);
            at = Some(providers.len());
        } else {
            providers.push(rule);
        }
    }

    // a model the user set by hand in ZCode keeps what they set
    let mut by_hand = Vec::new();
    let mut manual = Vec::new();
    for rule in models_config
        .get("manualProviderModelRules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if rule.get("providerId").and_then(Value::as_str) == Some(MAGPIE_ID) {
            if !on {
                continue;
            }
            if let Some(id) = rule.get("modelId").and_then(Value::as_str) {
                by_hand.push(id.to_owned());
            }
        }
        manual.push(rule);
    }
    let mut models = models_config
        .get("providerModelRules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|rule| rule.get("providerId").and_then(Value::as_str) != Some(MAGPIE_ID))
        .collect::<Vec<_>>();

    if on {
        let mut ids = Vec::new();
        for model in magpie_models().unwrap_or_default() {
            ids.push(model.id.clone());
            if by_hand.contains(&model.id) {
                continue;
            }
            let mut properties = json!({"inputFormat": {"supportsImage": model.images}});
            if model.context > 0 {
                properties["contextWindow"] = json!(model.context);
            }
            models.push(json!({
                "providerId": MAGPIE_ID,
                "modelId": &model.id,
                "config": {"properties": properties},
            }));
        }
        let mut rule = json!({
            "providerId": MAGPIE_ID,
            "providerName": MAGPIE_ID,
            "enabled": true,
            "config": {
                "group": "standard-personal",
                "access": {"type": "api-key", "apiKey": crate::gateway::TOKEN},
                "api": {"type": "anthropic-messages", "baseUrl": crate::gateway::url()},
                "personalModelIds": &ids,
                "modelOrder": &ids,
            },
        });
        // turned off in ZCode, it stays off
        if let Some(enabled) = old_enabled {
            rule["enabled"] = json!(enabled);
        }
        match at {
            Some(at) if at <= providers.len() => {
                providers.insert(at, rule);
            }
            _ => providers.push(rule),
        }
    }
    providers_config.insert("providerRules".to_owned(), Value::Array(providers));
    models_config.insert("providerModelRules".to_owned(), Value::Array(models));
    models_config.insert("manualProviderModelRules".to_owned(), Value::Array(manual));
    config.insert(
        "providerConfigRules".to_owned(),
        Value::Object(providers_config),
    );
    config.insert("modelConfigRules".to_owned(), Value::Object(models_config));

    let updated = serde_json::to_string(&doc).context("serialize provider config rules")?;
    if original.as_deref() == Some(updated.as_str()) {
        return Ok(());
    }
    // ZCode keeps this file private
    crate::config::atomic_write_secret_for_settings(path, updated.as_bytes())
}

// list is the array found under the keys' path, empty when anything along
// the way is missing.
fn list<'a>(doc: &'a Value, keys: &[&str]) -> &'a [Value] {
    let mut value = doc;
    for key in keys {
        let Some(next) = value.get(key) else {
            return &[];
        };
        value = next;
    }
    value.as_array().map_or(&[][..], Vec::as_slice)
}

// object_at is the object at key of parent, made an empty object first when
// it isn't one: ZCode's own keys are kept, its shapes are imposed.
fn object_at<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !parent.get(key).is_some_and(Value::is_object) {
        parent.insert(key.to_owned(), json!({}));
    }
    parent
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("object_at leaves the value an object")
}

// take_object is the object at key of parent, an empty object when it isn't
// one, taken out to be changed and put back whole.
fn take_object(parent: &mut Map<String, Value>, key: &str) -> Map<String, Value> {
    match parent.get_mut(key).map(Value::take) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::{ALL_AGENTS, Agent, testing};
    use std::fs;

    fn agent(path: &Path) -> Agent {
        Agent {
            spec: ALL_AGENTS.iter().find(|spec| spec.id == "zcode").unwrap(),
            path: path.to_owned(),
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn provider_of(doc: &Value) -> Value {
        doc["provider"]["magpie"].clone()
    }

    #[test]
    fn the_provider_goes_into_both_files_and_comes_out_of_both() {
        let home = testing::isolation();
        let path = home.path().join(".zcode/v2/config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"provider":{"builtin:bigmodel":{"name":"Bigmodel","kind":"anthropic","enabled":true}}}"#,
        )
        .unwrap();
        let zcode = agent(&path);
        assert!(zcode.is_detected());
        assert_eq!(zcode.values().unwrap(), vec![("provider", String::new())]);

        zcode.set("provider", "magpie").unwrap();
        let magpie = provider_of(&read(&path));
        assert_eq!(magpie["kind"], json!("anthropic"));
        assert_eq!(magpie["enabled"], json!(true));
        assert_eq!(magpie["source"], json!("custom"));
        assert_eq!(magpie["options"]["apiKey"], json!("magpie"));
        assert_eq!(magpie["options"]["baseURL"], json!(crate::gateway::url()));
        let model = &magpie["models"]["deepseek/pro"];
        assert!(model["limit"]["context"].is_u64());
        assert_eq!(model["modalities"]["input"], json!(["text"]));
        assert!(magpie["models"].is_object());
        // ZCode's own provider went nowhere
        assert!(read(&path)["provider"]["builtin:bigmodel"].is_object());
        assert_eq!(
            zcode.values().unwrap(),
            vec![("provider", "magpie".to_owned())]
        );

        // ZCode 3.14 reads provider_config.json
        let rules = rules_path(&path);
        fs::write(
            &rules,
            r#"{"schemaVersion":1,"config":{"providerConfigRules":{"providerRules":[{"providerId":"mine","config":{}}]},"modelConfigRules":{"providerModelRules":[],"manualProviderModelRules":[{"providerId":"magpie","modelId":"deepseek/pro","config":{"enabled":true}}]}},"other":1}"#,
        )
        .unwrap();
        zcode.set("provider", "magpie").unwrap();
        let doc = read(&rules);
        assert_eq!(doc["other"], json!(1));
        let provider_rules = doc["config"]["providerConfigRules"]["providerRules"]
            .as_array()
            .unwrap();
        assert_eq!(provider_rules.len(), 2);
        assert_eq!(provider_rules[0]["providerId"], json!("mine"));
        let rule = &provider_rules[1];
        assert_eq!(rule["providerId"], json!("magpie"));
        assert_eq!(rule["enabled"], json!(true));
        assert_eq!(rule["config"]["group"], json!("standard-personal"));
        assert_eq!(
            rule["config"]["access"],
            json!({"type": "api-key", "apiKey": "magpie"})
        );
        assert_eq!(
            rule["config"]["api"],
            json!({"type": "anthropic-messages", "baseUrl": crate::gateway::url()})
        );
        assert_eq!(
            rule["config"]["personalModelIds"],
            json!(["deepseek/pro", "deepseek/flash"])
        );
        // a model set by hand in ZCode keeps its manual rule, and gets no
        // second one; the other catalog model gets one of magpie's
        let model_rules = &doc["config"]["modelConfigRules"];
        assert_eq!(
            model_rules["manualProviderModelRules"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let auto_rules = model_rules["providerModelRules"].as_array().unwrap();
        assert_eq!(auto_rules.len(), 1);
        assert_eq!(auto_rules[0]["modelId"], json!("deepseek/flash"));

        // a provider added later reaches ZCode's picker
        testing::set_catalog(vec![
            testing::model("deepseek/pro", "pro · DeepSeek"),
            testing::model("kimi/k2", "k2 · Kimi"),
        ]);
        zcode.sync().unwrap();
        assert!(provider_of(&read(&path))["models"]["kimi/k2"].is_object());
        let doc = read(&rules);
        assert_eq!(
            doc["config"]["providerConfigRules"]["providerRules"][1]["config"]["personalModelIds"],
            json!(["deepseek/pro", "kimi/k2"])
        );
        let model_rules = doc["config"]["modelConfigRules"]["providerModelRules"]
            .as_array()
            .unwrap();
        assert_eq!(model_rules.len(), 1);
        assert_eq!(model_rules[0]["modelId"], json!("kimi/k2"));

        // taken out again: both files are clean, ZCode's rules stay
        zcode.set("provider", "").unwrap();
        assert!(provider_of(&read(&path)).is_null());
        assert_eq!(zcode.values().unwrap(), vec![("provider", String::new())]);
        let doc = read(&rules);
        assert_eq!(
            doc["config"]["providerConfigRules"]["providerRules"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            doc["config"]["modelConfigRules"]["manualProviderModelRules"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        // nothing to sync into a config that has no magpie provider
        let before = fs::read_to_string(&path).unwrap();
        zcode.sync().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert!(provider_of(&read(&path)).is_null());
    }

    // What the user did in ZCode stays: magpie turned off stays off, in
    // both files; its rule stays where it is among theirs; and a rule they
    // removed isn't put back when magpie syncs.
    #[test]
    fn zcodes_own_choices_stay() {
        let home = testing::isolation();
        let path = home.path().join(".zcode/v2/config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"provider":{"magpie":{"name":"magpie","kind":"anthropic","enabled":false}}}"#,
        )
        .unwrap();
        let rules = rules_path(&path);
        fs::write(
            &rules,
            r#"{"schemaVersion":1,"config":{"providerConfigRules":{"providerRules":[
                {"providerId":"a","config":{}},{"providerId":"magpie","enabled":false,"config":{}},{"providerId":"b","config":{}}]}}}"#,
        )
        .unwrap();
        let zcode = agent(&path);
        zcode.sync().unwrap();

        let doc = read(&rules);
        let provider_rules = doc["config"]["providerConfigRules"]["providerRules"]
            .as_array()
            .unwrap();
        let ids = provider_rules
            .iter()
            .map(|rule| rule["providerId"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["a", "magpie", "b"]);
        assert_eq!(provider_rules[1]["enabled"], json!(false));
        let magpie = provider_of(&read(&path));
        assert_eq!(magpie["enabled"], json!(false));
        assert!(magpie["models"].is_object());

        // removed in ZCode: not put back
        fs::write(
            &rules,
            r#"{"schemaVersion":1,"config":{"providerConfigRules":{"providerRules":[{"providerId":"a","config":{}}]}}}"#,
        )
        .unwrap();
        zcode.sync().unwrap();
        let doc = read(&rules);
        assert_eq!(
            doc["config"]["providerConfigRules"]["providerRules"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
