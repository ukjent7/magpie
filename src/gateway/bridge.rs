// The bridge: an agent's own binary driven as a model. Its answers are read
// back as events, and the caller's tools are offered to it as an MCP server
// of magpie's — a call there waits here until the caller's next request
// carries its result, which is what lets one turn span several round trips.
//
// What each agent is told, and how its words are read, is its runner's
// business; this is the part every agent shares: the run, its process, the
// calls parked on it and the conversations its runs are kept for.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, watch};

use super::ir::{Event, Kind, Message, Part, Request};

// A run left for its conversation's next turn waits that long at most, and
// this many are kept, the longest waiting let go first: each is a process.
const IDLE_LONGEST: Duration = Duration::from_secs(20 * 60);
const IDLE_MOST: usize = 6;

// How long one turn may take before the agent is let go.
const TURN_LONGEST: Duration = Duration::from_secs(30 * 60);

// What a caller waits on: an agent's answer to one of its tool calls, which
// took longer than the agent's patience to have one.
pub const WAIT_TOOL: &str = "magpie_wait";

// HELPER is the command that starts magpie's own MCP server: it is given the
// callback to post calls to, and the file the caller's tools are in.
pub const HELPER: &str = "claude-mcp-helper";

// outcome is what the caller sent back for one tool call.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    blocks: Vec<Value>,
    is_error: bool,
}

impl Outcome {
    fn words(text: &str) -> Outcome {
        Outcome {
            blocks: vec![json!({ "type": "text", "text": text })],
            is_error: false,
        }
    }

    fn failure(text: &str) -> Outcome {
        Outcome {
            blocks: vec![json!({ "type": "text", "text": text })],
            is_error: true,
        }
    }

    // json is the answer the helper hands to the agent.
    fn json(&self) -> Value {
        let mut out = json!({ "content": self.blocks });
        if self.is_error {
            out["is_error"] = json!(true);
        }
        out
    }
}

// call is one tool call's answer. The run keeps the only way to give it, so
// letting go of a call is how everyone waiting on one learns the run has
// ended — which is the caller's own request that made it, and its later wait.
struct Call {
    seen: watch::Receiver<Option<Outcome>>,
    send: watch::Sender<Option<Outcome>>,
}

impl Call {
    fn new() -> Call {
        let (send, seen) = watch::channel(None);
        Call { seen, send }
    }

    fn give(&self, outcome: Outcome) {
        let _ = self.send.send(Some(outcome));
    }

    // waited is what a waiter holds of a call: a hold on its answer, never a
    // way to give one.
    fn waited(&self) -> Seen {
        Seen(self.seen.clone())
    }
}

// seen is one waiter's hold on a call its run still owns.
#[derive(Clone)]
struct Seen(watch::Receiver<Option<Outcome>>);

impl Seen {
    // answered waits for the caller's result, and gives up when the run ends
    // without one.
    async fn answered(&self) -> Option<Outcome> {
        let mut seen = self.0.clone();
        loop {
            if let Some(outcome) = seen.borrow_and_update().as_ref().cloned() {
                return Some(outcome);
            }
            seen.changed().await.ok()?;
        }
    }
}

