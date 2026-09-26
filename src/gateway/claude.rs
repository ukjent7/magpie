// Claude Subscription is driven through the genuine Claude Code binary rather
// than by replaying its OAuth token over HTTP: Anthropic reads what a prompt
// asks for, not just the credentials it comes with, and a prompt written by
// another harness can be sent to Extra Usage for all that the account is paid
// for. The subprocess keeps Claude Code's own identity, and the caller's tools
// reach it as an MCP server of magpie's — see bridge.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;
use tokio::sync::mpsc;

use super::bridge::{self, Hooks, Run, Spec};
use super::ir::{Event, EventKind, Request, Usage, new_id, stop_from_anthropic};
use super::prompt;

const NAME: &str = "claude";

// Where the caller's tools are written for the helper to read, and what
// Claude Code puts on their names as its own MCP server's.
const TOOLS: &str = "tools.json";
const MCP_SERVER: &str = "mcp__magpie__";

// ask is Claude Code's answer to a request: the run that had this
// conversation's last turn takes this one on while it is still waiting, and a
// new process is started only when there is none.
pub async fn ask(
    req: &Request,
    model: &str,
    owner: &str,
    token: &str,
) -> Result<(Arc<Run>, mpsc::UnboundedReceiver<Event>)> {
    // A conversation Claude Code already has goes on in the same process, so
    // that what it has cached of the prompt is read again rather than written
    // anew: a new run would be told the whole history as one message, which
    // shares nothing with what was cached but Claude Code's own prompt.
    if let Some((so_far, since)) = bridge::resume_turn(req) {
        let key = bridge::turn_key(owner, req, so_far);
        if let Some(gone_on) = bridge::resume(&key, said(prompt::claude_turn(since))).await {
            return Ok(gone_on);
        }
    }
    start(req, model, owner, token).await
}

// start is a new Claude Code, told the whole conversation at once.
async fn start(
    req: &Request,
    model: &str,
    owner: &str,
    token: &str,
) -> Result<(Arc<Run>, mpsc::UnboundedReceiver<Event>)> {
    let binary = installed()?;
    let helper = env::current_exe().context("find magpie")?;
    // serializing the tools first: it is the one thing here that can fail
    // while the folder it would be written into is still nothing's to clean up
    let tools =
        serde_json::to_vec(&bridge::bridge_tools(req)).context("read the caller's tools")?;
    let dir = env::temp_dir().join(format!(
        "magpie-{NAME}-{}-{}",
        std::process::id(),
        new_id()
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let tools_path = dir.join(TOOLS);
    if let Err(error) = std::fs::write(&tools_path, tools) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(error).with_context(|| format!("write {}", tools_path.display()));
    }

    // the run owns the folder from here: letting it go takes the folder, the
    // process and everything waiting on it with it
    let run = Run::new(Spec {
        model: model.to_owned(),
        tmp: dir.clone(),
        owner: owner.to_owned(),
        patience: None,
        hooks: Hooks::default(),
    });
    let reading = run.attach().await;
    let config = json!({"mcpServers": {"magpie": {
        "command": helper.display().to_string(),
        "args": [bridge::HELPER, bridge::callback_url(run.token()), tools_path.display().to_string()],
    }}})
    .to_string();

    let mut command = crate::proc::async_command(binary);
    command
        .args(cli_args(model, &config, &req.effort))
        .current_dir(&dir);
    for blocked in BLOCKED {
        command.env_remove(blocked);
    }
    command
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "0")
        .env("DISABLE_AUTO_COMPACT", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    if !token.is_empty() {
        // a saved account in use beside the one Claude Code is signed in to
        command.env("CLAUDE_CODE_OAUTH_TOKEN", token);
    }
    let stdout = match bridge::open(&run, &mut command).await {
        Ok(stdout) => stdout,
        Err(error) => {
            run.abort().await;
            return Err(error).context("start Claude Code");
        }
    };

    let watching = run.clone();
    tokio::spawn(async move { read(&watching, stdout).await });

    if let Err(error) = run.tell(said(prompt::claude(req))).await {
        run.abort().await;
        return Err(error).context("ask Claude Code");
    }
    Ok((run, reading))
}

// Claude Code's own sign-in is what runs, so nothing that points it at another
// gateway, or tells it that it is inside one, comes through to it.
const BLOCKED: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

// said is one turn as Claude Code is fed it over its own input.
fn said(blocks: Vec<Value>) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": blocks}})
}

