// A group's rules: which member a request goes to first, by what can be
// seen in the request itself — how long it is, whether it carries an
// image, how hard the agent asked the model to think, which agent sent
// it, and, through the group's classifier, what the user's message asks
// for. A rule decides when a user's turn begins and what it decided holds
// for the rest of the turn (see ruleFor); the gateway keeps the state.

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use axum::http::HeaderMap;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    gateway::ApiProtocol,
    provider::{self, Group},
};

pub(crate) mod classify;

const GROUP_PREFIX: &str = "group/";

// Efforts are the reasoning levels a rule can ask for, lowest first.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

// MaxIntent is how long an intent may be, in characters.
pub const MAX_INTENT: usize = 200;

const RULE_USAGE: &str = "usage:
  magpie group rule <group>               the group's rules
  magpie group rule add <group> use=<model> [tokens=<n>] [images] [effort=on|low|medium|high|xhigh|max] [agents=a,b…]
                        [intent=\"<what the message asks for>\"] [classifier=<model>] [at=<n>]
                                          a rule: a turn that matches it goes to <model>, one of the group's,
                                          first
  magpie group rule rm <group> <n>        remove rule n
  magpie group rule mv <group> <n> <to>   move rule n to place <to>
  magpie group rule classifier <group> <model>
                                          the model that tells which intent a message is

  Rules are looked at top first when you send a message (a new turn); the first that
  matches puts its model first, and the group's others stay behind it if it fails.
  The agent's tool results within the turn stay with the model the turn began on.
  Every condition given must hold:
  tokens   the request is at least this long (200000, 200k, 1m): estimated from its size, or
           what the vendor counted the conversation's last request as, whichever is more
  images   it carries an image, now or earlier in the conversation
  effort   the agent asked for reasoning: on (any), or at least this level
  agents   it comes from one of these agents (claude, codex, opencode, … as magpie usage names them)
  intent   the user's message is of this kind, in your words (\"writing or fixing tests\", \"a quick
           question\"): as the turn begins, the group's classifier — any model magpie has, best a small
           fast one without reasoning — is asked which of the intents that may match the message is,
           once; if it fails or can't say, no intent matches. Its call shows in the usage as magpie's own

  e.g. magpie group rule add opus-anywhere use=openrouter/google/gemini-3-pro tokens=200k
       magpie group rule add opus-anywhere use=a/vision-model images
       magpie group rule add opus-anywhere use=deepseek/deepseek-v4-flash intent=\"a quick question\" classifier=groq/llama-3.1-8b-instant";

// Rule sends the requests it matches to one of its group's members first.
// Every condition set must hold; a rule sets at least one.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Rule {
    #[serde(rename = "use")]
    pub use_: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub tokens: i64,
    #[serde(skip_serializing_if = "is_false")]
    pub images: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub intent: String,
}

impl Rule {
    // Conditions says what a rule matches, for a list or a trace.
    pub fn conditions(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.tokens > 0 {
            out.push(format!("tokens ≥ {}", self.tokens));
        }
        if self.images {
            out.push("images".to_owned());
        }
        match self.effort.as_str() {
            "" => {}
            "on" => out.push("reasoning".to_owned()),
            effort => out.push(format!("effort ≥ {effort}")),
        }
        if !self.agents.is_empty() {
            out.push(format!("agent {}", self.agents.join("|")));
        }
        if !self.intent.is_empty() {
            out.push(format!("intent {:?}", self.intent));
        }
        out
    }

    // Matches reports whether the request is one the rule is for.
    pub fn matches(&self, q: &RuleRequest) -> bool {
        if !self.intent.is_empty() && !self.intent.eq_ignore_ascii_case(&q.intent) {
            return false;
        }
        self.matches_besides_intent(q)
    }

    // MatchesBesidesIntent is Matches but for the rule's intent: whether
    // the classifier's answer is all it waits on.
    pub fn matches_besides_intent(&self, q: &RuleRequest) -> bool {
        if self.tokens > 0 && q.tokens < self.tokens {
            return false;
        }
        if self.images && !q.images {
            return false;
        }
        match self.effort.as_str() {
            "" => {}
            "on" => {
                if !q.thinking && q.effort.is_empty() {
                    return false;
                }
            }
            level => {
                if effort_rank(&q.effort) < effort_rank(level) {
                    return false;
                }
            }
        }
        if !self.agents.is_empty() && !self.agents.contains(&q.agent) {
            return false;
        }
        !self.conditions().is_empty()
    }
}

fn effort_rank(effort: &str) -> i32 {
    EFFORTS
        .iter()
        .position(|level| *level == effort)
        .map_or(-1, |index| index as i32)
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !value
}

// RuleRequest is what a rule looks at in a request.
#[derive(Clone, Debug, Default)]
pub struct RuleRequest {
    pub tokens: i64,
    pub images: bool,
    pub thinking: bool,
    pub effort: String,
    pub agent: String,
    pub intent: String,
}

// MatchRule is the first of rules the request matches, None for none.
pub fn match_rule(rules: &[Rule], q: &RuleRequest) -> Option<usize> {
    rules.iter().position(|rule| rule.matches(q))
}

// Intents are the intents the classifier is to choose among for q: those
// of the rules that match it but for their intent, up to the first that
// matches outright (a rule after it could never be the first to match).
// None when no rule before that one has an intent — the classifier then
// isn't asked. Each is given once, as the first rule has it.
pub fn intents(rules: &[Rule], q: &RuleRequest) -> Vec<String> {
    let mut out = Vec::new();
    for rule in rules {
        if !rule.matches_besides_intent(q) {
            continue;
        }
        if rule.intent.is_empty() {
            break;
        }
        if !out
            .iter()
            .any(|intent: &String| intent.eq_ignore_ascii_case(rule.intent.as_str()))
        {
            out.push(rule.intent.clone());
        }
    }
    out
}