// What an agent's own stream does not tell the caller, which its runner says
// through these: an agent whose stream carries no tool calls at all (Cursor,
// the ACP ones) learns of them from the helper instead, and opens a resumed
// turn with the message start its stream has none of.
#[derive(Default)]
pub struct Hooks {
    on_call: Option<Arc<dyn Fn(&str, &str, &Value) + Send + Sync>>,
    on_begin: Option<Arc<dyn Fn() -> Event + Send + Sync>>,
    on_resume: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Hooks {
    #[must_use]
    pub fn on_call(self, hook: impl Fn(&str, &str, &Value) + Send + Sync + 'static) -> Hooks {
        Hooks {
            on_call: Some(Arc::new(hook)),
            ..self
        }
    }

    #[must_use]
    pub fn on_begin(self, hook: impl Fn() -> Event + Send + Sync + 'static) -> Hooks {
        Hooks {
            on_begin: Some(Arc::new(hook)),
            ..self
        }
    }

    #[must_use]
    pub fn on_resume(self, hook: impl Fn() + Send + Sync + 'static) -> Hooks {
        Hooks {
            on_resume: Some(Arc::new(hook)),
            ..self
        }
    }

    fn called(&self, id: &str, name: &str, args: &Value) {
        if let Some(on_call) = &self.on_call {
            on_call(id, name, args);
        }
    }

    fn began(&self) -> Option<Event> {
        self.on_begin.as_ref().map(|begin| begin())
    }

    fn resumed(&self) {
        if let Some(on_resume) = &self.on_resume {
            on_resume();
        }
    }
}

// Spec is what a run is asked for, bar the token it calls back with.
pub struct Spec {
    pub model: String,
    // where the run's own folder is, which goes with it
    pub tmp: PathBuf,
    // which account it runs as, so only its own conversation is resumed
    pub owner: String,
    // how long one of the agent's calls may park before it is told to wait
    // for the result in words, which is what its MCP client will not give up
    // on
    pub patience: Option<Duration>,
    pub hooks: Hooks,
}

// run is one agent process: the turn the caller is reading, the calls waiting
// on it, and the conversation it may go idle for.
pub struct Run {
    token: String,
    model: String,
    tmp: PathBuf,
    owner: String,
    patience: Option<Duration>,
    hooks: Hooks,
    inner: Mutex<Inner>,
    // the task waiting on the process, which kills it by ending
    reaper: Mutex<Option<tokio::task::JoinHandle<()>>>,
    // the task watching how long the turn takes
    watchdog: Mutex<Option<tokio::task::AbortHandle>>,
}

#[derive(Default)]
struct Inner {
    // the turn the caller is reading, if anyone is
    segment: Option<mpsc::UnboundedSender<Event>>,
    // calls the agent has made that the caller has not answered
    waiting: HashMap<String, Call>,
    // results the caller sent for calls the agent has not made yet: an agent
    // makes its calls one after another, while a client runs them all at once
    early: HashMap<String, Outcome>,
    // calls that outlasted the agent's patience, to be collected by wait_tool
    late: HashMap<String, Seen>,
    stderr: Vec<u8>,
    closed: bool,
    // which conversation this run waits for, once it is idle
    idle_key: String,
    // the agent's own input, kept open between its turns
    stdin: Option<ChildStdin>,
    // which turn is being watched, so an old watch cannot end a new one
    turn: u64,
}

// the runs the bridge knows: by the token their helper calls back with, by
// the tool call they are waiting on, and by the conversation they idle for.
#[derive(Default)]
struct Bridge {
    by_token: HashMap<String, Arc<Run>>,
    by_call: HashMap<String, Arc<Run>>,
    idle: HashMap<String, Idle>,
}

struct Idle {
    run: Arc<Run>,
    since: Instant,
}

static BRIDGE: LazyLock<Mutex<Bridge>> = LazyLock::new(|| Mutex::new(Bridge::default()));

impl Run {
    // new names a run's callback and gives it a turn to be read.
    #[must_use]
    pub fn new(spec: Spec) -> Arc<Run> {
        Arc::new(Run {
            token: random_token(),
            model: spec.model,
            tmp: spec.tmp,
            owner: spec.owner,
            patience: spec.patience,
            hooks: spec.hooks,
            inner: Mutex::new(Inner::default()),
            reaper: Mutex::new(None),
            watchdog: Mutex::new(None),
        })
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    #[must_use]
    pub fn patience(&self) -> Option<Duration> {
        self.patience
    }

    // attach opens a turn for a caller to read.
    pub async fn attach(&self) -> mpsc::UnboundedReceiver<Event> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.inner.lock().await.segment = Some(sender);
        receiver
    }

    // attached is whether a caller is reading the turn.
    pub async fn attached(&self) -> bool {
        self.inner.lock().await.segment.is_some()
    }

    // emit says one thing about the turn, while a caller is reading it.
    pub async fn emit(&self, ev: Event) {
        if let Some(sender) = &self.inner.lock().await.segment {
            let _ = sender.send(ev);
        }
    }

    // end_segment closes the turn a caller is reading, which is how a reply
    // ends without the run ending: the agent is still there for the next one.
    pub async fn end_segment(&self) {
        self.inner.lock().await.segment.take();
    }

    // tell sends the agent its next turn.
    pub async fn tell(&self, line: Value) -> std::io::Result<()> {
        let mut inner = self.inner.lock().await;
        let stdin = inner.stdin.as_mut().ok_or_else(|| no_pipe("input"))?;
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        stdin.write_all(&bytes).await?;
        stdin.flush().await
    }

    // keeps_input is whether the agent's input is still open: a run without
    // it ends by itself rather than waiting for another turn.
    pub async fn keeps_input(&self) -> bool {
        self.inner.lock().await.stdin.is_some()
    }

