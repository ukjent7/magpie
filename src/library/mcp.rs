// The MCP servers: one definition written into each agent's own file in
// that agent's own format, taking back only what magpie wrote.

use std::collections::BTreeMap;
use std::{path::Path, str::FromStr};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use toml_edit::{DocumentMut, Entry, InlineTable, Item, Table};

use crate::config::{self, atomic_write_for_settings};
use crate::library::{
    Backups, Homes, Library, Store, SyncResult, Target, change_in, is_name, read_optional, store,
};

// Server is one MCP server: a command magpie's agents start, or a URL they
// reach.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Server {
    pub name: String,
    // Transport is stdio for a command, http (streamable) or sse for a URL.
    pub transport: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub agents: Vec<String>,
}

impl Server {
    // Remote reports whether the server is reached by URL.
    pub fn remote(&self) -> bool {
        self.transport == "http" || self.transport == "sse"
    }

    pub(crate) fn check(&mut self) -> Result<()> {
        crate::library::check_name("server", &self.name)?;
        self.command = self.command.trim().to_owned();
        self.url = self.url.trim().to_owned();
        match self.transport.as_str() {
            "stdio" => {
                if self.command.is_empty() {
                    bail!("{}: a command is needed", self.name);
                }
                self.url = String::new();
                self.headers.clear();
            }
            "http" | "sse" => {
                if !self.url.starts_with("http://") && !self.url.starts_with("https://") {
                    bail!(
                        "{}: the URL has to start with http:// or https://",
                        self.name
                    );
                }
                self.command = String::new();
                self.args = Vec::new();
                self.env.clear();
            }
            other => bail!("{}: unknown transport {other:?}", self.name),
        }
        drop_blank(&mut self.env);
        drop_blank(&mut self.headers);
        Ok(())
    }

    // same reports whether two definitions start the same server; which
    // agents have it doesn't matter.
    pub fn same(&self, o: &Server) -> bool {
        self.transport == o.transport
            && self.command == o.command
            && self.args == o.args
            && self.env == o.env
            && self.url == o.url
            && self.headers == o.headers
    }
}

fn drop_blank(map: &mut BTreeMap<String, String>) {
    let blank = map
        .keys()
        .filter(|k| k.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>();
    for key in blank {
        map.remove(&key);
    }
}

// ---- each agent's own format -----------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Format {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Cursor,
    Copilot,
    Crush,
    Goose,
    Pi,
    Desktop,
    ZCode,
}

// McpFile is the file an agent keeps its user-wide MCP servers in.
#[derive(Clone, Debug)]
pub(crate) struct McpFile {
    pub(crate) path: std::path::PathBuf,
    pub(crate) format: Format,
}

// key is the object the servers are kept under.
fn key(format: Format) -> &'static str {
    match format {
        Format::Codex => "mcp_servers",
        Format::OpenCode | Format::Crush => "mcp",
        Format::Goose => "extensions",
        Format::ZCode => "mcp.servers",
        _ => "mcpServers",
    }
}

// supports says why the agent can't reach a server, or none when it can.
pub(crate) fn supports(format: &Format, transport: &str) -> Option<&'static str> {
    if transport == "sse" && matches!(format, Format::Codex | Format::Goose) {
        // what the page says of an agent that can't reach a server over SSE
        return Some("no-sse");
    }
    if matches!(transport, "http" | "sse") && *format == Format::Desktop {
        // an app that reaches only a server it runs itself (Claude Desktop,
        // whose remote ones are its Connectors)
        return Some("no-remote");
    }
    None
}

