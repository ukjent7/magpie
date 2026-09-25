use std::{fs, path::Path, str::FromStr};

use anyhow::{Context, Result, bail};
use jsonc_parser::{ParseOptions, cst::CstRootNode};
use toml_edit::{DocumentMut, Item, Table};
use yaml_edit::{Document, path::YamlPath};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigFormat {
    Jsonc,
    Toml,
    Yaml,
}

pub fn get(path: &Path, format: ConfigFormat, key_path: &str) -> Result<Option<String>> {
    let Some(text) = read_optional(path)? else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }

    match format {
        ConfigFormat::Jsonc => get_jsonc(&text, key_path),
        ConfigFormat::Toml => get_toml(&text, key_path),
        ConfigFormat::Yaml => get_yaml(&text, key_path),
    }
}

pub fn set(path: &Path, format: ConfigFormat, key_path: &str, value: &str) -> Result<()> {
    set_many(path, format, &[(key_path, value)])
}

pub fn set_many(path: &Path, format: ConfigFormat, assignments: &[(&str, &str)]) -> Result<()> {
    let text = read_optional(path)?.unwrap_or_default();
    let updated = match format {
        ConfigFormat::Jsonc => set_jsonc_many(&text, assignments)?,
        ConfigFormat::Toml => set_toml_many(&text, assignments)?,
        ConfigFormat::Yaml => set_yaml_many(&text, assignments)?,
    };
    write_atomic(path, updated.as_bytes())
}

pub fn delete(path: &Path, format: ConfigFormat, key_path: &str) -> Result<()> {
    delete_many(path, format, &[key_path])
}

pub fn delete_many(path: &Path, format: ConfigFormat, key_paths: &[&str]) -> Result<()> {
    let Some(text) = read_optional(path)? else {
        return Ok(());
    };
    if text.trim().is_empty() {
        return Ok(());
    }

    let updated = match format {
        ConfigFormat::Jsonc => delete_jsonc_many(&text, key_paths)?,
        ConfigFormat::Toml => delete_toml_many(&text, key_paths)?,
        ConfigFormat::Yaml => delete_yaml_many(&text, key_paths)?,
    };
    if let Some(updated) = updated {
        write_atomic(path, updated.as_bytes())?;
    }
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn key_parts(path: &str) -> Result<Vec<&str>> {
    let parts = path.split('.').collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        bail!("configuration key path must be a non-empty dotted path: {path:?}");
    }
    Ok(parts)
}

fn get_jsonc(text: &str, key_path: &str) -> Result<Option<String>> {
    let root = CstRootNode::parse(text, &ParseOptions::default()).context("parse JSONC")?;
    let Some(mut value) = root.value().and_then(|node| node.to_serde_value()) else {
        return Ok(None);
    };
    for key in key_parts(key_path)? {
        let Some(next) = value.get(key) else {
            return Ok(None);
        };
        value = next.clone();
    }
    Ok(Some(match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }))
}

fn set_jsonc_many(text: &str, assignments: &[(&str, &str)]) -> Result<String> {
    use jsonc_parser::cst::CstInputValue;

    let root = if text.trim().is_empty() {
        CstRootNode::parse("{}\n", &ParseOptions::default()).context("create JSONC document")?
    } else {
        CstRootNode::parse(text, &ParseOptions::default()).context("parse JSONC")?
    };
    let Some(root_object) = root.object_value_or_create() else {
        bail!("top level is not a JSON object");
    };
    for (key_path, value) in assignments {
        let parts = key_parts(key_path)?;
        let mut object = root_object.clone();
        for key in &parts[..parts.len() - 1] {
            object = object.object_value_or_set(key);
        }
        let last = parts[parts.len() - 1];
        let new_value = CstInputValue::String((*value).to_owned());
        if let Some(property) = object.get(last) {
            property.set_value(new_value);
        } else {
            object.append(last, new_value);
        }
    }
    Ok(root.to_string())
}

fn delete_jsonc_many(text: &str, key_paths: &[&str]) -> Result<Option<String>> {
    let root = CstRootNode::parse(text, &ParseOptions::default()).context("parse JSONC")?;
    let Some(root_object) = root.object_value() else {
        return Ok(None);
    };
    let mut changed = false;
    for key_path in key_paths {
        let parts = key_parts(key_path)?;
        let mut object = root_object.clone();
        let mut found = true;
        for key in &parts[..parts.len() - 1] {
            let Some(nested) = object.object_value(key) else {
                found = false;
                break;
            };
            object = nested;
        }
        if !found {
            continue;
        }
        if let Some(property) = object.get(parts[parts.len() - 1]) {
            property.remove();
            changed = true;
        }
    }
    Ok(changed.then(|| root.to_string()))
}