    // last_error is the tail of what the agent said on its own, which is
    // usually the reason it stopped answering.
    pub async fn last_error(&self, otherwise: &str) -> String {
        let inner = self.inner.lock().await;
        String::from_utf8_lossy(&inner.stderr)
            .lines()
            .next_back()
            .filter(|line| !line.trim().is_empty())
            .unwrap_or(otherwise)
            .to_owned()
    }

    // arm gives the run the time one turn, or one wait between turns, is
    // worth: after that the agent is let go.
    pub async fn arm(run: &Arc<Run>, wait: Duration) {
        let turn = {
            let mut inner = run.inner.lock().await;
            inner.turn += 1;
            inner.turn
        };
        let watching = run.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            if watching.inner.lock().await.turn == turn {
                watching.stop().await;
            }
        });
        let old = run.watchdog.lock().await.replace(timer.abort_handle());
        if let Some(old) = old {
            old.abort();
        }
    }

    // stop ends the run however it was going: the process is let go of, which
    // kills it, and everything waiting on it with it.
    async fn stop(&self) {
        if let Some(reaper) = self.reaper.lock().await.take() {
            reaper.abort();
        }
        self.finish().await;
    }

    // abort is a caller's way of stopping a run, watchdog included.
    pub async fn abort(&self) {
        if let Some(watchdog) = self.watchdog.lock().await.take() {
            watchdog.abort();
        }
        self.stop().await;
    }

    // finish says the process is gone: nothing else may wait on it.
    async fn finish(&self) {
        let waiting = {
            let mut inner = self.inner.lock().await;
            if inner.closed {
                return;
            }
            inner.closed = true;
            inner.stdin.take();
            inner.segment.take();
            inner.late.clear();
            inner
                .waiting
                .drain()
                .map(|(_, call)| call)
                .collect::<Vec<_>>()
        };
        // letting go of a call is how its waiters learn the run has ended
        drop(waiting);
        remove_run(self).await;
    }
}

// open starts the agent and puts its run where its helper's callbacks find
// it. What the process says on its own is kept aside for the error that says
// why it stopped; its answers are the runner's to read, its input the run's
// to keep open for the turn after.
pub async fn open(run: &Arc<Run>, command: &mut Command) -> std::io::Result<ChildStdout> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| no_pipe("input"))?;
    let stdout = child.stdout.take().ok_or_else(|| no_pipe("output"))?;
    let stderr = child.stderr.take().ok_or_else(|| no_pipe("error"))?;
    run.inner.lock().await.stdin = Some(stdin);
    BRIDGE
        .lock()
        .await
        .by_token
        .insert(run.token.clone(), run.clone());

    let minding = run.clone();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buf = [0u8; 8 * 1024];
        while let Ok(read) = stderr.read(&mut buf).await {
            if read == 0 {
                break;
            }
            let mut inner = minding.inner.lock().await;
            // up to a megabyte of it, which is more than any reason needs
            if inner.stderr.len() < 1 << 20 {
                inner.stderr.extend_from_slice(&buf[..read]);
            }
        }
    });

    let watching = run.clone();
    let reaper = tokio::spawn(async move {
        let mut child = child;
        let _ = child.wait().await;
        watching.finish().await;
    });
    *run.reaper.lock().await = Some(reaper);
    // A caller may abandon a turn after receiving the calls in it: the parked
    // agent and the callback waiting on it are not to be left alive forever.
    Run::arm(run, TURN_LONGEST).await;
    Ok(stdout)
}

fn no_pipe(what: &str) -> std::io::Error {
    std::io::Error::other(format!("the agent's {what} is closed"))
}

// remove_run takes the run out of every place it was found by, and its folder
// with it.
async fn remove_run(run: &Run) {
    let key = run.inner.lock().await.idle_key.clone();
    let mut bridge = BRIDGE.lock().await;
    bridge.by_token.remove(&run.token);
    if !key.is_empty()
        && bridge
            .idle
            .get(&key)
            .is_some_and(|idle| idle.run.token == run.token)
    {
        bridge.idle.remove(&key);
    }
    bridge
        .by_call
        .retain(|_, waiting| waiting.token != run.token);
    drop(bridge);
    let _ = std::fs::remove_dir_all(&run.tmp);
}