fn clean_list(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    for item in value
        .split([',', ' '])
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let item = item.to_ascii_lowercase();
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

// cleanRules validates a group's rules against its members.
pub fn clean_rules(rules: &[Rule], members: &[String]) -> Result<Vec<Rule>> {
    let mut out = Vec::new();
    for (index, saved) in rules.iter().enumerate() {
        let mut rule = saved.clone();
        rule.use_ = rule.use_.trim().to_owned();
        rule.effort = rule.effort.trim().to_ascii_lowercase();
        rule.agents = clean_list(&rule.agents.join(","));
        rule.intent = rule.intent.split_whitespace().collect::<Vec<_>>().join(" ");
        let n = index + 1;
        ensure!(
            !rule.use_.is_empty(),
            "rule {n}: which model it sends to is missing"
        );
        ensure!(
            members.contains(&rule.use_),
            "rule {n}: {} is not in the group",
            rule.use_
        );
        ensure!(rule.tokens >= 0, "rule {n}: tokens can't be negative");
        ensure!(
            rule.effort.is_empty()
                || rule.effort == "on"
                || EFFORTS.contains(&rule.effort.as_str()),
            "rule {n}: effort is on or one of {}, not {:?}",
            EFFORTS.join(", "),
            rule.effort
        );
        ensure!(
            rule.intent.chars().count() <= MAX_INTENT,
            "rule {n}: an intent is at most {MAX_INTENT} characters"
        );
        ensure!(
            !rule.conditions().is_empty(),
            "rule {n}: it needs a condition (tokens, images, effort, agents or intent)"
        );
        out.push(rule);
    }
    Ok(out)
}

// ---- the group's rules as the provider file keeps them ---------------------

// rules_of reads a group's rules; the provider file keeps them as the
// group's "rules" field, which the Group struct carries unexamined.
pub fn rules_of(group: &Group) -> Vec<Rule> {
    serde_json::to_value(group)
        .ok()
        .and_then(|value| value.get("rules").cloned())
        .and_then(|rules| serde_json::from_value::<Vec<Rule>>(rules).ok())
        .unwrap_or_default()
}

// classifier_of reads the group's classifier model.
pub fn classifier_of(group: &Group) -> String {
    serde_json::to_value(group)
        .ok()
        .and_then(|value| {
            value
                .get("classifier")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn save_group_rules(group: &Group, rules: Vec<Rule>, classifier: &str) -> Result<()> {
    let rules = clean_rules(&rules, &group.members)?;
    let mut value = serde_json::to_value(group).context("read the group's saved fields")?;
    let object = value
        .as_object_mut()
        .context("the group is not a JSON object")?;
    if rules.is_empty() {
        object.remove("rules");
    } else {
        object.insert("rules".to_owned(), serde_json::to_value(&rules)?);
    }
    let intents = rules.iter().any(|rule| !rule.intent.is_empty());
    let classifier = classifier
        .trim()
        .strip_prefix("magpie/")
        .unwrap_or(classifier.trim());
    ensure!(
        !classifier.starts_with(GROUP_PREFIX),
        "the classifier is a model, not a group ({classifier})"
    );
    ensure!(
        !intents || !classifier.is_empty(),
        "a rule with an intent needs the group's classifier: the model that tells which intent a message is"
    );
    if intents {
        object.insert(
            "classifier".to_owned(),
            Value::String(classifier.to_owned()),
        );
    } else {
        object.remove("classifier");
    }
    if !classifier.is_empty() {
        let entries = provider::available_model_entries()?;
        ensure!(
            entries.iter().any(|entry| entry.id == classifier),
            "magpie knows no model {classifier:?} to classify with"
        );
    }
    let group: Group = serde_json::from_value(value)?;
    provider::save_group(group)
}

// pruneRules drops the rules for models no longer in the group, saying so.
pub fn prune_rules(group: &Group) -> Result<bool> {
    let mut changed = false;
    let mut keep = Vec::new();
    for (index, rule) in rules_of(group).into_iter().enumerate() {
        if group.members.contains(&rule.use_) {
            keep.push(rule);
        } else {
            println!("! rule {} is removed with {}", index + 1, rule.use_);
            changed = true;
        }
    }
    if changed {
        save_group_rules(group, keep, &classifier_of(group))?;
    }
    Ok(changed)
}

pub fn find_group(reference: &str) -> Result<Group> {
    let reference = reference
        .trim()
        .strip_prefix("magpie/")
        .unwrap_or(reference.trim());
    let reference = reference.strip_prefix(GROUP_PREFIX).unwrap_or(reference);
    let groups = provider::groups()?;
    groups
        .iter()
        .find(|group| group.id == reference)
        .or_else(|| {
            groups
                .iter()
                .find(|group| group.id.eq_ignore_ascii_case(reference))
        })
        .or_else(|| {
            groups
                .iter()
                .find(|group| !group.hidden && group.name.eq_ignore_ascii_case(reference))
        })
        .cloned()
        .with_context(|| {
            let ids = groups
                .iter()
                .filter(|group| !group.hidden)
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if ids.is_empty() {
                format!("no group {reference:?}; add one with `magpie group add`")
            } else {
                format!("no group {reference:?}; groups: {ids}")
            }
        })
}

// ---- what a rule looks at in a request -------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PartKind {
    #[default]
    Text,
    Image,
    ToolCall,
    ToolResult,
}

#[derive(Clone, Debug, Default)]
pub struct Part {
    pub kind: PartKind,
    pub text: String,
    pub name: String,
    pub args: String,
}

#[derive(Clone, Debug, Default)]
pub struct Message {
    pub role: String,
    pub parts: Vec<Part>,
}

// TurnRequest is the slice of a gateway request a rule and the classifier
// see, gathered from whatever protocol the agent speaks.
#[derive(Clone, Debug, Default)]
pub struct TurnRequest {
    pub messages: Vec<Message>,
    pub system_len: usize,
    pub tools_len: usize,
    pub thinking: bool,
    pub effort: String,
}

impl TurnRequest {
    pub fn has_images(&self) -> bool {
        self.messages.iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| part.kind == PartKind::Image)
        })
    }
}

pub fn parse(protocol: ApiProtocol, body: &Value) -> TurnRequest {
    match protocol {
        ApiProtocol::Chat => parse_chat(body),
        ApiProtocol::Anthropic => parse_anthropic(body),
        ApiProtocol::Responses => parse_responses(body),
    }
}

fn string_or_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("text" | "input_text" | "output_text")
                )
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
            })
            .collect(),
        _ => String::new(),
    }
}

fn json_len(value: Option<&Value>) -> usize {
    value.map_or(0, |value| {
        serde_json::to_string(value).map_or(0, |text| text.len())
    })
}