// encode is the server as this agent writes it.
fn encode(format: Format, s: &Server) -> Value {
    let optional_map =
        |object: &mut serde_json::Map<String, Value>, key: &str, m: &BTreeMap<String, String>| {
            if !m.is_empty() {
                object.insert(
                    key.to_owned(),
                    Value::Object(
                        m.iter()
                            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                            .collect(),
                    ),
                );
            }
        };
    let list = |a: &[String]| -> Value {
        Value::Array(a.iter().map(|v| Value::String(v.clone())).collect())
    };
    let str_map = |m: &BTreeMap<String, String>| -> Value {
        Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )
    };
    let mut o = serde_json::Map::new();
    match format {
        Format::Claude | Format::Crush | Format::ZCode => {
            o.insert("type".to_owned(), Value::String(s.transport.clone()));
            if s.remote() {
                o.insert("url".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "headers", &s.headers);
            } else {
                o.insert("command".to_owned(), Value::String(s.command.clone()));
                o.insert("args".to_owned(), list(&s.args));
                o.insert("env".to_owned(), str_map(&s.env));
            }
        }
        Format::Gemini => match s.transport.as_str() {
            "http" => {
                o.insert("httpUrl".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "headers", &s.headers);
            }
            "sse" => {
                o.insert("url".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "headers", &s.headers);
            }
            _ => {
                o.insert("command".to_owned(), Value::String(s.command.clone()));
                o.insert("args".to_owned(), list(&s.args));
                optional_map(&mut o, "env", &s.env);
            }
        },
        Format::OpenCode => {
            if s.remote() {
                o.insert("type".to_owned(), Value::String("remote".to_owned()));
                o.insert("url".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "headers", &s.headers);
            } else {
                let mut command = vec![s.command.clone()];
                command.extend(s.args.iter().cloned());
                o.insert("type".to_owned(), Value::String("local".to_owned()));
                o.insert("command".to_owned(), list(&command));
                optional_map(&mut o, "environment", &s.env);
            }
            o.insert("enabled".to_owned(), Value::Bool(true));
        }
        Format::Cursor | Format::Desktop => {
            if s.remote() {
                o.insert("url".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "headers", &s.headers);
            } else {
                o.insert("command".to_owned(), Value::String(s.command.clone()));
                o.insert("args".to_owned(), list(&s.args));
                optional_map(&mut o, "env", &s.env);
            }
        }
        Format::Copilot => {
            if s.remote() {
                o.insert("type".to_owned(), Value::String(s.transport.clone()));
                o.insert("url".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "headers", &s.headers);
            } else {
                o.insert("type".to_owned(), Value::String("local".to_owned()));
                o.insert("command".to_owned(), Value::String(s.command.clone()));
                o.insert("args".to_owned(), list(&s.args));
                optional_map(&mut o, "env", &s.env);
            }
            o.insert(
                "tools".to_owned(),
                Value::Array(vec![Value::String("*".to_owned())]),
            );
        }
        Format::Goose => {
            o.insert("enabled".to_owned(), Value::Bool(true));
            o.insert("name".to_owned(), Value::String(s.name.clone()));
            match s.transport.as_str() {
                "http" => {
                    o.insert(
                        "type".to_owned(),
                        Value::String("streamable_http".to_owned()),
                    );
                    o.insert("uri".to_owned(), Value::String(s.url.clone()));
                    optional_map(&mut o, "headers", &s.headers);
                }
                "sse" => {
                    o.insert("type".to_owned(), Value::String("sse".to_owned()));
                    o.insert("uri".to_owned(), Value::String(s.url.clone()));
                }
                _ => {
                    o.insert("type".to_owned(), Value::String("stdio".to_owned()));
                    o.insert("cmd".to_owned(), Value::String(s.command.clone()));
                    o.insert("args".to_owned(), list(&s.args));
                    optional_map(&mut o, "envs", &s.env);
                }
            }
            o.insert("timeout".to_owned(), Value::Number(300.into()));
        }
        Format::Pi => {
            // pi-mcp-extension reads the transport from "transport",
            // pi-mcp-adapter from "httpTransport"; each ignores the other's
            if s.remote() {
                let t = if s.transport == "sse" {
                    "sse"
                } else {
                    "streamable-http"
                };
                o.insert("transport".to_owned(), Value::String(t.to_owned()));
                o.insert("httpTransport".to_owned(), Value::String(t.to_owned()));
                o.insert("url".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "headers", &s.headers);
            } else {
                o.insert("command".to_owned(), Value::String(s.command.clone()));
                o.insert("args".to_owned(), list(&s.args));
                optional_map(&mut o, "env", &s.env);
            }
        }
        Format::Codex => {
            if s.remote() {
                o.insert("url".to_owned(), Value::String(s.url.clone()));
                optional_map(&mut o, "http_headers", &s.headers);
            } else {
                o.insert("command".to_owned(), Value::String(s.command.clone()));
                o.insert("args".to_owned(), list(&s.args));
                optional_map(&mut o, "env", &s.env);
            }
        }
    }
    Value::Object(o)
}