// ended is told how a turn's reply went. A run whose reply was whole and
// asked for nothing more waits for the conversation's next turn; one that
// failed, was cut short or went unheard is let go.
pub async fn ended(run: &Arc<Run>, req: &Request, said: &str, stop: &str, ok: bool) {
    if !run.keeps_input().await || (stop == "tool" && ok) {
        return; // it ends by itself, or waits on its tool calls
    }
    if !ok || stop != "stop" {
        run.abort().await;
        return;
    }
    let mut so_far = req.messages.clone();
    so_far.push(Message {
        role: "assistant".to_owned(),
        parts: vec![Part {
            kind: Kind::Text,
            text: said.to_owned(),
            ..Part::default()
        }],
    });
    let key = turn_key(&run.owner, req, &so_far);
    run.inner.lock().await.idle_key.clone_from(&key);
    let mut waiting: Vec<Arc<Run>> = Vec::new();
    {
        let mut bridge = BRIDGE.lock().await;
        if let Some(old) = bridge.idle.insert(
            key,
            Idle {
                run: run.clone(),
                since: Instant::now(),
            },
        ) {
            waiting.push(old.run);
        }
        while bridge.idle.len() > IDLE_MOST {
            let oldest = bridge
                .idle
                .iter()
                .min_by_key(|(_, idle)| idle.since)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            if let Some(idle) = bridge.idle.remove(&oldest) {
                waiting.push(idle.run);
            }
        }
    }
    Run::arm(run, IDLE_LONGEST).await;
    for idle in waiting {
        idle.abort().await;
    }
}

// turn_key is what a conversation has been so far: whom it runs as, the model,
// its settings and tools, and what was said in it. Whitespace, reasoning and
// how a reply is split into messages are left out, as clients keep those
// differently.
#[must_use]
pub fn turn_key(owner: &str, req: &Request, messages: &[Message]) -> String {
    let tools = serde_json::to_string(&req.tools).unwrap_or_default();
    let mut said = format!(
        "{owner}\0{}\0{}\0{}\0{}\0{}",
        req.model, req.effort, req.tool_choice, req.system, tools
    );
    let mut role = String::new();
    for message in messages {
        let mut words = String::new();
        for part in &message.parts {
            match part.kind {
                Kind::Text => {
                    words.push_str(&part.text);
                    words.push(' ');
                }
                Kind::ToolCall => {
                    let _ = write!(words, "\u{1}call {} {} ", part.name, part.id);
                }
                Kind::ToolResult => {
                    let _ = write!(words, "\u{1}result {} {} ", part.call_id, part.text);
                }
                Kind::Image => {
                    let _ = write!(words, "\u{1}image {} {} ", part.data.len(), part.url);
                }
                // how a reply reasoned is no part of what it said
                Kind::Thinking => {}
            }
        }
        let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
        if words.is_empty() {
            continue;
        }
        if message.role != role {
            role.clone_from(&message.role);
            let _ = write!(said, "\0{role}:");
        }
        let _ = write!(said, "{words} ");
    }
    hex(&Sha256::digest(said))
}

// resume_turn is a conversation a waiting run may be given another turn in:
// what has been said so far and, after the last reply, what the caller has
// said since — nothing of it answering a tool call, which is a turn still on
// its way rather than a new one.
#[must_use]
pub fn resume_turn(req: &Request) -> Option<(&[Message], &[Message])> {
    let so_far = req.messages.iter().rposition(|m| m.role == "assistant")? + 1;
    let since = &req.messages[so_far..];
    if since.is_empty()
        || since
            .iter()
            .any(|m| m.parts.iter().any(|part| part.kind == Kind::ToolResult))
    {
        return None;
    }
    Some((&req.messages[..so_far], since))
}

// resume hands the agent its conversation's next turn, which is only what the
// caller has said since the last reply.
pub async fn resume(key: &str, line: Value) -> Option<(Arc<Run>, mpsc::UnboundedReceiver<Event>)> {
    let waiting = BRIDGE.lock().await.idle.remove(key);
    let Some(idle) = waiting else {
        return None;
    };
    let run = idle.run;
    run.inner.lock().await.idle_key.clear();
    if run.inner.lock().await.closed {
        run.abort().await;
        return None;
    }
    let segment = run.attach().await;
    Run::arm(&run, TURN_LONGEST).await;
    if run.tell(line).await.is_err() {
        run.abort().await;
        return None;
    }
    Some((run, segment))
}