fn get_toml(text: &str, key_path: &str) -> Result<Option<String>> {
    let document = DocumentMut::from_str(text).context("parse TOML")?;
    let parts = key_parts(key_path)?;
    let mut table = document.as_table();
    for (index, key) in parts.iter().enumerate() {
        let Some(item) = table.get(key) else {
            return Ok(None);
        };
        if index + 1 == parts.len() {
            return Ok(item
                .as_str()
                .map(str::to_owned)
                .or_else(|| item.as_value().map(ToString::to_string)));
        }
        let Some(nested) = item.as_table() else {
            return Ok(None);
        };
        table = nested;
    }
    Ok(None)
}

fn set_toml_many(text: &str, assignments: &[(&str, &str)]) -> Result<String> {
    let mut document = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        DocumentMut::from_str(text).context("parse TOML")?
    };
    for (key_path, value) in assignments {
        let parts = key_parts(key_path)?;
        let mut table = document.as_table_mut();
        for key in &parts[..parts.len() - 1] {
            let item = table
                .entry(*key)
                .or_insert_with(|| Item::Table(Table::new()));
            table = item
                .as_table_mut()
                .with_context(|| format!("{key:?} in {key_path:?} is not a TOML table"))?;
        }
        table.insert(parts[parts.len() - 1], (*value).into());
    }
    Ok(document.to_string())
}

fn delete_toml_many(text: &str, key_paths: &[&str]) -> Result<Option<String>> {
    let mut document = DocumentMut::from_str(text).context("parse TOML")?;
    let mut changed = false;
    for key_path in key_paths {
        let parts = key_parts(key_path)?;
        changed |= remove_toml_path(document.as_table_mut(), &parts);
    }
    Ok(changed.then(|| document.to_string()))
}

fn remove_toml_path(table: &mut Table, parts: &[&str]) -> bool {
    let Some((head, tail)) = parts.split_first() else {
        return false;
    };
    if tail.is_empty() {
        return table.remove(*head).is_some();
    }

    table
        .get_mut(*head)
        .and_then(Item::as_table_mut)
        .is_some_and(|nested| remove_toml_path(nested, tail))
}

fn get_yaml(text: &str, key_path: &str) -> Result<Option<String>> {
    let document = Document::from_str(text).context("parse YAML")?;
    let Some(node) = document.try_get_path(key_path).ok() else {
        return Ok(None);
    };
    Ok(node.as_scalar().map(|scalar| scalar.as_string()))
}

fn set_yaml_many(text: &str, assignments: &[(&str, &str)]) -> Result<String> {
    let document = if text.trim().is_empty() {
        Document::new_mapping()
    } else {
        Document::from_str(text).context("parse YAML")?
    };
    for (key_path, value) in assignments {
        document
            .try_set_path(key_path, *value)
            .with_context(|| format!("set YAML field {key_path:?}"))?;
    }
    Ok(document.to_string())
}

fn delete_yaml_many(text: &str, key_paths: &[&str]) -> Result<Option<String>> {
    let document = Document::from_str(text).context("parse YAML")?;
    let mut changed = false;
    for key_path in key_paths {
        changed |= document.try_remove_path(key_path).is_ok();
    }
    Ok(changed.then(|| document.to_string()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    #[cfg(unix)]
    let mode = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777);

    let file_name = path
        .file_name()
        .context("configuration path has no file name")?;
    let mut temp_path = None;
    for _ in 0..16 {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            id
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                let mut result = file.write_all(bytes).map_err(anyhow::Error::from);
                #[cfg(unix)]
                if result.is_ok() {
                    if let Some(mode) = mode {
                        result = file
                            .set_permissions(fs::Permissions::from_mode(mode))
                            .with_context(|| format!("preserve permissions on {}", path.display()));
                    }
                }
                if result.is_ok() {
                    result = file.sync_all().map_err(anyhow::Error::from);
                }
                if let Err(error) = result {
                    drop(file);
                    let _ = fs::remove_file(&candidate);
                    return Err(error).with_context(|| format!("write {}", candidate.display()));
                }
                temp_path = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()));
            }
        }
    }
    let temp_path = temp_path.context("could not allocate a temporary configuration file")?;
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("replace {}", path.display()));
    }
    Ok(())
}

pub(crate) fn atomic_write_for_settings(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_edits_keep_comments_and_unrelated_values() {
        let original = "{\n  // keep this note\n  \"model\": \"old\",\n  \"other\": true\n}\n";
        let updated = set_jsonc_many(original, &[("model", "new")]).unwrap();

        assert!(updated.contains("// keep this note"));
        assert!(updated.contains("\"other\": true"));
        assert_eq!(
            get_jsonc(&updated, "model").unwrap().as_deref(),
            Some("new")
        );
    }

    #[test]
    fn nested_yaml_edit_keeps_inline_comment() {
        let original = "model:\n  name: old # keep this note\n";
        let updated = set_yaml_many(original, &[("model.name", "new")]).unwrap();

        assert!(updated.contains("# keep this note"));
        assert_eq!(
            get_yaml(&updated, "model.name").unwrap().as_deref(),
            Some("new")
        );
    }
}