// cli_args is what makes Claude Code answer once, in a stream, with nothing of
// its own to work on: no built-in tools, no settings, no MCP servers but
// magpie's.
fn cli_args(model: &str, mcp_config: &str, effort: &str) -> Vec<String> {
    let mut args = vec![
        "-p".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--include-partial-messages".to_owned(),
        "--verbose".to_owned(),
        "--model".to_owned(),
        model.to_owned(),
        "--tools".to_owned(),
        String::new(),
        "--strict-mcp-config".to_owned(),
        "--mcp-config".to_owned(),
        mcp_config.to_owned(),
        "--setting-sources".to_owned(),
        String::new(),
        "--dangerously-skip-permissions".to_owned(),
    ];
    if !effort.is_empty() {
        // what Claude Code calls its deepest thinking is what magpie calls
        // the highest effort
        let effort = if effort == "xhigh" { "max" } else { effort };
        args.extend([
            "--effort".to_owned(),
            effort.to_owned(),
            "--thinking-display".to_owned(),
            "summarized".to_owned(),
        ]);
    }
    args
}

// installed is where Claude Code's own binary is: on the PATH, or in one of
// the folders its installer uses.
fn installed() -> Result<PathBuf> {
    if let Some(found) = crate::agent::on_path(NAME) {
        return Ok(found);
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let further = home
        .map(|home| home.join(".local").join("bin").join(NAME))
        .into_iter()
        .chain([
            PathBuf::from("/usr/local/bin").join(NAME),
            PathBuf::from("/opt/homebrew/bin").join(NAME),
        ]);
    for candidate in further {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "Claude Code is not installed; install it and run `claude auth login`"
    ))
}

// read says what the process's own lines tell about its answer, until one ends
// the turn: the process stays for the conversation's next one.
async fn read(run: &Run, stdout: ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(None) => break,
            Ok(Some(line)) => {
                let (events, ended) = reads(&line);
                for ev in events {
                    run.emit(ev).await;
                }
                if ended {
                    run.end_segment().await;
                }
            }
            Err(error) => {
                run.emit(Event {
                    kind: EventKind::Error,
                    text: error.to_string(),
                    ..Event::default()
                })
                .await;
                break;
            }
        }
    }
}

// reads is one of Claude Code's lines, as far as it goes: the events it comes
// to, and whether it ends the turn. Lines of any other kind — the whole
// assistant message Claude Code repeats itself, its system and result records
// — say nothing on their own, bar an error it ends with.
fn reads(line: &str) -> (Vec<Event>, bool) {
    let Ok(envelope) = serde_json::from_str::<Line>(line) else {
        return (Vec::new(), false);
    };
    if envelope.kind == "result" {
        return if envelope.failed {
            (
                vec![Event {
                    kind: EventKind::Error,
                    text: envelope.result,
                    ..Event::default()
                }],
                false,
            )
        } else {
            (Vec::new(), false)
        };
    }
    if envelope.kind != "stream_event" {
        return (Vec::new(), false);
    }
    let said = envelope.event;
    match said.kind.as_str() {
        "message_start" => (
            vec![Event {
                kind: EventKind::Start,
                msg_id: said.message.id,
                model: said.message.model,
                usage: said.message.usage.usage(),
                ..Event::default()
            }],
            false,
        ),
        "content_block_start" => match said.block.kind.as_str() {
            "tool_use" => {
                let named = said
                    .block
                    .name
                    .strip_prefix(MCP_SERVER)
                    .unwrap_or(&said.block.name)
                    .to_owned();
                (
                    vec![Event {
                        kind: EventKind::ToolStart,
                        id: said.block.id,
                        name: named,
                        ..Event::default()
                    }],
                    false,
                )
            }
            "text" if !said.block.text.is_empty() => (
                vec![Event {
                    kind: EventKind::Text,
                    text: said.block.text,
                    ..Event::default()
                }],
                false,
            ),
            _ => (Vec::new(), false),
        },
        "content_block_delta" => {
            let Some((kind, text)) = (match said.delta.kind.as_str() {
                "text_delta" => Some((EventKind::Text, said.delta.text)),
                "thinking_delta" => Some((EventKind::Think, said.delta.thinking)),
                "signature_delta" => Some((EventKind::Sig, said.delta.signature)),
                "input_json_delta" => Some((EventKind::ToolArgs, said.delta.arguments)),
                _ => None,
            }) else {
                return (Vec::new(), false);
            };
            (
                vec![Event {
                    kind,
                    text,
                    ..Event::default()
                }],
                false,
            )
        }
        "message_delta" => {
            let mut events = vec![Event {
                kind: EventKind::Usage,
                usage: said.usage.usage(),
                ..Event::default()
            }];
            if !said.delta.stop.is_empty() {
                events.push(Event {
                    kind: EventKind::Stop,
                    stop: stop_from_anthropic(&said.delta.stop).to_owned(),
                    ..Event::default()
                });
            }
            (events, false)
        }
        "message_stop" => (Vec::new(), true),
        _ => (Vec::new(), false),
    }
}