fn value_str(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn to_string_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn object_to_map(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, x)| (k.clone(), to_string_value(x)))
                .collect()
        })
        .unwrap_or_default()
}

fn value_list_at(v: &Value, key: &str) -> Vec<String> {
    v.get(key).map(list_items).unwrap_or_default()
}

fn list_items(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(to_string_value).collect())
        .unwrap_or_default()
}

// remote and local are the two shapes an agent's entry can take: a URL the
// agent reaches, or a command it starts.
fn remote(s: &mut Server, transport: &str, url: String, headers: &Value) {
    s.transport = transport.to_owned();
    s.url = url;
    s.headers = object_to_map(headers);
}

fn local(s: &mut Server, cmd: String, args: &Value, env: &Value) {
    s.transport = "stdio".to_owned();
    s.command = cmd;
    s.args = list_items(args);
    s.env = object_to_map(env);
}

// decode reads one of the agent's entries; none for one magpie can't read
// as a server (a Goose builtin, say).
fn decode(format: Format, name: &str, m: &Value) -> Option<Server> {
    let mut s = Server {
        name: name.to_owned(),
        ..Server::default()
    };
    match format {
        Format::OpenCode => {
            if value_str(m, "type") == "remote" {
                remote(
                    &mut s,
                    "http",
                    value_str(m, "url"),
                    m.get("headers").unwrap_or(&Value::Null),
                );
            } else {
                let command = value_list_at(m, "command");
                if let Some((first, rest)) = command.split_first() {
                    local(
                        &mut s,
                        first.clone(),
                        &Value::Array(rest.iter().cloned().map(Value::String).collect()),
                        m.get("environment").unwrap_or(&Value::Null),
                    );
                }
            }
        }
        Format::Goose => match value_str(m, "type").as_str() {
            "stdio" => {
                local(
                    &mut s,
                    value_str(m, "cmd"),
                    m.get("args").unwrap_or(&Value::Null),
                    m.get("envs").unwrap_or(&Value::Null),
                );
            }
            "streamable_http" => {
                remote(
                    &mut s,
                    "http",
                    value_str(m, "uri"),
                    m.get("headers").unwrap_or(&Value::Null),
                );
            }
            "sse" => {
                remote(
                    &mut s,
                    "sse",
                    value_str(m, "uri"),
                    m.get("headers").unwrap_or(&Value::Null),
                );
            }
            _ => {}
        },
        Format::Codex => {
            let url = value_str(m, "url");
            if !url.is_empty() {
                remote(
                    &mut s,
                    "http",
                    url,
                    m.get("http_headers").unwrap_or(&Value::Null),
                );
            } else {
                local(
                    &mut s,
                    value_str(m, "command"),
                    m.get("args").unwrap_or(&Value::Null),
                    m.get("env").unwrap_or(&Value::Null),
                );
            }
        }
        Format::Gemini => {
            let http_url = value_str(m, "httpUrl");
            let url = value_str(m, "url");
            if !http_url.is_empty() {
                remote(
                    &mut s,
                    "http",
                    http_url,
                    m.get("headers").unwrap_or(&Value::Null),
                );
            } else if !url.is_empty() {
                let t = if value_str(m, "type") == "http" {
                    "http"
                } else {
                    "sse"
                };
                remote(&mut s, t, url, m.get("headers").unwrap_or(&Value::Null));
            } else {
                local(
                    &mut s,
                    value_str(m, "command"),
                    m.get("args").unwrap_or(&Value::Null),
                    m.get("env").unwrap_or(&Value::Null),
                );
            }
        }
        Format::Pi => {
            let url = value_str(m, "url");
            if !url.is_empty() {
                let t = if value_str(m, "transport") == "sse"
                    || value_str(m, "httpTransport") == "sse"
                {
                    "sse"
                } else {
                    "http"
                };
                remote(&mut s, t, url, m.get("headers").unwrap_or(&Value::Null));
            } else {
                local(
                    &mut s,
                    value_str(m, "command"),
                    m.get("args").unwrap_or(&Value::Null),
                    m.get("env").unwrap_or(&Value::Null),
                );
            }
        }
        // Claude Code, Cursor, Copilot, Crush, ZCode, Desktop
        _ => {
            let mut t = value_str(m, "type");
            let url = value_str(m, "url");
            if !url.is_empty() {
                if t != "sse" {
                    t = "http".to_owned();
                }
                remote(&mut s, &t, url, m.get("headers").unwrap_or(&Value::Null));
            } else {
                local(
                    &mut s,
                    value_str(m, "command"),
                    m.get("args").unwrap_or(&Value::Null),
                    m.get("env").unwrap_or(&Value::Null),
                );
            }
        }
    }
    if s.transport.is_empty()
        || (s.transport == "stdio" && s.command.is_empty())
        || (s.remote() && s.url.is_empty())
    {
        return None;
    }
    Some(s)
}