// find_run is the run a request's new tool results are for, and those results:
// the ones sent since the last assistant message. They are one run's when
// every call of them it knows is that run's; the rest it has not made yet.
pub async fn find_run(req: &Request) -> Option<(Arc<Run>, Vec<Part>)> {
    let mut fresh: Vec<Part> = Vec::new();
    for message in req
        .messages
        .iter()
        .rev()
        .take_while(|m| m.role != "assistant")
    {
        let mut results = message
            .parts
            .iter()
            .filter(|part| part.kind == Kind::ToolResult)
            .cloned()
            .collect::<Vec<_>>();
        results.extend(fresh);
        fresh = results;
    }
    if fresh.is_empty() {
        return None;
    }
    // an agent ends its message just before scheduling the calls in it, so a
    // very fast client can answer one before it has been registered: the few
    // milliseconds that takes are given a bounded grace.
    for _ in 0..20 {
        let found = {
            let bridge = BRIDGE.lock().await;
            let mut found: Option<Arc<Run>> = None;
            for part in &fresh {
                let Some(run) = bridge.by_call.get(&part.call_id) else {
                    continue;
                };
                if found.as_ref().is_some_and(|one| one.token != run.token) {
                    found = None;
                    break;
                }
                found = Some(run.clone());
            }
            found
        };
        if let Some(run) = found {
            return Some((run, fresh));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

// continue_with hands the agent its tool results: at once for the calls it is
// waiting on, the others kept until it makes them.
pub async fn continue_with(
    run: &Arc<Run>,
    results: Vec<Part>,
) -> anyhow::Result<mpsc::UnboundedReceiver<Event>> {
    Run::arm(run, TURN_LONGEST).await;
    let segment = run.attach().await;
    if let Some(began) = run.hooks.began() {
        run.emit(began).await;
    }
    let wanted = call_ids(&results);
    let mut delivered = 0;
    for part in results {
        let outcome = Outcome {
            blocks: vec![json!({ "type": "text", "text": part.text })],
            is_error: part.is_error,
        };
        let waiting = {
            let mut inner = run.inner.lock().await;
            let waiting = inner.waiting.remove(&part.call_id);
            if waiting.is_none() && !inner.closed {
                inner.early.insert(part.call_id.clone(), outcome.clone());
            }
            waiting
        };
        let Some(waiting) = waiting else { continue };
        delivered += 1;
        BRIDGE.lock().await.by_call.remove(&part.call_id);
        waiting.give(outcome);
    }
    if delivered == 0 {
        run.abort().await;
        anyhow::bail!("the agent is not waiting for tool results {wanted}");
    }
    run.hooks.resumed();
    Ok(segment)
}

// mcp_call is the callback an agent's helper posts its tool calls to. It
// answers when the caller sends the result, which is how one of the agent's
// turns spans the caller's several requests.
pub async fn mcp_call(token: &str, body: Value) -> Response {
    let run = BRIDGE.lock().await.by_token.get(token).cloned();
    let Some(run) = run else {
        return (StatusCode::NOT_FOUND, "unknown or expired run").into_response();
    };
    let id = body
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, "invalid tool call").into_response();
    }
    let called = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let arguments = body.get("arguments").cloned().unwrap_or(Value::Null);
    if called == WAIT_TOOL && run.patience.is_some() {
        return awaited(&run, id, &arguments).await;
    }

    // The run keeps the call; this request keeps only a hold on its answer, so
    // that the run ending is what ends the wait.
    let call = Call::new();
    let waiting = call.waited();
    let given = {
        let mut inner = run.inner.lock().await;
        match inner.early.remove(id) {
            Some(outcome) => Some(outcome),
            None => {
                inner.waiting.insert(id.to_owned(), call);
                None
            }
        }
    };
    if let Some(outcome) = given {
        return answer(&outcome);
    }
    BRIDGE
        .lock()
        .await
        .by_call
        .insert(id.to_owned(), run.clone());
    run.hooks.called(id, &called, &arguments);

    let answered = match run.patience {
        Some(wait) => tokio::select! {
            outcome = waiting.answered() => outcome,
            () = tokio::time::sleep(wait) => {
                // the call stays where it was made, too: the caller's next
                // request answers both
                run.inner.lock().await.late.insert(id.to_owned(), waiting.clone());
                return answer(&still_running(id, &called));
            }
        },
        None => waiting.answered().await,
    };
    match answered {
        Some(outcome) => answer(&outcome),
        None => (StatusCode::GONE, "the agent's run ended").into_response(),
    }
}

// awaited answers wait_tool: the late result once it is in, or that the call
// is still running.
async fn awaited(run: &Run, id: &str, arguments: &Value) -> Response {
    let wanted = arguments
        .get("call")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_owned();
    let waiting = run.inner.lock().await.late.get(&wanted).cloned();
    let Some(waiting) = waiting else {
        return answer(&Outcome::failure(&format!(
            "No tool call {wanted:?} is running."
        )));
    };
    // the agent's patience again, so that one wait never holds the turn twice
    // over
    let Some(patience) = run.patience else {
        return answer(&still_running(&wanted, ""));
    };
    match tokio::time::timeout(patience, waiting.answered()).await {
        Ok(outcome) => {
            run.inner.lock().await.late.remove(&wanted);
            match outcome {
                Some(outcome) => answer(&outcome),
                None => (StatusCode::GONE, "the agent's run ended").into_response(),
            }
        }
        Err(_) => answer(&still_running(&wanted, "")),
    }
}

fn answer(outcome: &Outcome) -> Response {
    Json(outcome.json()).into_response()
}

// bridge_tools is the caller's tools as the agent's helper is told them.
#[must_use]
pub fn bridge_tools(req: &Request) -> Vec<Value> {
    req.tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.schema,
            })
        })
        .collect()
}