fn chat_parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(text)) => (!text.is_empty())
            .then(|| Part {
                kind: PartKind::Text,
                text: text.clone(),
                ..Part::default()
            })
            .into_iter()
            .collect(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => Some(Part {
                    kind: PartKind::Text,
                    text: block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    ..Part::default()
                }),
                Some("image_url") => Some(Part {
                    kind: PartKind::Image,
                    ..Part::default()
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn tool_call_part(name: Option<&str>, args: Option<&Value>) -> Part {
    Part {
        kind: PartKind::ToolCall,
        name: name.unwrap_or_default().to_owned(),
        args: args.map_or_else(String::new, |args| args.to_string()),
        ..Part::default()
    }
}

fn parse_chat(body: &Value) -> TurnRequest {
    let mut out = TurnRequest::default();
    out.effort = effort_of(
        body.get("reasoning_effort")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    out.thinking = !out.effort.is_empty();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match message.get("role").and_then(Value::as_str) {
            Some("system" | "developer") => {
                out.system_len += string_or_text(message.get("content")).len();
            }
            Some("user") => out.messages.push(Message {
                role: "user".to_owned(),
                parts: chat_parts(message.get("content")),
            }),
            Some("assistant") => {
                let mut parts = chat_parts(message.get("content"));
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let function = call.get("function");
                    parts.push(tool_call_part(
                        function.and_then(|f| f.get("name")).and_then(Value::as_str),
                        function.and_then(|f| f.get("arguments")),
                    ));
                }
                out.messages.push(Message {
                    role: "assistant".to_owned(),
                    parts,
                });
            }
            Some("tool") => out.messages.push(Message {
                role: "user".to_owned(),
                parts: vec![Part {
                    kind: PartKind::ToolResult,
                    text: string_or_text(message.get("content")),
                    ..Part::default()
                }],
            }),
            _ => {}
        }
    }
    for tool in body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(kind) = tool.get("type").and_then(Value::as_str)
            && kind != "function"
        {
            continue;
        }
        let function = tool.get("function");
        out.tools_len += function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .map_or(0, str::len)
            + function
                .and_then(|f| f.get("description"))
                .and_then(Value::as_str)
                .map_or(0, str::len)
            + json_len(function.and_then(|f| f.get("parameters")));
    }
    out
}

fn anthropic_parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(text)) => (!text.is_empty())
            .then(|| Part {
                kind: PartKind::Text,
                text: text.clone(),
                ..Part::default()
            })
            .into_iter()
            .collect(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => Some(Part {
                    kind: PartKind::Text,
                    text: block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    ..Part::default()
                }),
                Some("image") => Some(Part {
                    kind: PartKind::Image,
                    ..Part::default()
                }),
                Some("tool_use") => Some(tool_call_part(
                    block.get("name").and_then(Value::as_str),
                    block.get("input"),
                )),
                Some("tool_result") => Some(Part {
                    kind: PartKind::ToolResult,
                    text: string_or_text(block.get("content")),
                    ..Part::default()
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_anthropic(body: &Value) -> TurnRequest {
    let mut out = TurnRequest::default();
    out.system_len += string_or_text(body.get("system")).len();
    let thinking = body.get("thinking").filter(|thinking| !thinking.is_null());
    if let Some(kind) = thinking
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        && matches!(kind, "enabled" | "adaptive")
    {
        out.thinking = true;
        out.effort = effort_of_budget(
            thinking
                .and_then(|thinking| thinking.get("budget_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
        );
        if let Some(effort) = body
            .get("output_config")
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .map(effort_of)
            .filter(|effort| !effort.is_empty())
        {
            out.effort = effort;
        }
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        out.messages.push(Message {
            role: message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            parts: anthropic_parts(message.get("content")),
        });
    }
    for tool in body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or_default();
        let schema = tool
            .get("input_schema")
            .is_some_and(|schema| !schema.is_null());
        if !kind.is_empty() && kind != "custom" && !schema {
            continue;
        }
        out.tools_len += tool.get("name").and_then(Value::as_str).map_or(0, str::len)
            + tool
                .get("description")
                .and_then(Value::as_str)
                .map_or(0, str::len)
            + json_len(tool.get("input_schema"));
    }
    out
}

fn responses_parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(text)) => (!text.is_empty())
            .then(|| Part {
                kind: PartKind::Text,
                text: text.clone(),
                ..Part::default()
            })
            .into_iter()
            .collect(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("input_text" | "text" | "output_text") => Some(Part {
                    kind: PartKind::Text,
                    text: block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    ..Part::default()
                }),
                Some("input_image" | "image") => Some(Part {
                    kind: PartKind::Image,
                    ..Part::default()
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn merge_consecutive(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for message in messages {
        if let Some(last) = out.last_mut()
            && last.role == message.role
        {
            last.parts.extend(message.parts);
        } else {
            out.push(message);
        }
    }
    out
}

fn parse_responses(body: &Value) -> TurnRequest {
    let mut out = TurnRequest::default();
    out.system_len += string_or_text(body.get("instructions")).len();
    if let Some(reasoning) = body
        .get("reasoning")
        .filter(|reasoning| !reasoning.is_null())
    {
        out.effort = effort_of(
            reasoning
                .get("effort")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        out.thinking = true;
    }
    match body.get("input") {
        Some(Value::String(text)) => out.messages.push(Message {
            role: "user".to_owned(),
            parts: vec![Part {
                kind: PartKind::Text,
                text: text.clone(),
                ..Part::default()
            }],
        }),
        Some(Value::Array(items)) => {
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
                let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
                if kind == "message" || (kind.is_empty() && !role.is_empty()) {
                    if role == "system" || role == "developer" {
                        out.system_len += string_or_text(item.get("content")).len();
                        continue;
                    }
                    out.messages.push(Message {
                        role: if role == "assistant" {
                            "assistant"
                        } else {
                            "user"
                        }
                        .to_owned(),
                        parts: responses_parts(item.get("content")),
                    });
                } else if kind == "function_call" {
                    out.messages.push(Message {
                        role: "assistant".to_owned(),
                        parts: vec![tool_call_part(
                            item.get("name").and_then(Value::as_str),
                            item.get("arguments"),
                        )],
                    });
                } else if kind == "function_call_output" {
                    out.messages.push(Message {
                        role: "user".to_owned(),
                        parts: vec![Part {
                            kind: PartKind::ToolResult,
                            text: string_or_text(item.get("output")),
                            ..Part::default()
                        }],
                    });
                }
            }
            out.messages = merge_consecutive(out.messages);
        }
        _ => {}
    }
    for tool in body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        out.tools_len += tool.get("name").and_then(Value::as_str).map_or(0, str::len)
            + tool
                .get("description")
                .and_then(Value::as_str)
                .map_or(0, str::len)
            + json_len(tool.get("parameters"));
    }
    out
}

// effortOf normalises the reasoning effort names the APIs use.
fn effort_of(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimal" | "none" => "low".to_owned(),
        level @ ("low" | "medium" | "high" | "xhigh" | "max") => level.to_owned(),
        _ => String::new(),
    }
}

fn effort_of_budget(budget: i64) -> String {
    if budget <= 0 {
        String::new()
    } else if budget <= 4096 {
        "low".to_owned()
    } else if budget <= 12000 {
        "medium".to_owned()
    } else if budget <= 24000 {
        "high".to_owned()
    } else {
        "xhigh".to_owned()
    }
}

// estimate is a token count from the request's sizes.
pub fn estimate(req: &TurnRequest) -> i64 {
    let mut n = req.system_len + req.tools_len;
    for message in &req.messages {
        for part in &message.parts {
            n += part.text.len() + part.args.len() + part.name.len();
        }
    }
    (n / 4) as i64
}

// turnIn counts the user's turns in a request, and says whether it is the
// agent handing tool results back within one.
pub fn turn_in(req: &TurnRequest) -> (usize, bool) {
    let mut turn = 0;
    let mut within = false;
    for message in &req.messages {
        if message.role != "user" {
            continue;
        }
        let mut text = false;
        let mut result = false;
        for part in &message.parts {
            match part.kind {
                PartKind::Text | PartKind::Image => text = true,
                PartKind::ToolResult => result = true,
                PartKind::ToolCall => {}
            }
        }
        if text && !result {
            turn += 1;
        }
        within = result;
    }
    (turn, within)
}

// firstWords tells an agent's subagents from it: they share its session
// but not what they were asked. Only the words are taken — not how the
// agent marked them for caching — as they stay the same for the
// conversation's whole life.
pub fn first_words(req: &TurnRequest) -> String {
    let mut hasher = Sha256::new();
    if let Some(message) = req.messages.iter().find(|message| message.role == "user") {
        for part in &message.parts {
            if part.kind == PartKind::Text {
                hasher.update(part.text.as_bytes());
            }
        }
    }
    hex_prefix(&hasher.finalize())
}

fn hex_prefix(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(24);
    for byte in &digest[..12] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn conversation_id(headers: &HeaderMap) -> String {
    for name in [
        "x-opencode-session",
        "x-session-affinity",
        "x-session-id",
        "session_id",
        "session-id",
        "x-claude-code-session-id",
    ] {
        if let Some(value) = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return value.to_owned();
        }
    }
    format!("magpie-{}", hex_prefix(&Sha256::digest([])))
}

// ruleKey is the conversation a group's rules keep a turn's decision for:
// the agent's session and the conversation's first words.
pub fn rule_key(group_id: &str, headers: &HeaderMap, req: &TurnRequest) -> String {
    format!(
        "{GROUP_PREFIX}{group_id}|{}|{}",
        conversation_id(headers),
        first_words(req)
    )
}

fn strip_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        match after.find(CLOSE) {
            Some(end) => rest = &after[end + CLOSE.len()..],
            None => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

// userText is what the user said to begin the turn: the last user
// message's text, without what agents add to it for the model
// (<system-reminder>…), its middle left out when long.
pub fn user_text(req: &TurnRequest) -> String {
    let mut parts = Vec::new();
    if let Some(message) = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
    {
        for part in &message.parts {
            if part.kind == PartKind::Text {
                parts.push(part.text.clone());
            }
        }
    }
    let text = strip_reminders(&parts.join("\n")).trim().to_owned();
    const HEAD: usize = 3000;
    const TAIL: usize = 1000;
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() > HEAD + TAIL {
        return format!(
            "{}\n…\n{}",
            chars[..HEAD].iter().collect::<String>(),
            chars[chars.len() - TAIL..].iter().collect::<String>()
        );
    }
    text
}

// ---- the trace --------------------------------------------------------------

// RuleHit is what the trace tells of a group's rules for a request.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RuleHit {
    pub n: usize,
    #[serde(rename = "use", skip_serializing_if = "String::is_empty")]
    pub use_: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub held: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub waits: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub grown: bool,
    pub turn: usize,
    pub tokens: i64,
    #[serde(skip_serializing_if = "is_false")]
    pub images: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
    #[serde(skip_serializing_if = "is_false")]
    pub unready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classified: Option<Classified>,
}

// Classified is what the classifier was asked as a turn began, and said.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Classified {
    pub by: String,
    pub intents: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub intent: String,
    #[serde(skip_serializing_if = "is_false")]
    pub cached: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub ms: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

// The classifier a group's turn asks: any model magpie has, through the
// gateway itself.
pub trait Classifier: Send + Sync {
    fn ask<'a>(
        &'a self,
        model: &'a str,
        intents: &'a [String],
        text: &'a str,
    ) -> BoxFuture<'a, Result<String>>;
}

#[derive(Clone)]
struct TurnRule {
    turn: usize,
    use_: String,
    n: usize,
    at: Instant,
    input: i64,
    intent: String,
}

const STICK_KEEP: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_TURN_RULES: usize = 4096;

static TURN_RULES: LazyLock<Mutex<HashMap<String, TurnRule>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn turn_rules() -> std::sync::MutexGuard<'static, HashMap<String, TurnRule>> {
    TURN_RULES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ruleFor is the rule a group request goes by: at a turn's start, the
// first of the group's rules it matches — asking the group's classifier
// first when a rule that may match has an intent; within the turn — the
// agent handing tool results back — what was decided when it began, unless
// the conversation has grown past what that model can take and a rule
// sends it to one with more room. None for a group without rules.
pub async fn rule_for(
    key: &str,
    rules: &[Rule],
    classifier: &str,
    req: &TurnRequest,
    agent: &str,
    contexts: &BTreeMap<String, i64>,
    ask: Option<&dyn Classifier>,
) -> Option<RuleHit> {
    if rules.is_empty() {
        return None;
    }
    let (turn, within) = turn_in(req);
    let mut q = RuleRequest {
        tokens: estimate(req),
        thinking: req.thinking,
        effort: req.effort.clone(),
        agent: agent.to_ascii_lowercase(),
        ..RuleRequest::default()
    };
    q.images = req.has_images();
    let now = Instant::now();
    let mut had = false;
    let mut tr = TurnRule {
        turn: 0,
        use_: String::new(),
        n: 0,
        at: now,
        input: 0,
        intent: String::new(),
    };
    {
        let state = turn_rules();
        if let Some(saved) = state.get(key) {
            tr = saved.clone();
            had = true;
        }
    }
    if had && now.saturating_duration_since(tr.at) > STICK_KEEP {
        had = false;
    }
    if had && tr.input > q.tokens {
        q.tokens = tr.input; // what the vendor counted last, a floor: the conversation only grew since
    }
    let mut hit = RuleHit {
        turn,
        tokens: q.tokens,
        images: q.images,
        effort: if q.effort.is_empty() && q.thinking {
            "on".to_owned()
        } else {
            q.effort.clone()
        },
        ..RuleHit::default()
    };
    if within {
        // the same turn: what was decided as it began, while the group
        // still has that rule; else nothing moves it
        if had {
            q.intent = tr.intent.clone(); // as the classifier said when the turn began
        }
        if !had || (tr.turn != turn && turn > 0) {
            hit.waits = true;
        } else if tr.use_.is_empty() {
            hit.held = true;
        } else if tr.n >= 1 && tr.n <= rules.len() && rules[tr.n - 1].use_ == tr.use_ {
            hit.held = true;
            hit.n = tr.n;
            hit.use_ = tr.use_.clone();
            hit.when = rules[tr.n - 1].conditions();
        } else {
            hit.waits = true;
        }
        if let Some(grown) = outgrown(rules, contexts, &hit, &q) {
            hit = grown;
            turn_rules().insert(
                key.to_owned(),
                TurnRule {
                    turn,
                    use_: hit.use_.clone(),
                    n: hit.n,
                    at: now,
                    input: tr.input,
                    intent: q.intent,
                },
            );
            return Some(hit);
        }
        if had && let Some(saved) = turn_rules().get_mut(key) {
            saved.at = now;
        }
        return Some(hit);
    }
    // a new turn: the classifier is asked only when a rule that could be
    // the first to match waits on its intent
    let intents = intents(rules, &q);
    if !intents.is_empty() {
        let mut classified = Classified {
            by: classifier.to_owned(),
            intents: intents.clone(),
            ..Classified::default()
        };
        let text = user_text(req);
        if classifier.is_empty() {
            classified.error = "the group has no classifier".to_owned();
        } else if ask.is_none() {
            classified.error = "nothing to ask the classifier with".to_owned();
        } else if text.is_empty() {
            classified.error = "the message has no words to classify".to_owned();
        } else {
            let started = Instant::now();
            match classify::classify(ask.expect("checked above"), classifier, &intents, &text).await
            {
                Ok((intent, cached)) => {
                    classified.intent = intent;
                    classified.cached = cached;
                }
                Err(error) => classified.error = error.to_string(),
            }
            classified.ms = started.elapsed().as_millis() as i64;
        }
        q.intent = classified.intent.clone();
        hit.classified = Some(classified);
    }
    if let Some(index) = match_rule(rules, &q) {
        hit.n = index + 1;
        hit.use_ = rules[index].use_.clone();
        hit.when = rules[index].conditions();
    }
    {
        let mut state = turn_rules();
        let mut input = tr.input;
        if let Some(current) = state.get(key) {
            input = current.input; // answered while the classifier was asked
        }
        state.insert(
            key.to_owned(),
            TurnRule {
                turn,
                use_: hit.use_.clone(),
                n: hit.n,
                at: now,
                input,
                intent: q.intent,
            },
        );
        if state.len() > MAX_TURN_RULES {
            state.retain(|_, tr| now.saturating_duration_since(tr.at) <= STICK_KEEP);
        }
    }
    Some(hit)
}

// outgrown is the rule a request within a turn moves by when it no longer
// fits the model the turn is on: what its rule sent it to, or, when no
// rule did, the least any member takes. It moves only to a model known to
// take more.
fn outgrown(
    rules: &[Rule],
    contexts: &BTreeMap<String, i64>,
    held: &RuleHit,
    q: &RuleRequest,
) -> Option<RuleHit> {
    let mut limit = 0;
    if !held.use_.is_empty() {
        limit = contexts.get(&held.use_).copied().unwrap_or(0);
    } else {
        for context in contexts.values() {
            if *context > 0 && (limit == 0 || *context < limit) {
                limit = *context;
            }
        }
    }
    if limit == 0 || q.tokens < limit * 95 / 100 {
        return None;
    }
    let index = match_rule(rules, q)?;
    let rule = &rules[index];
    if rule.use_ == held.use_ || contexts.get(&rule.use_).copied().unwrap_or(0) <= limit {
        return None;
    }
    let mut out = held.clone();
    out.held = false;
    out.waits = false;
    out.grown = true;
    out.n = index + 1;
    out.use_ = rule.use_.clone();
    out.when = rule.conditions();
    Some(out)
}

// memberContexts is the tokens each member takes, where known.
pub fn member_contexts(members: &[String]) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for member in members {
        let Some((provider_id, model)) = member.split_once('/') else {
            continue;
        };
        let Some(catalog_id) = provider::catalog_id_for_usage(provider_id) else {
            continue;
        };
        let context = crate::catalog::available_models(provider_id, &catalog_id)
            .into_iter()
            .filter(|entry| entry.id == model && entry.context > 0)
            .map(|entry| entry.context as i64)
            .min();
        if let Some(context) = context {
            out.insert(member.clone(), context);
        }
    }
    out
}

// ruleAnswered keeps what the vendor counted a conversation's request as,
// for its next turn's rules to go by.
#[allow(dead_code)] // called by the usage/trace hook once it records completions
pub fn rule_answered(key: &str, usage: crate::usage::TokenUsage) {
    let tokens = (usage.input + usage.cache_read + usage.cache_write) as i64;
    if tokens <= 0 {
        return;
    }
    if let Some(tr) = turn_rules().get_mut(key) {
        tr.input = tokens;
    }
}

// rule_line is a rule as magpie group shows it.
pub fn rule_line(rule: &Rule) -> String {
    format!("{} → {}", rule.conditions().join(" · "), rule.use_)
}

// ---- magpie group rule add|rm|mv from the terminal --------------------------

// parseTokens reads 200000, 200k, 1.5m.
fn parse_tokens(value: &str) -> Result<i64> {
    let s = value.replace('_', "");
    let s = s.trim().to_ascii_lowercase();
    let (multiplier, s) = if let Some(number) = s.strip_suffix('k') {
        (1e3, number)
    } else if let Some(number) = s.strip_suffix('m') {
        (1e6, number)
    } else {
        (1.0, s.as_str())
    };
    let length: f64 = s.parse().map_err(|_| {
        anyhow::Error::msg(format!(
            "tokens {value:?} is not a length (200000, 200k, 1m)"
        ))
    })?;
    ensure!(
        length >= 0.0,
        "tokens {value:?} is not a length (200000, 200k, 1m)"
    );
    Ok((length * multiplier) as i64)
}

// groupMember is the group's member a typed model names: its id, its
// model id without the provider, or the model's last name.
fn group_member(group: &Group, input: &str) -> Result<String> {
    let input = input.trim().strip_prefix("magpie/").unwrap_or(input.trim());
    let matchers: [fn(&str, &str) -> bool; 4] = [
        |member, input| member == input,
        |member, input| member.eq_ignore_ascii_case(input),
        |member, input| {
            member
                .split_once('/')
                .map(|(_, bare)| bare)
                .unwrap_or_default()
                .eq_ignore_ascii_case(input)
        },
        |member, input| {
            member[member.rfind('/').map_or(0, |at| at + 1)..].eq_ignore_ascii_case(input)
        },
    ];
    for matcher in matchers {
        let hits = group
            .members
            .iter()
            .filter(|member| matcher(member, input))
            .cloned()
            .collect::<Vec<_>>();
        match hits.as_slice() {
            [] => continue,
            [only] => return Ok((*only).clone()),
            _ => bail!("{input} is {}: name one", hits.join(" and ")),
        }
    }
    bail!(
        "{input} is not in {} (its models: {})",
        group.id,
        group.members.join(", ")
    )
}

// parseRule makes a rule of k=v words; the model is resolved among the
// group's members. classifier is the group's classifier when one was
// given.
fn parse_rule(group: &Group, words: &[String]) -> Result<(Rule, usize, String)> {
    let mut rule = Rule::default();
    let mut at = 0;
    let mut classifier = String::new();
    for word in words {
        let (key, value) = word.split_once('=').unwrap_or((word.as_str(), ""));
        match key.trim().to_ascii_lowercase().as_str() {
            "use" | "model" | "to" => rule.use_ = group_member(group, value)?,
            "tokens" | "context" | "longer" => rule.tokens = parse_tokens(value)?,
            "images" | "image" => match value.to_ascii_lowercase().as_str() {
                "" | "yes" | "true" | "on" | "1" => rule.images = true,
                "no" | "false" | "off" | "0" => rule.images = false,
                _ => bail!("images takes no value (or yes/no), not {value:?}"),
            },
            "effort" | "reasoning" | "thinking" => {
                rule.effort = if value.is_empty() {
                    "on".to_owned()
                } else {
                    value.trim().to_ascii_lowercase()
                };
            }
            "agents" | "agent" => rule.agents = clean_list(value),
            "intent" | "asks" | "about" => {
                rule.intent = value.trim().to_owned();
                ensure!(
                    !rule.intent.is_empty(),
                    r#"intent says what the message asks for: intent="writing or fixing tests""#
                );
            }
            "classifier" | "classify" | "by" => {
                classifier = value.trim().to_owned();
                ensure!(
                    !classifier.is_empty(),
                    "classifier=<model>: the model that tells which intent a message is"
                );
            }
            "at" => {
                at = value
                    .parse()
                    .with_context(|| format!("at is a place from 1, not {value:?}"))?;
                ensure!(at >= 1, "at is a place from 1, not {value:?}");
            }
            _ => bail!(
                "unknown {word:?} (use, tokens, images, effort, agents, intent, classifier, at)\n\n{RULE_USAGE}"
            ),
        }
    }
    ensure!(
        !rule.use_.is_empty(),
        "use=<model> is missing: one of {}",
        group.members.join(", ")
    );
    ensure!(
        rule.intent.is_empty() || !classifier.is_empty() || !classifier_of(group).is_empty(),
        "a rule with an intent needs the group's classifier, the model that tells which intent a message is: add classifier=<model>, best a small fast one"
    );
    Ok((rule, at, classifier))
}

fn place(group: &Group, word: &str) -> Result<usize> {
    let count = rules_of(group).len();
    match word.parse::<usize>() {
        Ok(n) if n >= 1 && n <= count => Ok(n - 1),
        _ if count == 0 => bail!("{} has no rules", group.id),
        _ => bail!("rule {word}: {} has rules 1 to {count}", group.id),
    }
}

// ruleCmd: magpie group rule …
pub fn command(args: &[String]) -> Result<()> {
    if args.is_empty() || matches!(args[0].as_str(), "help" | "-h" | "--help") {
        println!("{RULE_USAGE}");
        return Ok(());
    }
    let (verb, rest) = (args[0].as_str(), &args[1..]);
    if rest.is_empty() {
        // magpie group rule <group>: the group, its rules with it
        if let Ok(group) = find_group(verb) {
            let rules = rules_of(&group);
            if rules.is_empty() {
                println!(
                    "  {} has no rules · magpie group rule add {} use=<model> …",
                    group.id, group.id
                );
                return Ok(());
            }
            return show(&group, &rules);
        }
        bail!("{RULE_USAGE}");
    }
    let group = find_group(&rest[0])?;
    ensure!(
        !group.hidden,
        "{} was removed: magpie group restore {} brings it back first",
        group.id,
        group.id
    );
    let mut rules = rules_of(&group);
    let mut classifier = classifier_of(&group);
    match verb {
        "add" | "new" => {
            let (rule, at, given) = parse_rule(&group, &rest[1..])?;
            if !given.is_empty() {
                classifier = given;
            }
            if at == 0 || at > rules.len() {
                rules.push(rule);
            } else {
                rules.insert(at - 1, rule);
            }
        }
        "rm" | "remove" | "delete" => {
            let [word] = &rest[1..] else {
                bail!("magpie group rule rm <group> <n>");
            };
            let index = place(&group, word)?;
            rules.remove(index);
        }
        "mv" | "move" => {
            let [from, to] = &rest[1..] else {
                bail!("magpie group rule mv <group> <n> <to>");
            };
            let index = place(&group, from)?;
            let target = place(&group, to)?;
            let rule = rules.remove(index);
            rules.insert(target, rule);
        }
        "classifier" | "classify" => {
            let [model] = &rest[1..] else {
                bail!("magpie group rule classifier <group> <model>");
            };
            ensure!(
                rules.iter().any(|rule| !rule.intent.is_empty()),
                "{} has no rule with an intent to classify for",
                group.id
            );
            classifier = model.as_str().to_owned();
        }
        _ => bail!("magpie group rule has no {verb:?}\n\n{RULE_USAGE}"),
    }
    save_group_rules(&group, rules, &classifier)?;
    let group = find_group(&group.id)?;
    let rules = rules_of(&group);
    println!("✓ saved {}", group.name);
    show(&group, &rules)
}

fn show(group: &Group, rules: &[Rule]) -> Result<()> {
    println!("{}  {GROUP_PREFIX}{}", group.name, group.id);
    for (index, rule) in rules.iter().enumerate() {
        println!("  {:>6}. {}", index + 1, rule_line(rule));
    }
    let classifier = classifier_of(group);
    if !classifier.is_empty() {
        println!("  classifier  {classifier}  tells which intent a message is");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grouprule::classify::{classify_body, fit_effort, read_intent};
    use serde_json::json;

    fn chat(body: Value) -> TurnRequest {
        parse(ApiProtocol::Chat, &body)
    }

    #[test]
    fn conditions_say_what_a_rule_matches() {
        let rule = Rule {
            use_: "b/big".to_owned(),
            tokens: 200000,
            effort: "high".to_owned(),
            agents: vec!["codex".to_owned()],
            ..Rule::default()
        };
        assert_eq!(
            rule.conditions(),
            vec![
                "tokens ≥ 200000".to_owned(),
                "effort ≥ high".to_owned(),
                "agent codex".to_owned()
            ]
        );
        let intent = Rule {
            use_: "a/small".to_owned(),
            intent: "planning".to_owned(),
            ..Rule::default()
        };
        assert_eq!(intent.conditions(), vec![r#"intent "planning""#.to_owned()]);
        assert_eq!(
            rule_line(&intent),
            r#"intent "planning" → a/small"#.to_owned()
        );
        let reasoning = Rule {
            use_: "a/small".to_owned(),
            effort: "on".to_owned(),
            ..Rule::default()
        };
        assert_eq!(reasoning.conditions(), vec!["reasoning".to_owned()]);
    }

    #[test]
    fn rules_match_by_tokens_images_effort_and_agent() {
        let rule = Rule {
            use_: "b/big".to_owned(),
            tokens: 20000,
            ..Rule::default()
        };
        let short = RuleRequest {
            tokens: 19999,
            ..RuleRequest::default()
        };
        let long = RuleRequest {
            tokens: 20000,
            ..RuleRequest::default()
        };
        assert!(!rule.matches(&short));
        assert!(rule.matches(&long));

        let vision = Rule {
            use_: "b/big".to_owned(),
            images: true,
            ..Rule::default()
        };
        assert!(!vision.matches(&RuleRequest::default()));
        assert!(vision.matches(&RuleRequest {
            images: true,
            ..RuleRequest::default()
        }));

        let effort = Rule {
            use_: "b/big".to_owned(),
            effort: "high".to_owned(),
            ..Rule::default()
        };
        for (asked, want) in [
            ("", false),
            ("medium", false),
            ("high", true),
            ("xhigh", true),
            ("max", true),
        ] {
            assert_eq!(
                effort.matches(&RuleRequest {
                    effort: asked.to_owned(),
                    ..RuleRequest::default()
                }),
                want,
                "{asked}"
            );
        }
        // "on" is any reasoning at all
        let reasoning = Rule {
            use_: "b/big".to_owned(),
            effort: "on".to_owned(),
            ..Rule::default()
        };
        assert!(!reasoning.matches(&RuleRequest::default()));
        assert!(reasoning.matches(&RuleRequest {
            thinking: true,
            ..RuleRequest::default()
        }));

        let agent = Rule {
            use_: "b/big".to_owned(),
            agents: vec!["codex".to_owned()],
            ..Rule::default()
        };
        assert!(agent.matches(&RuleRequest {
            agent: "codex".to_owned(),
            ..RuleRequest::default()
        }));
        assert!(!agent.matches(&RuleRequest {
            agent: "claude".to_owned(),
            ..RuleRequest::default()
        }));
    }

    #[test]
    fn intents_stop_at_the_first_outright_match_and_name_each_once() {
        let rules = vec![
            Rule {
                use_: "a/small".to_owned(),
                intent: "a quick question".to_owned(),
                ..Rule::default()
            },
            Rule {
                use_: "b/big".to_owned(),
                intent: "Planning a large change".to_owned(),
                ..Rule::default()
            },
            Rule {
                use_: "b/big".to_owned(),
                intent: "planning a large change".to_owned(),
                agents: vec!["codex".to_owned()],
                ..Rule::default()
            },
            Rule {
                use_: "b/big".to_owned(),
                intent: "reviewing code".to_owned(),
                images: true,
                ..Rule::default()
            },
        ];
        let q = RuleRequest::default();
        assert_eq!(
            intents(&rules, &q),
            vec![
                "a quick question".to_owned(),
                "Planning a large change".to_owned()
            ]
        );
        // an image is none of the first two's kinds but the review rule may take it
        let image = RuleRequest {
            images: true,
            ..RuleRequest::default()
        };
        assert_eq!(
            intents(&rules, &image),
            vec![
                "a quick question".to_owned(),
                "Planning a large change".to_owned(),
                "reviewing code".to_owned()
            ]
        );
    }

    #[test]
    fn clean_rules_validates_against_the_group() {
        let members = vec!["a/small".to_owned(), "b/big".to_owned()];
        assert!(
            clean_rules(
                &[Rule {
                    use_: "c/other".to_owned(),
                    tokens: 5,
                    ..Rule::default()
                }],
                &members
            )
            .is_err()
        );
        assert!(
            clean_rules(
                &[Rule {
                    use_: "b/big".to_owned(),
                    ..Rule::default()
                }],
                &members
            )
            .is_err()
        );
        assert!(
            clean_rules(
                &[Rule {
                    use_: "b/big".to_owned(),
                    effort: "x".to_owned(),
                    ..Rule::default()
                }],
                &members
            )
            .is_err()
        );
        assert!(
            clean_rules(
                &[Rule {
                    use_: "b/big".to_owned(),
                    intent: "x".repeat(MAX_INTENT + 1),
                    ..Rule::default()
                }],
                &members
            )
            .is_err()
        );
        let cleaned = clean_rules(
            &[Rule {
                use_: " b/big ".to_owned(),
                effort: " HIGH ".to_owned(),
                agents: vec!["Codex".to_owned(), "codex".to_owned()],
                intent: "  a   quick  question ".to_owned(),
                ..Rule::default()
            }],
            &members,
        )
        .unwrap();
        assert_eq!(cleaned[0].use_, "b/big");
        assert_eq!(cleaned[0].effort, "high");
        assert_eq!(cleaned[0].agents, vec!["codex".to_owned()]);
        assert_eq!(cleaned[0].intent, "a quick question");
    }

    #[test]
    fn parse_tokens_reads_200k_1m_and_underscores() {
        for (input, want) in [
            ("200000", 200000),
            ("200k", 200000),
            ("200K", 200000),
            ("1m", 1000000),
            ("1.5m", 1500000),
            ("128_000", 128000),
            (" 32k ", 32000),
        ] {
            assert_eq!(parse_tokens(input).unwrap(), want, "{input}");
        }
        for input in ["", "k", "lots", "-5", "1g"] {
            assert!(parse_tokens(input).is_err(), "{input}");
        }
    }

    #[test]
    fn group_members_are_named_by_id_bare_or_last_name() {
        let group = Group {
            id: "x".to_owned(),
            members: vec![
                "a/m".to_owned(),
                "b/vendor/m".to_owned(),
                "a/Big".to_owned(),
                "c/deepseek/deepseek-v4-pro".to_owned(),
            ],
            ..Group::default()
        };
        for (input, want) in [
            ("a/m", "a/m"),
            ("magpie/a/m", "a/m"),
            ("A/BIG", "a/Big"),
            ("big", "a/Big"),
            ("vendor/m", "b/vendor/m"),
            ("deepseek-v4-pro", "c/deepseek/deepseek-v4-pro"),
            ("deepseek/deepseek-v4-pro", "c/deepseek/deepseek-v4-pro"),
        ] {
            assert_eq!(group_member(&group, input).unwrap(), want, "{input}");
        }
        // "m" is a's model by name before b's by its last name
        assert_eq!(group_member(&group, "m").unwrap(), "a/m");
        assert!(group_member(&group, "gone").is_err());
    }

    #[test]
    fn parse_rule_resolves_and_refuses() {
        let group = Group {
            id: "opus".to_owned(),
            members: vec!["a/claude-opus-5-5".to_owned(), "b/gpt-5.5".to_owned()],
            ..Group::default()
        };
        let words = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let (rule, at, classifier) = parse_rule(
            &group,
            &words(&[
                "use=gpt-5.5",
                "images",
                "effort=HIGH",
                "agents=codex, claude",
            ]),
        )
        .unwrap();
        assert_eq!(rule.use_, "b/gpt-5.5");
        assert!(rule.images);
        assert_eq!(rule.effort, "high");
        assert_eq!(rule.agents, vec!["codex".to_owned(), "claude".to_owned()]);
        assert_eq!((at, classifier.as_str()), (0, ""));

        let (rule, at, _) =
            parse_rule(&group, &words(&["use=claude-opus-5-5", "thinking", "at=1"])).unwrap();
        assert_eq!(rule.effort, "on");
        assert_eq!(at, 1);

        for bad in [
            &["tokens=5"][..],
            &["use=a/m", "tokens=5"][..],
            &["use=gpt-5.5"][..],
            &["use=gpt-5.5", "effort=x"][..],
            &["use=gpt-5.5", "images=maybe"][..],
            &["use=gpt-5.5", "tokens=5", "color=red"][..],
            &["use=gpt-5.5", "tokens=5", "at=0"][..],
            &["use=gpt-5.5", "intent="][..],
        ] {
            assert!(parse_rule(&group, &words(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parse_chat_gathers_turns_words_and_images() {
        let req = chat(json!({
            "model": "group/r",
            "reasoning_effort": "high",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}
                ]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "ls", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "abcd"}
            ]
        }));
        assert!(req.thinking);
        assert_eq!(req.effort, "high");
        assert!(req.has_images());
        let (turn, within) = turn_in(&req);
        assert_eq!(turn, 1);
        assert!(within);
        assert_eq!(estimate(&req), (8 + 4 + 4 + 4) / 4);
    }

    #[test]
    fn turn_in_counts_user_turns_past_trailing_system() {
        // a system message after the tool results doesn't end the turn
        let round = chat(json!({"messages": [
            {"role": "user", "content": "read it"},
            {"role": "system", "content": "note"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "t1", "type": "function", "function": {"name": "Read", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "t1", "content": "x"},
            {"role": "system", "content": "reminder"}
        ]}));
        assert_eq!(turn_in(&round), (1, true));

        let next = chat(json!({"messages": [
            {"role": "user", "content": "read it"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "more"},
            {"role": "system", "content": "note"}
        ]}));
        assert_eq!(turn_in(&next), (2, false));
    }

    #[test]
    fn parse_reads_anthropic_and_responses_requests() {
        let anthropic = parse(
            ApiProtocol::Anthropic,
            &json!({
                "system": "be brief",
                "thinking": {"type": "enabled", "budget_tokens": 10000},
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "look"},
                        {"type": "image", "source": {"type": "base64", "data": "aGk="}}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": "abcd"}
                    ]}
                ]
            }),
        );
        assert!(anthropic.thinking);
        assert_eq!(anthropic.effort, "medium");
        assert!(anthropic.has_images());
        assert_eq!(turn_in(&anthropic), (1, true));

        let responses = parse(
            ApiProtocol::Responses,
            &json!({
                "instructions": "be brief",
                "reasoning": {"effort": "low"},
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                    {"type": "function_call", "name": "ls", "arguments": "{}"},
                    {"type": "function_call_output", "output": "abcd"}
                ]
            }),
        );
        assert!(responses.thinking);
        assert_eq!(responses.effort, "low");
        assert_eq!(turn_in(&responses), (1, true));
        assert!(
            responses.messages[0]
                .parts
                .iter()
                .any(|part| part.kind == PartKind::Text)
        );
    }

    #[test]
    fn user_text_takes_the_last_user_message_without_reminders() {
        let req = chat(json!({"messages": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>\nx\n</system-reminder>"},
                {"type": "text", "text": "  second  "}
            ]}
        ]}));
        assert_eq!(user_text(&req), "second");

        let big = "a".repeat(3000) + &"m".repeat(5000) + &"z".repeat(1000);
        let got = user_text(&chat(
            json!({"messages": [{"role": "user", "content": big}]}),
        ));
        assert!(got.starts_with(&format!("{}\n…\n", "a".repeat(3000))));
        assert!(got.ends_with(&"z".repeat(1000)));
        assert!(!got.contains('m'));

        // an image alone has no words
        let image = parse(
            ApiProtocol::Chat,
            &json!({"messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}
            ]}]}),
        );
        assert_eq!(user_text(&image), "");
    }

    #[test]
    fn read_intent_takes_the_first_number() {
        let intents = vec!["x".to_owned(), "y".to_owned()];
        for (answer, want) in [("1", "x"), ("2", "y"), (" 0 ", ""), ("2\n", "y")] {
            assert_eq!(read_intent(answer, &intents).unwrap(), want, "{answer:?}");
        }
        for answer in ["3", "-", "none", ""] {
            assert!(read_intent(answer, &intents).is_err(), "{answer:?}");
        }
    }

    #[test]
    fn classify_body_asks_for_the_number_plainly() {
        let body = classify_body(
            "cls",
            "",
            &["writing or fixing tests".to_owned()],
            "add a unit test",
        );
        assert_eq!(body["model"], "cls");
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["stream"], false);
        assert!(body.get("reasoning_effort").is_none());
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("1. writing or fixing tests"));
        assert!(user.contains("add a unit test"));

        let effort = classify_body("cls", "low", &["x".to_owned()], "t");
        assert_eq!(effort["reasoning_effort"], "low");
    }

    #[test]
    fn classifiers_reason_least_their_model_takes() {
        assert_eq!(fit_effort("none", &[]), "none");
        assert_eq!(
            fit_effort(
                "none",
                &["low".to_owned(), "high".to_owned(), "max".to_owned()]
            ),
            "low"
        );
        assert_eq!(
            fit_effort(
                "none",
                &["none".to_owned(), "low".to_owned(), "high".to_owned()]
            ),
            "none"
        );
        assert_eq!(
            fit_effort("none", &["minimal".to_owned(), "medium".to_owned()]),
            "minimal"
        );
    }

    #[tokio::test]
    async fn rule_decides_at_turn_start_and_holds() {
        let rules = vec![Rule {
            use_: "b/big".to_owned(),
            tokens: 20000,
            ..Rule::default()
        }];
        let key = "group/r|turn-holds|words";
        let contexts = BTreeMap::new();
        // turn 1, short: no rule, the group's order
        let short = chat(json!({"messages": [{"role": "user", "content": "hello"}]}));
        let hit = rule_for(key, &rules, "", &short, "", &contexts, None)
            .await
            .unwrap();
        assert_eq!(hit.n, 0);
        assert!(!hit.held);
        assert_eq!(hit.turn, 1);
        // its tool rounds grow past the rule's length: they stay
        let tool = json!({"messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "ls", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "abcd".repeat(30000)}
        ]});
        let hit = rule_for(key, &rules, "", &chat(tool), "", &contexts, None)
            .await
            .unwrap();
        assert!(hit.held);
        assert_eq!(hit.n, 0);
        assert!(hit.tokens >= 20000);
        // turn 2 begins long: the rule sends it to b
        let long = chat(json!({"messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "abcd".repeat(25000)}
        ]}));
        let hit = rule_for(key, &rules, "", &long, "", &contexts, None)
            .await
            .unwrap();
        assert_eq!(hit.n, 1);
        assert_eq!(hit.use_, "b/big");
        assert!(!hit.held);
        // its tool rounds stay on b
        let tool = json!({"messages": [
            {"role": "user", "content": "hello"},
            {"role": "user", "content": "abcd".repeat(25000)},
            {"role": "tool", "tool_call_id": "c1", "content": "x"}
        ]});
        let hit = rule_for(key, &rules, "", &chat(tool), "", &contexts, None)
            .await
            .unwrap();
        assert!(hit.held);
        assert_eq!(hit.n, 1);
    }

    #[tokio::test]
    async fn rule_tokens_floor_comes_from_the_answered_usage() {
        let rules = vec![Rule {
            use_: "b/big".to_owned(),
            tokens: 2000,
            ..Rule::default()
        }];
        let key = "group/r|tokens-floor|hi";
        let contexts = BTreeMap::new();
        let short = chat(json!({"messages": [{"role": "user", "content": "hi"}]}));
        let hit = rule_for(key, &rules, "", &short, "", &contexts, None)
            .await
            .unwrap();
        assert_eq!(hit.n, 0);
        // the vendor said 3000 tokens (500 new, 2500 read from its cache)
        rule_answered(
            key,
            crate::usage::TokenUsage {
                input: 500,
                cache_read: 2500,
                ..crate::usage::TokenUsage::default()
            },
        );
        let hit = rule_for(key, &rules, "", &short, "", &contexts, None)
            .await
            .unwrap();
        assert_eq!(hit.n, 1);
        assert_eq!(hit.tokens, 3000);
        // another conversation starts short
        let other = rule_for(
            "group/r|tokens-floor|other",
            &rules,
            "",
            &short,
            "",
            &contexts,
            None,
        )
        .await
        .unwrap();
        assert_eq!(other.n, 0);
    }

    #[tokio::test]
    async fn a_turn_magpie_did_not_see_begin_waits() {
        let rules = vec![Rule {
            use_: "b/big".to_owned(),
            tokens: 20000,
            ..Rule::default()
        }];
        let key = "group/r|waits|words";
        let contexts = BTreeMap::new();
        let long = chat(json!({"messages": [
            {"role": "user", "content": "abcd".repeat(25000)},
            {"role": "tool", "tool_call_id": "c1", "content": "x"}
        ]}));
        let hit = rule_for(key, &rules, "", &long, "", &contexts, None)
            .await
            .unwrap();
        assert!(hit.waits);
        assert_eq!(hit.n, 0);
        // a new turn is looked at afresh
        let next = chat(json!({"messages": [
            {"role": "user", "content": "abcd".repeat(25000)},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "next"}
        ]}));
        let hit = rule_for(key, &rules, "", &next, "", &contexts, None)
            .await
            .unwrap();
        assert_eq!(hit.n, 1);
        assert!(!hit.waits);
        // the rule's member changed within the turn: it waits again
        let changed = vec![Rule {
            use_: "a/small".to_owned(),
            tokens: 20000,
            ..Rule::default()
        }];
        let round = chat(json!({"messages": [
            {"role": "user", "content": "abcd".repeat(25000)},
            {"role": "user", "content": "next"},
            {"role": "tool", "tool_call_id": "c1", "content": "x"}
        ]}));
        let hit = rule_for(key, &changed, "", &round, "", &contexts, None)
            .await
            .unwrap();
        assert!(hit.waits);
        assert_eq!(hit.use_, "");
    }

    #[tokio::test]
    async fn a_turn_that_outgrows_its_model_moves_once() {
        let rules = vec![Rule {
            use_: "b/big".to_owned(),
            tokens: 50000,
            ..Rule::default()
        }];
        let key = "group/r|outgrows|hi";
        let contexts =
            BTreeMap::from([("a/small".to_owned(), 64000), ("b/big".to_owned(), 1000000)]);
        let short = chat(json!({"messages": [{"role": "user", "content": "hi"}]}));
        let hit = rule_for(key, &rules, "", &short, "", &contexts, None)
            .await
            .unwrap();
        assert_eq!(hit.n, 0);
        // just short of 95% of a's 64k: it stays
        let round = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "c1", "content": "abcd".repeat(60000)}
        ]});
        let hit = rule_for(key, &rules, "", &chat(round), "", &contexts, None)
            .await
            .unwrap();
        assert!(hit.held);
        assert!(!hit.grown);
        // at 95%: to b
        let round = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "c1", "content": "abcd".repeat(61000)}
        ]});
        let hit = rule_for(key, &rules, "", &chat(round.clone()), "", &contexts, None)
            .await
            .unwrap();
        assert!(hit.grown);
        assert_eq!(hit.n, 1);
        assert_eq!(hit.use_, "b/big");
        // and the turn's later rounds stay there
        let hit = rule_for(key, &rules, "", &chat(round.clone()), "", &contexts, None)
            .await
            .unwrap();
        assert!(hit.held);
        assert!(!hit.grown);
        assert_eq!(hit.n, 1);
    }

    #[tokio::test]
    async fn no_rules_leave_the_group_alone() {
        let key = "group/r|no-rules|hi";
        let req = chat(json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert!(
            rule_for(key, &[], "", &req, "", &BTreeMap::new(), None)
                .await
                .is_none()
        );
        assert!(!turn_rules().contains_key(key));
    }
}