// entries is every entry under the servers' key, as the file has it.
pub(crate) fn entries(f: &McpFile) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    let Some(raw) = read_optional(&f.path)? else {
        return Ok(out);
    };
    if raw.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(out);
    }
    let doc = match f.format {
        Format::Codex => toml_document(&raw)?,
        Format::Goose => yaml_document(&raw)?,
        _ => jsonc_document(&raw)?,
    };
    // the key can be a path: ZCode keeps them in mcp.servers
    let mut all = &doc;
    for part in key(f.format).split('.') {
        let Some(next) = all.get(part) else {
            return Ok(out);
        };
        all = next;
    }
    if let Some(object) = all.as_object() {
        for (name, v) in object {
            if v.is_object() {
                out.insert(name.clone(), v.clone());
            }
        }
    }
    Ok(out)
}

fn jsonc_document(raw: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(raw);
    let root = jsonc_parser::cst::CstRootNode::parse(&text, &jsonc_parser::ParseOptions::default())
        .context("parse JSONC")?;
    Ok(root
        .value()
        .and_then(|node| node.to_serde_value())
        .unwrap_or(Value::Null))
}

fn yaml_document(raw: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(raw);
    let document = yaml_edit::Document::from_str(&text).context("parse YAML")?;
    let value = yaml_edit::YamlValue::from_document(&document);
    Ok(yaml_to_json(&value))
}

fn yaml_to_json(v: &yaml_edit::YamlValue) -> Value {
    match v {
        yaml_edit::YamlValue::Scalar(s) => {
            if let Some(b) = s.to_bool() {
                Value::Bool(b)
            } else if let Some(i) = s.to_i64() {
                Value::Number(i.into())
            } else if let Some(f) = s.to_f64() {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            } else if s.style() == yaml_edit::ScalarStyle::Plain
                && matches!(s.value(), "" | "~" | "null" | "Null" | "NULL")
            {
                Value::Null
            } else {
                Value::String(s.value().to_owned())
            }
        }
        yaml_edit::YamlValue::Sequence(items) => {
            Value::Array(items.iter().map(yaml_to_json).collect())
        }
        yaml_edit::YamlValue::Mapping(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), yaml_to_json(v)))
                .collect(),
        ),
        yaml_edit::YamlValue::Set(s) => {
            Value::Array(s.iter().map(|k| Value::String(k.clone())).collect())
        }
        yaml_edit::YamlValue::OrderedMapping(pairs) | yaml_edit::YamlValue::Pairs(pairs) => {
            Value::Object(
                pairs
                    .iter()
                    .map(|(k, v)| (k.clone(), yaml_to_json(v)))
                    .collect(),
            )
        }
    }
}