// still_running is what an agent is told about a call that has no answer yet,
// and how to wait for one.
#[must_use]
pub fn still_running(id: &str, name: &str) -> Outcome {
    let what = if name.is_empty() {
        "The tool call".to_owned()
    } else {
        format!("The {name} call")
    };
    Outcome::words(&format!(
        "{what} is still running in the user's environment. Call {WAIT_TOOL} with \
         {{\"call\": \"{id}\"}} to wait for its result. Do not make the call again."
    ))
}

// call_ids names the results a request carries, for the error that says none
// of them were waited on.
#[must_use]
pub fn call_ids(parts: &[Part]) -> String {
    parts
        .iter()
        .map(|part| part.call_id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// callback_url is where an agent's helper posts its calls, and random_token
// names one run's: the token is a secret as much as a name, since it is what
// lets a process reach the turns waiting for it.
#[must_use]
pub fn callback_url(token: &str) -> String {
    format!("{}/_magpie/claude-mcp/{token}", super::url())
}

#[must_use]
pub fn random_token() -> String {
    let mut bytes = [0u8; 24];
    let _ = getrandom::fill(&mut bytes);
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ir::{EventKind, Tool};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn message(role: &str, parts: Vec<Part>) -> Message {
        Message {
            role: role.to_owned(),
            parts,
        }
    }

    fn said(words: &str) -> Part {
        Part {
            kind: Kind::Text,
            text: words.to_owned(),
            ..Part::default()
        }
    }

    fn asked() -> Request {
        Request {
            model: "m".to_owned(),
            system: "be brief".to_owned(),
            ..Request::default()
        }
    }

    #[test]
    fn a_turn_key_reads_the_same_words_in_any_wrapping() {
        let req = asked();
        let one = turn_key(
            "owner",
            &req,
            &[message("user", vec![said("what  a   day")])],
        );
        let other = turn_key(
            "owner",
            &req,
            &[
                message("user", vec![said("what")]),
                message("user", vec![said("a day")]),
            ],
        );
        assert_eq!(one, other, "one turn is one turn however it came");
        let asked_by = turn_key(
            "someone",
            &req,
            &[message("user", vec![said("what a day")])],
        );
        assert_ne!(one, asked_by, "whose turn it is belongs to it");
    }

    #[test]
    fn a_turn_key_reads_reasoning_as_no_words_at_all() {
        let req = asked();
        let plain = turn_key("o", &req, &[message("assistant", vec![said("x")])]);
        let reasoned = turn_key(
            "o",
            &req,
            &[message(
                "assistant",
                vec![
                    Part {
                        kind: Kind::Thinking,
                        text: "hmm".to_owned(),
                        ..Part::default()
                    },
                    said("x"),
                ],
            )],
        );
        assert_eq!(plain, reasoned);
        let called = turn_key(
            "o",
            &req,
            &[message(
                "assistant",
                vec![Part {
                    kind: Kind::ToolCall,
                    id: "c1".to_owned(),
                    name: "read".to_owned(),
                    ..Part::default()
                }],
            )],
        );
        assert_ne!(plain, called, "a call is a thing that was done");
        let tooling = Request {
            tools: vec![Tool {
                name: "read".to_owned(),
                description: String::new(),
                schema: json!({"type": "object"}),
            }],
            ..req
        };
        assert_ne!(
            plain,
            turn_key("o", &tooling, &[message("assistant", vec![said("x")])]),
            "what could be done belongs to what was"
        );
    }

    #[test]
    fn a_turn_key_tells_one_speaker_from_the_next() {
        let req = asked();
        let asked_then_said = turn_key(
            "o",
            &req,
            &[
                message("user", vec![said("x")]),
                message("assistant", vec![said("y")]),
            ],
        );
        let said_then_asked = turn_key(
            "o",
            &req,
            &[
                message("assistant", vec![said("y")]),
                message("user", vec![said("x")]),
            ],
        );
        assert_ne!(asked_then_said, said_then_asked);
        let twice = turn_key(
            "o",
            &req,
            &[
                message("user", vec![said("x")]),
                message("assistant", vec![said("y")]),
                message("user", vec![said("x")]),
                message("assistant", vec![said("y")]),
            ],
        );
        assert_ne!(
            asked_then_said, twice,
            "a repeated turn is still another one"
        );
    }

    #[test]
    fn a_turn_after_a_reply_only_reads_what_was_asked_since() {
        let req = asked();
        let messages = vec![
            message("user", vec![said("do it")]),
            message("assistant", vec![said("done")]),
            message("user", vec![said("and again")]),
        ];
        let with_more = Request {
            messages: messages.clone(),
            ..asked()
        };
        let (so_far, since) = resume_turn(&with_more).expect("a turn after a reply");
        assert_eq!(so_far, &messages[..2]);
        assert_eq!(since, &messages[2..]);
        assert_eq!(
            turn_key("o", &req, so_far),
            turn_key("o", &req, &[messages[0].clone(), messages[1].clone()])
        );
    }

    #[test]
    fn a_turn_that_answers_a_call_is_not_a_new_one() {
        let req = Request {
            messages: vec![
                message("user", vec![said("do it")]),
                message("assistant", vec![said("done")]),
                message(
                    "user",
                    vec![Part {
                        kind: Kind::ToolResult,
                        call_id: "c1".to_owned(),
                        text: "here".to_owned(),
                        ..Part::default()
                    }],
                ),
            ],
            ..asked()
        };
        assert!(resume_turn(&req).is_none(), "that call is still on its way");
        let nothing_after = Request {
            messages: vec![message("user", vec![said("do it")])],
            ..req
        };
        assert!(
            resume_turn(&nothing_after).is_none(),
            "no reply has been made to go on from"
        );
    }

    #[test]
    fn the_tools_are_named_as_the_helper_wants_them() {
        let req = Request {
            tools: vec![Tool {
                name: "read".to_owned(),
                description: "a file".to_owned(),
                schema: json!({"type": "object"}),
            }],
            ..Request::default()
        };
        assert_eq!(
            bridge_tools(&req),
            vec![json!({
                "name": "read",
                "description": "a file",
                "inputSchema": {"type": "object"},
            })]
        );
    }

    #[test]
    fn a_waiting_call_says_how_to_wait_for_it() {
        let still = still_running("c1", "read");
        let text = still.blocks[0]["text"].as_str().unwrap();
        assert!(text.contains(WAIT_TOOL), "{text}");
        assert!(text.contains(r#"{"call": "c1"}"#), "{text}");
        assert!(text.starts_with("The read call"), "{text}");
        assert!(!still.is_error);
        assert_eq!(still.json().get("is_error"), None);
        let unnamed = still_running("c1", "");
        assert!(
            unnamed.blocks[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("The tool call")
        );
    }

    #[test]
    fn a_result_is_written_as_the_helper_reads_it() {
        assert_eq!(
            Outcome::words("hi").json(),
            json!({"content": [{"type": "text", "text": "hi"}]})
        );
        assert_eq!(Outcome::failure("no").json()["is_error"], json!(true));
    }

    #[test]
    fn results_are_named_for_the_calls_they_answer() {
        let parts = vec![
            said("words"),
            Part {
                kind: Kind::ToolResult,
                call_id: "c1".to_owned(),
                ..Part::default()
            },
            Part {
                kind: Kind::ToolResult,
                call_id: "c2".to_owned(),
                ..Part::default()
            },
        ];
        assert_eq!(call_ids(&parts), ", c1, c2");
    }

    #[test]
    fn a_run_is_found_by_its_own_token() {
        let run = Run::new(Spec {
            model: "m".to_owned(),
            tmp: PathBuf::from("nowhere"),
            owner: "p\0u".to_owned(),
            patience: None,
            hooks: Hooks::default(),
        });
        assert_eq!(run.model(), "m");
        assert_eq!(run.owner(), "p\0u");
        assert_eq!(run.token().len(), 48);
        assert!(run.patience().is_none());
        assert_ne!(
            run.token(),
            Run::new(Spec {
                model: String::new(),
                tmp: PathBuf::from("nowhere"),
                owner: String::new(),
                patience: None,
                hooks: Hooks::default(),
            })
            .token()
        );
    }

    #[tokio::test]
    async fn a_call_answered_before_it_is_made_is_not_lost() {
        let call = Call::new();
        let waiting = call.waited();
        call.give(Outcome::words("early"));
        assert_eq!(
            waiting.answered().await.unwrap().blocks[0]["text"],
            json!("early")
        );
    }

    #[tokio::test]
    async fn a_call_left_without_an_answer_says_the_run_ended() {
        let call = Call::new();
        let waiting = call.waited();
        drop(call);
        assert!(waiting.answered().await.is_none());
    }

    #[tokio::test]
    async fn one_run_holds_its_turn_open_and_another_cannot_read_it() {
        let run = Run::new(Spec {
            model: "m".to_owned(),
            tmp: PathBuf::from("nowhere"),
            owner: "o".to_owned(),
            patience: None,
            hooks: Hooks::default(),
        });
        let mut reading = run.attach().await;
        assert!(run.attached().await);
        run.emit(Event {
            kind: EventKind::Text,
            text: "hi".to_owned(),
            ..Event::default()
        })
        .await;
        assert_eq!(reading.recv().await.unwrap().text, "hi");
        run.end_segment().await;
        // an agent says what it has to say with nobody reading it
        run.emit(Event {
            kind: EventKind::Text,
            text: "gone".to_owned(),
            ..Event::default()
        })
        .await;
        assert!(reading.recv().await.is_none(), "the turn is over");
        assert!(!run.attached().await);
        // without a process there is no input to keep for the next turn
        assert!(!run.keeps_input().await);
        assert!(run.tell(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn what_an_agent_said_on_its_own_reads_as_its_reason() {
        let run = Run::new(Spec {
            model: "m".to_owned(),
            tmp: PathBuf::from("nowhere"),
            owner: "o".to_owned(),
            patience: None,
            hooks: Hooks::default(),
        });
        assert_eq!(run.last_error("nothing").await, "nothing");
        {
            let mut inner = run.inner.lock().await;
            inner.stderr.extend_from_slice(b"first\nout of quota\n");
        }
        assert_eq!(run.last_error("nothing").await, "out of quota");
    }

    #[test]
    fn hooks_say_what_an_agent_stream_leaves_out() {
        let calls = Arc::new(AtomicUsize::new(0));
        let made = calls.clone();
        let after = calls.clone();
        let hooks = Hooks::default()
            .on_call(move |_, _, _| {
                made.fetch_add(1, Ordering::SeqCst);
            })
            .on_begin(|| Event {
                kind: EventKind::Start,
                msg_id: "msg_1".to_owned(),
                ..Event::default()
            })
            .on_resume(move || {
                after.fetch_add(1, Ordering::SeqCst);
            });
        hooks.called("c1", "read", &json!({}));
        hooks.resumed();
        assert_eq!(hooks.began().unwrap().msg_id, "msg_1");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let silent = Hooks::default();
        silent.called("c1", "read", &json!({}));
        silent.resumed();
        assert!(silent.began().is_none());
    }

    #[tokio::test]
    async fn a_run_without_a_process_stops_cleanly() {
        let run = Run::new(Spec {
            model: "m".to_owned(),
            tmp: PathBuf::from("nowhere"),
            owner: "o".to_owned(),
            patience: None,
            hooks: Hooks::default(),
        });
        let token = run.token().to_owned();
        let reading = run.attach().await;
        drop(reading);
        run.abort().await;
        run.abort().await;
        assert!(run.inner.lock().await.closed);
        assert!(!BRIDGE.lock().await.by_token.contains_key(&token));
    }

    #[test]
    fn a_callback_is_addressed_to_the_running_gateway() {
        let url = callback_url("abc");
        assert!(url.ends_with("/_magpie/claude-mcp/abc"), "{url}");
        assert!(url.starts_with("http://"), "{url}");
    }
}