// line is one of what Claude Code says over its own output, which is a stream
// of its events wrapped in what the CLI adds around them.
#[derive(Debug, Default, Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(rename = "is_error", default)]
    failed: bool,
    #[serde(default)]
    result: String,
    #[serde(default)]
    event: Announce,
}

#[derive(Debug, Default, Deserialize)]
struct Announce {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: Opening,
    #[serde(rename = "content_block", default)]
    block: Block,
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    usage: Counted,
}

#[derive(Debug, Default, Deserialize)]
struct Opening {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: Counted,
}

#[derive(Debug, Default, Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    signature: String,
    #[serde(rename = "partial_json", default)]
    arguments: String,
    #[serde(rename = "stop_reason", default)]
    stop: String,
}

// counted is what the CLI counts, which is Messages' own shape rather than the
// one magpie keeps a reply's cost in.
#[derive(Debug, Default, Deserialize)]
struct Counted {
    #[serde(rename = "input_tokens", default)]
    input: i64,
    #[serde(rename = "output_tokens", default)]
    output: i64,
    #[serde(rename = "cache_read_input_tokens", default)]
    cache_read: i64,
    #[serde(rename = "cache_creation_input_tokens", default)]
    cache_write: i64,
    #[serde(rename = "output_tokens_details", default)]
    details: Detailed,
}

#[derive(Debug, Default, Deserialize)]
struct Detailed {
    #[serde(rename = "thinking_tokens", default)]
    reasoning: i64,
}