fn toml_document(raw: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(raw);
    let document = DocumentMut::from_str(&text).context("parse TOML")?;
    Ok(toml_table_to_json(document.as_table().iter()))
}

fn item_to_value(item: &Item) -> Value {
    match item {
        Item::Value(v) => toml_value_to_json(v),
        Item::Table(t) => toml_table_to_json(t.iter()),
        Item::ArrayOfTables(a) => {
            Value::Array(a.iter().map(|t| toml_table_to_json(t.iter())).collect())
        }
        Item::None => Value::Null,
    }
}

fn toml_table_to_json<'a>(entries: impl Iterator<Item = (&'a str, &'a Item)>) -> Value {
    Value::Object(
        entries
            .map(|(k, v)| (k.to_owned(), item_to_value(v)))
            .collect(),
    )
}

fn toml_value_to_json(v: &toml_edit::Value) -> Value {
    match v {
        toml_edit::Value::String(s) => Value::String(s.value().to_owned()),
        toml_edit::Value::Integer(i) => Value::Number((*i.value()).into()),
        toml_edit::Value::Float(f) => serde_json::Number::from_f64(*f.value())
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml_edit::Value::Boolean(b) => Value::Bool(*b.value()),
        toml_edit::Value::Datetime(d) => Value::String(d.to_string()),
        toml_edit::Value::Array(a) => Value::Array(a.iter().map(toml_value_to_json).collect()),
        toml_edit::Value::InlineTable(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.to_owned(), toml_value_to_json(v)))
                .collect(),
        ),
    }
}

fn json_to_toml_value(v: &Value) -> toml_edit::Value {
    match v {
        Value::String(s) => toml_edit::Value::from(s.as_str()),
        Value::Bool(b) => toml_edit::Value::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml_edit::Value::from(i)
            } else {
                toml_edit::Value::from(n.as_f64().unwrap_or_default())
            }
        }
        Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(json_to_toml_value(item));
            }
            toml_edit::Value::Array(array)
        }
        Value::Object(items) => {
            let mut table = InlineTable::new();
            for (k, item) in items {
                table.insert(k, json_to_toml_value(item));
            }
            toml_edit::Value::InlineTable(table)
        }
        Value::Null => toml_edit::Value::from(""),
    }
}

// read is every server in the file, magpie's or not, by name.
pub(crate) fn read(f: &McpFile) -> Result<BTreeMap<String, Server>> {
    let entries = entries(f)?;
    Ok(entries
        .iter()
        .filter_map(|(name, v)| decode(f.format, name, v).map(|s| (name.clone(), s)))
        .collect())
}

// owned are the keys of an entry that say what the server is: magpie writes
// them. An entry's other keys — a timeout, the tools it may call, a note —
// are the user's, and kept when magpie writes the server again.
fn owned(format: Format) -> &'static [&'static str] {
    match format {
        Format::Gemini => &[
            "type", "httpUrl", "url", "headers", "command", "args", "env",
        ],
        Format::OpenCode => &[
            "type",
            "url",
            "headers",
            "command",
            "environment",
            "enabled",
        ],
        Format::Goose => &[
            "enabled", "name", "type", "uri", "headers", "cmd", "args", "envs",
        ],
        Format::Codex => &["url", "http_headers", "command", "args", "env"],
        Format::Pi => &[
            "transport",
            "httpTransport",
            "url",
            "headers",
            "command",
            "args",
            "env",
        ],
        _ => &["type", "url", "headers", "command", "args", "env"],
    }
}

// merged is the entry magpie writes, with what the user added to the old
// one kept; a default magpie gives (Copilot's tools, Goose's timeout)
// yields to the user's.
fn merged(format: Format, s: &Server, old: Option<&Value>) -> Value {
    let mut o = match encode(format, s) {
        Value::Object(o) => o,
        _ => serde_json::Map::new(),
    };
    let Some(old) = old.and_then(Value::as_object) else {
        return Value::Object(o);
    };
    let mine = owned(format);
    for (k, v) in o.iter_mut() {
        if !mine.contains(&k.as_str())
            && let Some(old_value) = old.get(k)
        {
            *v = old_value.clone();
        }
    }
    for (k, v) in old {
        if mine.contains(&k.as_str()) || o.contains_key(k) {
            continue;
        }
        o.insert(k.clone(), v.clone());
    }
    Value::Object(o)
}

// put writes the server into the file, in place of one by its name.
fn put(f: &McpFile, s: &Server, old: Option<&Value>) -> Result<()> {
    let entry = merged(f.format, s, old);
    match f.format {
        Format::Codex => codex_put(&f.path, &s.name, &entry),
        Format::Goose => {
            config::set_yaml_values(&f.path, &[(&format!("extensions.{}", s.name), entry)])
        }
        _ => config::set_jsonc_value(&f.path, &format!("{}.{}", key(f.format), s.name), &entry),
    }
}

// del takes the server by that name out of the file.
fn del(f: &McpFile, name: &str) -> Result<()> {
    match f.format {
        Format::Codex => codex_del(&f.path, name),
        Format::Goose => config::delete(
            &f.path,
            config::ConfigFormat::Yaml,
            &format!("extensions.{name}"),
        ),
        _ => config::delete(
            &f.path,
            config::ConfigFormat::Jsonc,
            &format!("{}.{}", key(f.format), name),
        ),
    }
}

// ---- Codex's TOML ----------------------------------------------------------

fn codex_document(path: &Path) -> Result<Option<DocumentMut>> {
    let Some(raw) = read_optional(path)? else {
        return Ok(None);
    };
    if raw.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&raw);
    DocumentMut::from_str(&text)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

// codex_put writes the server's table, in place of one by that name; the
// tables under it (its env, its env_vars) go with it, what the user added
// comes back from merged.
fn codex_put(path: &Path, name: &str, entry: &Value) -> Result<()> {
    let mut doc = codex_document(path)?.unwrap_or_else(DocumentMut::new);
    let servers = doc
        .as_table_mut()
        .entry("mcp_servers")
        .or_insert(Item::Table({
            let mut t = Table::new();
            t.set_implicit(true);
            t
        }));
    let Some(servers) = servers.as_table_mut() else {
        bail!("mcp_servers in {} is not a table", path.display());
    };
    let mut table = Table::new();
    if let Some(object) = entry.as_object() {
        for (k, v) in object {
            table.insert(k, toml_edit::Item::Value(json_to_toml_value(v)));
        }
    }
    match servers.entry(name) {
        Entry::Vacant(entry) => {
            entry.insert(Item::Table(table));
        }
        Entry::Occupied(mut entry) => match entry.get_mut() {
            // the server's own table: only its values change, so that the
            // tables the user wrote after it keep their place in the file
            Item::Table(old) => {
                old.clear();
                for (key, item) in table {
                    old.insert(&key, item);
                }
                old.set_implicit(false);
            }
            item => {
                *item = Item::Table(table);
            }
        },
    }
    atomic_write_for_settings(path, doc.to_string().as_bytes())
}