impl Counted {
    fn usage(&self) -> Usage {
        Usage {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            reasoning: self.details.reasoning,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(line: Value) -> (Vec<Event>, bool) {
        reads(&line.to_string())
    }

    fn stream(event: Value) -> Value {
        json!({"type": "stream_event", "event": event})
    }

    #[test]
    fn the_answer_is_asked_as_a_turn_with_nothing_of_its_own() {
        let args = cli_args("claude-sonnet-4-5", "{}", "");
        let joined = args.join(" ");
        assert!(joined.contains("-p --output-format stream-json --input-format stream-json"), "{joined}");
        assert!(joined.contains("--model claude-sonnet-4-5"), "{joined}");
        assert!(joined.contains("--tools  --strict-mcp-config"), "{joined}");
        assert!(joined.contains("--setting-sources  --dangerously-skip-permissions"), "{joined}");
        assert!(!joined.contains("--effort"), "{joined}");
        let thinking = cli_args("m", "{}", "xhigh");
        let at = thinking.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(&thinking[at..at + 4], ["--effort", "max", "--thinking-display", "summarized"]);
        let less = cli_args("m", "{}", "low");
        let at = less.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(less[at + 1], "low");
    }

    #[test]
    fn a_turn_is_written_as_claude_code_reads_it() {
        assert_eq!(
            said(vec![json!({"type": "text", "text": "hi"})]),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}})
        );
    }

    #[test]
    fn a_message_start_opens_the_answer() {
        let (events, ended) = event(stream(json!({
            "type": "message_start",
            "message": {"id": "msg_1", "model": "claude-opus-4-1", "usage": {
                "input_tokens": 11, "output_tokens": 2,
                "cache_read_input_tokens": 7, "cache_creation_input_tokens": 3,
                "output_tokens_details": {"thinking_tokens": 1},
            }},
        })));
        assert!(!ended);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Start);
        assert_eq!(events[0].msg_id, "msg_1");
        assert_eq!(events[0].model, "claude-opus-4-1");
        assert_eq!(
            events[0].usage,
            Usage {
                input: 11,
                output: 2,
                cache_read: 7,
                cache_write: 3,
                reasoning: 1,
            }
        );
    }

    #[test]
    fn a_tool_block_is_named_as_the_caller_knows_it() {
        let (events, _) = event(stream(json!({
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "id": "c1", "name": "mcp__magpie__read"},
        })));
        assert_eq!(events[0].kind, EventKind::ToolStart);
        assert_eq!(events[0].id, "c1");
        assert_eq!(events[0].name, "read");
        let (theirs, _) = event(stream(json!({
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "id": "c2", "name": "Bash"},
        })));
        assert_eq!(theirs[0].name, "Bash");
        let (nothing, _) = event(stream(json!({
            "type": "content_block_start",
            "content_block": {"type": "thinking"},
        })));
        assert!(nothing.is_empty());
    }

    #[test]
    fn what_opens_with_words_is_said_at_once() {
        let (events, _) = event(stream(json!({
            "type": "content_block_start",
            "content_block": {"type": "text", "text": "here"},
        })));
        assert_eq!(events[0].kind, EventKind::Text);
        assert_eq!(events[0].text, "here");
        let (empty, _) = event(stream(json!({
            "type": "content_block_start",
            "content_block": {"type": "text", "text": ""},
        })));
        assert!(empty.is_empty());
    }

    #[test]
    fn every_kind_of_delta_is_read_as_itself() {
        for (delta, kind, text) in [
            (
                json!({"type": "text_delta", "text": "a"}),
                EventKind::Text,
                "a",
            ),
            (
                json!({"type": "thinking_delta", "thinking": "b"}),
                EventKind::Think,
                "b",
            ),
            (
                json!({"type": "signature_delta", "signature": "c"}),
                EventKind::Sig,
                "c",
            ),
            (
                json!({"type": "input_json_delta", "partial_json": "{}"}),
                EventKind::ToolArgs,
                "{}",
            ),
        ] {
            let (events, _) = event(stream(json!({"type": "content_block_delta", "delta": delta})));
            assert_eq!(events.len(), 1, "{delta}");
            assert_eq!(events[0].kind, kind);
            assert_eq!(events[0].text, text);
        }
    }

    #[test]
    fn a_stop_reason_ends_the_turn_with_the_counts() {
        let (events, ended) = event(stream(json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"output_tokens": 9},
        })));
        assert!(!ended, "the counts come before the stop");
        assert_eq!(events[0].kind, EventKind::Usage);
        assert_eq!(events[0].usage.output, 9);
        assert_eq!(events[1].kind, EventKind::Stop);
        assert_eq!(events[1].stop, "tool");
        let (open, _) = event(stream(json!({
            "type": "message_delta",
            "delta": {"type": "text_delta"},
            "usage": {},
        })));
        assert_eq!(open.len(), 1, "a turn still going says no stop");
        let (over, _) = event(json!({"type": "stream_event", "event": {"type": "message_stop"}}));
        assert!(over.is_empty());
    }

    #[test]
    fn a_result_that_failed_is_the_error_the_caller_hears() {
        let (events, _) = event(json!({"type": "result", "is_error": true, "result": "out of quota"}));
        assert_eq!(events[0].kind, EventKind::Error);
        assert_eq!(events[0].text, "out of quota");
        let (whole, _) = event(json!({"type": "result", "result": "all right"}));
        assert!(whole.is_empty());
        let (echoed, _) = event(json!({"type": "assistant", "message": {"content": []}}));
        assert!(echoed.is_empty(), "what the CLI repeats says nothing new");
        let (junk, _) = reads("not json at all");
        assert!(junk.0.is_empty() && !junk.1);
        let (other, _) = event(json!({"type": "system", "subtype": "init"}));
        assert!(other.is_empty());
    }

    #[test]
    fn a_turn_of_a_conversation_is_written_whole() {
        let req = Request {
            model: "claude-sonnet-4-5".to_owned(),
            system: "be brief".to_owned(),
            ..Request::default()
        };
        let line = said(prompt::claude(&req));
        assert_eq!(line["type"], "user");
        assert_eq!(line["message"]["role"], "user");
        let blocks = line["message"]["content"].as_array().unwrap();
        assert!(!blocks.is_empty());
        assert_eq!(blocks[0]["type"], "text");
        assert!(
            blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("be brief")
        );
    }
}