// codex_del takes out the server's table and the tables under it ([…env],
// [[…env_vars]]), which would otherwise implicitly recreate the server.
fn codex_del(path: &Path, name: &str) -> Result<()> {
    let Some(mut doc) = codex_document(path)? else {
        return Ok(());
    };
    let mut changed = false;
    // the blank line that sets a table apart is its own: once the table that
    // opened the file is gone, its successor must not lead with that blank line
    let mut opened = false;
    if let Some(servers) = doc
        .as_table_mut()
        .get_mut("mcp_servers")
        .and_then(Item::as_table_mut)
        && let Some(removed) = servers.remove(name)
    {
        changed = true;
        let sep = removed
            .as_table()
            .and_then(|t| t.decor().prefix())
            .and_then(|p| p.as_str())
            .unwrap_or_default();
        opened = !sep.contains('\n');
    }
    if !changed {
        return Ok(());
    }
    let text = doc.to_string();
    let text = if opened {
        text.trim_start_matches('\n')
    } else {
        text.as_str()
    };
    atomic_write_for_settings(path, text.as_bytes())
}

// ---- sync ------------------------------------------------------------------

pub(crate) fn sync_mcp(l: &mut Library, t: &Target, b: &mut Backups, res: &mut SyncResult) {
    let Some(f) = t.mcp.clone() else {
        return;
    };
    let id = t.agent.spec.id;
    let entries = match entries(&f) {
        Ok(entries) => entries,
        Err(error) => {
            res.fail(id, "mcp", &error);
            return;
        }
    };
    let mut mine = Vec::new();
    let had = l.applied.get(id).map(|a| a.mcp.clone()).unwrap_or_default();
    for name in had {
        let keep = match l.server(&name) {
            Some(s) => {
                s.agents.iter().any(|x| x == id) && supports(&f.format, &s.transport).is_none()
            }
            None => false,
        };
        if keep {
            mine.push(name);
            continue;
        }
        if entries.contains_key(&name) {
            let what = format!("mcp:{name}");
            if let Err(error) = b.keep(id, &f.path).and_then(|()| del(&f, &name)) {
                res.fail(id, &what, &error);
                mine.push(name);
            } else {
                res.changed(id);
            }
        }
    }
    for s in &l.mcp {
        if !s.agents.iter().any(|x| x == id) {
            continue;
        }
        if let Some(why) = supports(&f.format, &s.transport) {
            res.fail(id, &format!("mcp:{}", s.name), &anyhow::anyhow!("{why}"));
            continue;
        }
        let old = entries.get(&s.name);
        if let Some(old) = old
            && let Some(cur) = decode(f.format, &s.name, old)
            && cur.same(s)
        {
            mine.push(s.name.clone());
            continue;
        }
        let what = format!("mcp:{}", s.name);
        if let Err(error) = b.keep(id, &f.path).and_then(|()| put(&f, s, old)) {
            res.fail(id, &what, &error);
            continue;
        }
        mine.push(s.name.clone());
        res.changed(id);
    }
    l.applied_mut(id).mcp = mine;
}

// ---- the library's own servers ---------------------------------------------

// SaveServer adds a server, or replaces the one called old (renaming it).
pub fn save_server(old: &str, s: Server) -> Result<SyncResult> {
    save_server_at(&store(), &Homes::detect(), old, s)
}

pub(crate) fn save_server_at(
    store: &Store,
    homes: &Homes,
    old: &str,
    mut s: Server,
) -> Result<SyncResult> {
    s.check()?;
    change_in(store, homes, move |l| {
        if s.name != old && l.server(&s.name).is_some() {
            bail!("the library already has a server called {}", s.name);
        }
        if !old.is_empty() {
            let i = l
                .mcp
                .iter()
                .position(|x| x.name == old)
                .with_context(|| format!("no server called {old}"))?;
            l.mcp.remove(i);
        }
        s.agents.sort();
        l.mcp.push(s);
        Ok(())
    })
}

// ServerAgents sets which agents get a server.
pub fn server_agents(name: &str, agents: Vec<String>) -> Result<SyncResult> {
    server_agents_at(&store(), &Homes::detect(), name, agents)
}

pub(crate) fn server_agents_at(
    store: &Store,
    homes: &Homes,
    name: &str,
    agents: Vec<String>,
) -> Result<SyncResult> {
    let name = name.to_owned();
    change_in(store, homes, move |l| {
        let s = l
            .server_mut(&name)
            .with_context(|| format!("no server called {name}"))?;
        s.agents = agents;
        s.agents.sort();
        Ok(())
    })
}

// RemoveServer takes a server out of the library and out of every agent
// magpie gave it to.
pub fn remove_server(name: &str) -> Result<SyncResult> {
    remove_server_at(&store(), &Homes::detect(), name)
}

pub(crate) fn remove_server_at(store: &Store, homes: &Homes, name: &str) -> Result<SyncResult> {
    let name = name.to_owned();
    change_in(store, homes, move |l| {
        let i = l
            .mcp
            .iter()
            .position(|x| x.name == name)
            .with_context(|| format!("no server called {name}"))?;
        l.mcp.remove(i);
        Ok(())
    })
}

// ImportServer takes a server the agents have into the library: the agents
// that have it as it is get the library's from then on, the same entry.
pub fn import_server(name: &str) -> Result<SyncResult> {
    import_server_at(&store(), &Homes::detect(), name)
}

pub(crate) fn import_server_at(store: &Store, homes: &Homes, name: &str) -> Result<SyncResult> {
    let name = name.to_owned();
    change_in(store, homes, move |l| {
        for f in found_servers(l, homes) {
            if f.server.name != name {
                continue;
            }
            let mut s = f.server;
            s.agents.sort();
            for id in &s.agents {
                let a = l.applied_mut(id);
                if !a.mcp.iter().any(|x| x == &name) {
                    a.mcp.push(name.clone());
                }
            }
            l.mcp.push(s);
            return Ok(());
        }
        bail!("no agent has a server called {name} that the library hasn't")
    })
}

// ---- found in agents -------------------------------------------------------

// Found is a server an agent has that the library doesn't: the agents that
// have it as it is, and those that have another by that name.
#[derive(Debug, Serialize)]
pub struct Found {
    pub server: Server,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    // Own: the agent's app puts it there itself, each time it starts (Codex's
    // node_repl and cua_repl): it isn't one to bring in, and taking it out
    // doesn't last.
    #[serde(rename = "own", default, skip_serializing_if = "is_false")]
    pub own: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

// appOwned is whether a server runs from inside an app's own install — a
// Mac app bundle, a Microsoft Store app, the Codex app's runtimes — as the
// servers an agent's app writes into its config itself do.
fn app_owned(s: &Server) -> bool {
    if s.remote() {
        return false;
    }
    let cmd = s.command.to_lowercase().replace('\\', "/");
    [
        ".app/contents/",
        "/windowsapps/",
        "/cua_node/",
        "/openai/codex/runtimes/",
    ]
    .iter()
    .any(|inside| cmd.contains(inside))
}

pub(crate) fn found_servers(l: &Library, homes: &Homes) -> Vec<Found> {
    let mut by_name: BTreeMap<String, Found> = BTreeMap::new();
    let mut names: Vec<String> = Vec::new();
    for t in &crate::library::targets(homes) {
        let Some(f) = &t.mcp else { continue };
        let Ok(have) = read(f) else {
            continue;
        };
        let mine = l
            .applied
            .get(t.agent.spec.id)
            .map(|a| a.mcp.clone())
            .unwrap_or_default();
        let id = t.agent.spec.id;
        for (name, mut s) in have {
            if mine.iter().any(|x| x == &name) || l.server(&name).is_some() || !is_name(&name) {
                continue;
            }
            match by_name.get_mut(&name) {
                None => {
                    s.agents = vec![id.to_owned()];
                    names.push(name.clone());
                    by_name.insert(
                        name,
                        Found {
                            own: app_owned(&s),
                            server: s,
                            others: Vec::new(),
                            icon: String::new(),
                        },
                    );
                }
                Some(found) if found.server.same(&s) => {
                    found.server.agents.push(id.to_owned());
                }
                Some(found) => found.others.push(id.to_owned()),
            }
        }
    }
    names
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .collect()
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
