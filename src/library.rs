// The library keeps what every agent should know, once: the instructions
// each reads before a conversation, the MCP servers it can call and the
// skills it can load. magpie writes them into each agent's own files in
// that agent's own format, and takes back only what it wrote — whatever
// else is in those files stays as it was.

pub mod archive;
pub mod ccswitch;
pub mod instructions;
pub mod mcp;
pub mod rtk;
pub mod skills;

pub use instructions::{
    InstructionsChange, InstructionsView, import_instructions, read_instructions, save_instructions,
};
pub use mcp::{Found, Server, import_server, remove_server, save_server, server_agents};
pub use rtk::{RTKAgent, RTKGain, RTKView, read_rtk, rtk_takes, set_rtk};
pub use skills::{
    Candidate, Probe, Skill, SkillView, import_skill, install_skills, probe_skills, remove_skill,
    skill_agents, skill_path, skill_text, update_skill,
};

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{agent, config::atomic_write_for_settings, settings};

// ---- where the library's files are ----------------------------------------

// Store is the folder every magpie file lives in; tests build one at a
// temporary directory.
#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) root: PathBuf,
}

pub fn store() -> Store {
    Store {
        root: settings::path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    }
}

impl Store {
    pub fn at(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    fn library_path(&self) -> PathBuf {
        self.root.join("library.json")
    }

    pub(crate) fn dir(&self) -> PathBuf {
        self.root.join("library")
    }

    pub(crate) fn backup_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
}

static CHANGE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    CHANGE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---- Homes: where each agent keeps its files -------------------------------

// Homes is where the agents' files are on this machine. detect() reads the
// environment; tests build one at a temporary directory.
#[derive(Clone, Debug)]
pub(crate) struct Homes {
    pub(crate) home: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) claude: PathBuf,
    pub(crate) claude_json: PathBuf,
    pub(crate) codex: PathBuf,
    pub(crate) pi: PathBuf,
    pub(crate) copilot: PathBuf,
}

impl Homes {
    pub(crate) fn detect() -> Homes {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let config = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Homes {
            claude_json: env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|value| !value.is_empty())
                .map(|dir| PathBuf::from(&dir).join(".claude.json"))
                .unwrap_or_else(|| home.join(".claude.json")),
            claude: env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
            codex: env::var_os("CODEX_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            pi: env::var_os("PI_CODING_AGENT_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".pi/agent")),
            copilot: env::var_os("COPILOT_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".copilot")),
            home,
            config,
        }
    }

    #[cfg(test)]
    pub(crate) fn at(home: &Path) -> Homes {
        Homes {
            claude_json: home.join(".claude.json"),
            claude: home.join(".claude"),
            codex: home.join(".codex"),
            pi: home.join(".pi/agent"),
            copilot: home.join(".copilot"),
            config: home.join(".config"),
            home: home.to_path_buf(),
        }
    }
}

// ---- the agents the library can give anything to ---------------------------

static CLAUDE_DESKTOP_SPEC: agent::AgentSpec = agent::AgentSpec {
    id: "claude-desktop",
    name: "Claude Desktop",
    aliases: &[],
    executable: "",
    relative_path: "",
    format: crate::config::ConfigFormat::Jsonc,
    fields: &[],
};

pub(crate) fn agent_icon(id: &str) -> &'static str {
    match id {
        "claude" => "claudecode-color",
        "claude-desktop" => "claude-color",
        "codex" => "codex-color",
        "gemini" => "geminicli-color",
        "opencode" => "opencode",
        "pi" => "pi",
        "omp" => "omp",
        "goose" => "goose",
        "cursor" => "cursor",
        "copilot" => "githubcopilot",
        "crush" => "crush",
        "zcode" => "zcode",
        "hermes" => "hermes",
        _ => "",
    }
}

// agent_path is where the agent's own config file is, as the library needs
// it — from homes, not the environment, so tests can point it anywhere.
pub(crate) fn agent_path(homes: &Homes, id: &str) -> PathBuf {
    match id {
        "claude" => homes.claude.join("settings.json"),
        "claude-desktop" => homes.config.join("Claude/claude_desktop_config.json"),
        "codex" => homes.codex.join("config.toml"),
        "gemini" => homes.home.join(".gemini/settings.json"),
        "opencode" => {
            let directory = homes.config.join("opencode");
            let jsonc = directory.join("opencode.jsonc");
            if jsonc.exists() {
                jsonc
            } else {
                directory.join("opencode.json")
            }
        }
        "pi" => homes.pi.join("settings.json"),
        "omp" => homes.home.join(".omp/agent/config.yml"),
        "goose" => homes.config.join("goose/config.yaml"),
        "cursor" => homes.home.join(".cursor/cli-config.json"),
        "copilot" => homes.copilot.join("settings.json"),
        "crush" => homes.config.join("crush/crush.json"),
        "zcode" => homes.home.join(".zcode/v2/config.json"),
        _ => homes.home.join(".config"),
    }
}

fn agent_by_id(homes: &Homes, id: &str) -> Option<agent::Agent> {
    let (spec, path) = if id == "claude-desktop" {
        (&CLAUDE_DESKTOP_SPEC, agent_path(homes, id))
    } else {
        let spec = agent::ALL_AGENTS.iter().find(|a| a.id == id)?;
        (spec, agent_path(homes, id))
    };
    Some(agent::Agent { spec, path })
}

// detected is the agents on this machine magpie could drive, as the library
// sees them: those whose config file or folder is there.
pub(crate) fn detected(homes: &Homes) -> Vec<agent::Agent> {
    let mut ids: Vec<&str> = agent::ALL_AGENTS.iter().map(|a| a.id).collect();
    ids.push("claude-desktop");
    ids.into_iter()
        .filter_map(|id| agent_by_id(homes, id))
        .filter(|a| a.is_detected())
        .collect()
}

// Target is where one agent keeps each of the three: none is something it
// has no user-wide place for.
pub(crate) struct Target {
    pub(crate) agent: agent::Agent,
    // Instructions is the file the agent reads before every conversation;
    // override, when it exists, is read instead of it (Codex's
    // AGENTS.override.md).
    pub(crate) instructions: Option<PathBuf>,
    pub(crate) override_: Option<PathBuf>,
    pub(crate) mcp: Option<mcp::McpFile>,
    pub(crate) skills: Option<PathBuf>,
    // skills_also are agents whose skills this one reads as well, as
    // OpenCode reads Claude Code's.
    pub(crate) skills_also: Vec<String>,
    // mcp_via is the extension the agent reads its MCP servers through, for
    // one that has none of its own.
    pub(crate) mcp_via: String,
    pub(crate) note: String,
}

fn target_of(homes: &Homes, a: &agent::Agent) -> Option<Target> {
    let id = a.spec.id;
    let mut t = Target {
        agent: a.clone(),
        instructions: None,
        override_: None,
        mcp: None,
        skills: None,
        skills_also: Vec::new(),
        mcp_via: String::new(),
        note: String::new(),
    };
    match id {
        "claude" => {
            t.instructions = Some(homes.claude.join("CLAUDE.md"));
            t.mcp = Some(mcp::McpFile {
                path: homes.claude_json.clone(),
                format: mcp::Format::Claude,
            });
            t.skills = Some(homes.claude.join("skills"));
        }
        "codex" => {
            t.instructions = Some(homes.codex.join("AGENTS.md"));
            t.override_ = Some(homes.codex.join("AGENTS.override.md"));
            t.mcp = Some(mcp::McpFile {
                path: homes.codex.join("config.toml"),
                format: mcp::Format::Codex,
            });
            t.skills = Some(homes.codex.join("skills"));
        }
        "gemini" => {
            let d = homes.home.join(".gemini");
            t.instructions = Some(d.join("GEMINI.md"));
            t.mcp = Some(mcp::McpFile {
                path: d.join("settings.json"),
                format: mcp::Format::Gemini,
            });
            t.skills = Some(d.join("skills"));
        }
        "opencode" => {
            let d = a
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| homes.config.join("opencode"));
            t.instructions = Some(d.join("AGENTS.md"));
            t.note = "opencode-claude".to_owned();
            t.mcp = Some(mcp::McpFile {
                path: a.path.clone(),
                format: mcp::Format::OpenCode,
            });
            t.skills = Some(d.join("skills"));
            t.skills_also = vec!["claude".to_owned()];
        }
        "pi" => {
            t.instructions = Some(homes.pi.join("AGENTS.md"));
            // Pi has no MCP of its own: its extensions for it (pi-mcp-adapter,
            // pi-mcp-extension) both read the agent folder's mcp.json
            t.mcp = Some(mcp::McpFile {
                path: homes.pi.join("mcp.json"),
                format: mcp::Format::Pi,
            });
            t.mcp_via = "pi-mcp-adapter".to_owned();
            t.skills = Some(homes.pi.join("skills"));
        }
        "omp" => {
            let d = homes.home.join(".omp/agent");
            t.instructions = Some(d.join("AGENTS.md"));
            t.skills = Some(d.join("skills"));
        }
        "goose" => {
            t.instructions = Some(a.path.parent()?.join(".goosehints"));
            t.mcp = Some(mcp::McpFile {
                path: a.path.clone(),
                format: mcp::Format::Goose,
            });
        }
        "cursor" => {
            let d = homes.home.join(".cursor");
            t.mcp = Some(mcp::McpFile {
                path: d.join("mcp.json"),
                format: mcp::Format::Cursor,
            });
            t.skills = Some(d.join("skills"));
        }
        "copilot" => {
            t.instructions = Some(homes.copilot.join("copilot-instructions.md"));
            t.mcp = Some(mcp::McpFile {
                path: homes.copilot.join("mcp-config.json"),
                format: mcp::Format::Copilot,
            });
            t.skills = Some(homes.copilot.join("skills"));
        }
        "crush" => {
            t.instructions = Some(a.path.parent()?.join("CRUSH.md"));
            t.mcp = Some(mcp::McpFile {
                path: a.path.clone(),
                format: mcp::Format::Crush,
            });
            t.skills = Some(a.path.parent()?.join("skills"));
            t.skills_also = vec!["claude".to_owned()];
        }
        "zcode" => {
            // ZCode's own servers are its cli/config.json's mcp.servers (the
            // app's MCP settings write there); AGENTS.md and skills beside it
            let d = homes.home.join(".zcode");
            t.instructions = Some(d.join("AGENTS.md"));
            t.mcp = Some(mcp::McpFile {
                path: d.join("cli/config.json"),
                format: mcp::Format::ZCode,
            });
            t.skills = Some(d.join("skills"));
        }
        "claude-desktop" => {
            // Claude Desktop reads only commands from its file: a remote
            // server is added in its own Connectors settings
            t.mcp = Some(mcp::McpFile {
                path: a.path.clone(),
                format: mcp::Format::Desktop,
            });
        }
        _ => return None,
    }
    Some(t)
}

// targets is the agents on this machine that magpie can give any of the
// three to.
pub(crate) fn targets(homes: &Homes) -> Vec<Target> {
    detected(homes)
        .iter()
        .filter_map(|a| target_of(homes, a))
        .collect()
}

pub(crate) fn target_by_id(homes: &Homes, id: &str) -> Option<Target> {
    targets(homes).into_iter().find(|t| t.agent.spec.id == id)
}

// Takes is the id of the agent q names (its id, an alias, its name) when
// the library can give it kind — "instructions", "mcp" or "skills" — or
// why not: an agent it has no place for isn't recorded as getting it and
// then given nothing.
pub(crate) fn takes(homes: &Homes, q: &str, kind: &str) -> Result<String> {
    let app = q.eq_ignore_ascii_case("claude-desktop") || q.eq_ignore_ascii_case("Claude Desktop");
    let mut a = if app {
        agent_by_id(homes, "claude-desktop").context("agent not found")?
    } else {
        agent::find(q)?
    };
    a.path = agent_path(homes, a.spec.id);
    let t = target_of(homes, &a);
    let what = match kind {
        "instructions" => "instructions",
        "mcp" => "MCP servers",
        "skills" => "skills",
        _ => bail!("unknown kind {kind:?}"),
    };
    let has = t.as_ref().is_some_and(|t| match kind {
        "instructions" => t.instructions.is_some(),
        "mcp" => t.mcp.is_some(),
        "skills" => t.skills.is_some(),
        _ => false,
    });
    if !has {
        bail!(
            "{} has no user-wide place for {what} that magpie knows of",
            a.spec.name
        );
    }
    Ok(a.spec.id.to_owned())
}

// ---- library.json ----------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Instructions {
    #[serde(default)]
    pub agents: Vec<String>,
}

// Applied is what magpie last wrote into one agent.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Applied {
    #[serde(default, skip_serializing_if = "is_false")]
    pub instructions: bool,
    // of the part last written
    #[serde(
        default,
        rename = "instrHash",
        skip_serializing_if = "String::is_empty"
    )]
    pub instr_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

// Library is library.json: what magpie keeps, and which agents get it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub instructions: Instructions,
    #[serde(default)]
    pub mcp: Vec<mcp::Server>,
    #[serde(default)]
    pub skills: Vec<skills::Skill>,
    // Applied is what magpie wrote into each agent, so that what it takes
    // away is only ever its own.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub applied: HashMap<String, Applied>,
    // Icons are the icons of the servers added from the market, by what
    // each runs.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub icons: HashMap<String, String>,
    // kept is where a change put what it kept aside before the sync, for
    // the sync to keep the agents' files beside it.
    #[serde(skip)]
    pub(crate) kept: Option<Backups>,
}

pub(crate) fn load(store: &Store) -> Result<Library> {
    let bytes = read_optional(&store.library_path())?;
    let mut l = match bytes {
        Some(bytes) if !bytes.is_empty() => serde_json::from_slice::<Library>(&bytes)
            .with_context(|| format!("parse {}", store.library_path().display()))?,
        _ => Library::default(),
    };
    l.kept = None;
    Ok(l)
}

pub(crate) fn save(store: &Store, l: &mut Library) -> Result<()> {
    l.mcp.sort_by(|a, b| a.name.cmp(&b.name));
    l.skills.sort_by(|a, b| a.name.cmp(&b.name));
    for a in l.applied.values_mut() {
        a.mcp.sort();
        a.skills.sort();
    }
    let mut bytes = serde_json::to_vec_pretty(l).context("serialize library")?;
    bytes.push(b'\n');
    atomic_write_for_settings(&store.library_path(), &bytes)
}

impl Library {
    pub(crate) fn applied_mut(&mut self, agent: &str) -> &mut Applied {
        self.applied.entry(agent.to_owned()).or_default()
    }

    pub(crate) fn server(&self, name: &str) -> Option<&mcp::Server> {
        self.mcp.iter().find(|s| s.name == name)
    }

    pub(crate) fn server_mut(&mut self, name: &str) -> Option<&mut mcp::Server> {
        self.mcp.iter_mut().find(|s| s.name == name)
    }

    pub(crate) fn skill(&self, name: &str) -> Option<&skills::Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub(crate) fn skill_mut(&mut self, name: &str) -> Option<&mut skills::Skill> {
        self.skills.iter_mut().find(|s| s.name == name)
    }
}

// A name is what a server or a skill is called in every agent's file, and
// a skill's folder: it has to be a bare key in TOML and a plain file name.
pub(crate) fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub(crate) fn check_name(kind: &str, name: &str) -> Result<()> {
    if !is_name(name) {
        bail!("{kind} name {name:?}: use letters, digits, - and _ only");
    }
    Ok(())
}

// set turns id on or off in a list of agent ids, which stays sorted.
pub(crate) fn set(list: &mut Vec<String>, id: &str, on: bool) {
    match (on, list.iter().position(|x| x == id)) {
        (true, None) => {
            list.push(id.to_owned());
            list.sort();
        }
        (false, Some(i)) => {
            list.remove(i);
        }
        _ => {}
    }
}

// change runs f on the library under the lock, saves it, and writes what
// changed into the agents.
pub(crate) fn change(
    store: &Store,
    f: impl FnOnce(&mut Library) -> Result<()>,
) -> Result<SyncResult> {
    change_in(store, &Homes::detect(), f)
}

pub(crate) fn change_in(
    store: &Store,
    homes: &Homes,
    f: impl FnOnce(&mut Library) -> Result<()>,
) -> Result<SyncResult> {
    let _guard = lock();
    let mut l = load(store)?;
    f(&mut l)?;
    save(store, &mut l)?;
    let res = sync_library(homes, store, &mut l);
    save(store, &mut l)?;
    Ok(res)
}

// ---- backups ---------------------------------------------------------------

// How many changes' backups are kept.
const KEEP_BACKUPS: usize = 30;

// Backups copies each file once, the first time a change is about to write
// it: one folder a change, named by when, a folder an agent in it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Backups {
    pub(crate) dir: PathBuf,
    done: HashSet<PathBuf>,
}

impl Backups {
    pub(crate) fn new() -> Backups {
        Backups::default()
    }

    // keep copies path aside, as the agent had it, before it is written; a
    // file that isn't there has nothing to keep.
    pub(crate) fn keep(&mut self, agent: &str, path: &Path) -> Result<()> {
        if self.done.contains(path) {
            return Ok(());
        }
        self.done.insert(path.to_path_buf());
        let Some(data) = read_optional(path)? else {
            return Ok(());
        };
        if self.dir.as_os_str().is_empty() {
            self.dir = store().backup_dir().join(timestamp());
        }
        let dst = self.dir.join(agent).join(file_name(path)?);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&dst, data).with_context(|| format!("write {}", dst.display()))
    }
}

fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .context("path has no file name")
}

fn timestamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.{:03}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}

// prune leaves the newest KEEP_BACKUPS.
pub(crate) fn prune_backups(store: &Store) {
    let Ok(entries) = fs::read_dir(store.backup_dir()) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    while names.len() > KEEP_BACKUPS {
        let _ = fs::remove_dir_all(store.backup_dir().join(&names[0]));
        names.remove(0);
    }
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub(crate) fn read_text(path: &Path) -> String {
    read_optional(path)
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).replace("\r\n", "\n"))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

// ---- what a change did ------------------------------------------------------

// SyncResult is what a change did to the agents.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncResult {
    // agents whose files were written
    #[serde(default)]
    pub changed: Vec<String>,
    // what couldn't be written, and why
    #[serde(default)]
    pub problems: Vec<Problem>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backup: String,
    // the servers and skills (mcp:<name>, skill:<name>) a profile named that
    // the library no longer has
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

// Problem is one thing that couldn't be given to an agent.
#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    pub agent: String,
    // instructions, mcp:<name>, skill:<name>
    pub what: String,
    pub error: String,
}

impl SyncResult {
    fn changed(&mut self, agent: &str) {
        if !self.changed.iter().any(|id| id == agent) {
            self.changed.push(agent.to_owned());
        }
    }

    fn fail(&mut self, agent: &str, what: &str, error: &anyhow::Error) {
        self.problems.push(Problem {
            agent: agent.to_owned(),
            what: what.to_owned(),
            error: error.to_string(),
        });
    }
}

// sync writes the library into every agent on this machine.
fn sync_library(homes: &Homes, store: &Store, l: &mut Library) -> SyncResult {
    let mut res = SyncResult::default();
    let mut b = l.kept.take().unwrap_or_default();
    let all = targets(homes);
    for t in &all {
        instructions::sync_instructions(store, l, t, &mut b, &mut res);
        mcp::sync_mcp(l, t, &mut b, &mut res);
        skills::sync_skills(store, l, t, &mut res, &all);
    }
    res.backup = b.dir.to_string_lossy().into_owned();
    if !b.dir.as_os_str().is_empty() {
        prune_backups(store);
    }
    res
}

// Sync writes the library into the agents again: after an agent is
// installed, or a profile brings another library in.
pub fn sync() -> Result<SyncResult> {
    sync_in(&store(), &Homes::detect())
}

pub(crate) fn sync_in(store: &Store, homes: &Homes) -> Result<SyncResult> {
    change_in(store, homes, |_| Ok(()))
}

// ---- setup: a profile's memory of the library ------------------------------

// Setup is which agents get what from the library, and the instructions, as
// a profile keeps them to switch back to. The servers and the skills
// themselves stay in the library: a setup only names them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Setup {
    #[serde(default)]
    pub instructions: SetupInstructions,
    // server → the agents that get it
    #[serde(default)]
    pub mcp: BTreeMap<String, Vec<String>>,
    // skill → the agents that get it
    #[serde(default)]
    pub skills: BTreeMap<String, Vec<String>>,
}

// SetupInstructions is the shared text, the agents it is written to, and
// what each agent gets besides.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SetupInstructions {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shared: String,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

impl Setup {
    pub fn empty(&self) -> bool {
        self.mcp.is_empty()
            && self.skills.is_empty()
            && self.instructions.shared.is_empty()
            && self.instructions.agents.is_empty()
            && self.instructions.extra.is_empty()
    }

    // Summary is what a setup gives out, in a few words: the servers and
    // skills given to at least one agent, and whether any agent gets the
    // instructions.
    pub fn summary(&self) -> String {
        let count = |m: &BTreeMap<String, Vec<String>>, one: &str, many: &str| -> Option<String> {
            let n = m.values().filter(|agents| !agents.is_empty()).count();
            match n {
                0 => None,
                1 => Some(format!("1 {one}")),
                n => Some(format!("{n} {many}")),
            }
        };
        let mut parts = Vec::new();
        if let Some(p) = count(&self.mcp, "server", "servers") {
            parts.push(p);
        }
        if let Some(p) = count(&self.skills, "skill", "skills") {
            parts.push(p);
        }
        if self.gives_instructions() {
            parts.push("instructions".to_owned());
        }
        if parts.is_empty() {
            return "library: nothing on".to_owned();
        }
        format!("library: {}", parts.join(", "))
    }

    // GivesInstructions reports whether any agent gets instructions from it.
    pub fn gives_instructions(&self) -> bool {
        let i = &self.instructions;
        if !i.shared.is_empty() && !i.agents.is_empty() {
            return true;
        }
        i.agents
            .iter()
            .any(|id| i.extra.get(id).is_some_and(|text| !text.is_empty()))
    }

    // On counts the servers and the skills a setup gives to at least one
    // agent.
    pub fn on(&self) -> (usize, usize) {
        (
            self.mcp
                .values()
                .filter(|agents| !agents.is_empty())
                .count(),
            self.skills
                .values()
                .filter(|agents| !agents.is_empty())
                .count(),
        )
    }
}

fn extras(store: &Store) -> BTreeMap<String, String> {
    let dir = store.dir().join("instructions");
    let Ok(entries) = fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".md")?;
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) || check_name("agent", id).is_err()
            {
                return None;
            }
            let text = read_text(&store.dir().join("instructions").join(&name));
            (!text.is_empty()).then(|| (id.to_owned(), text))
        })
        .collect()
}

// Snapshot is the library's setup as it is now.
pub fn snapshot() -> Result<Setup> {
    snapshot_at(&store())
}

pub(crate) fn snapshot_at(store: &Store) -> Result<Setup> {
    let _guard = lock();
    let l = load(store)?;
    let s = Setup {
        instructions: SetupInstructions {
            shared: read_text(&instructions::shared_path(store)),
            agents: l.instructions.agents.clone(),
            extra: extras(store),
        },
        mcp: l
            .mcp
            .iter()
            .map(|x| (x.name.clone(), x.agents.clone()))
            .collect(),
        skills: l
            .skills
            .iter()
            .map(|x| (x.name.clone(), x.agents.clone()))
            .collect(),
    };
    Ok(s)
}

// Restore gives the servers and skills to the agents s names and puts the
// instructions back as they were, then writes the library into the agents.
// A server or skill s names that the library no longer has is left out and
// listed in SyncResult.missing; one added to the library since s was taken
// is left with the agents it has, as s knows nothing of it.
pub fn restore(s: Setup) -> Result<SyncResult> {
    restore_at(&store(), s)
}

pub(crate) fn restore_at(store: &Store, s: Setup) -> Result<SyncResult> {
    let mut texts: Vec<(PathBuf, String)> = vec![(
        instructions::shared_path(store),
        s.instructions.shared.clone(),
    )];
    for id in extras(store).keys() {
        texts.push((instructions::extra_path(store, id), String::new()));
    }
    for (id, text) in &s.instructions.extra {
        check_name("agent", id)?;
        texts.push((instructions::extra_path(store, id), text.clone()));
    }
    let mut missing: Vec<String> = Vec::new();
    let res = change(store, |l| {
        missing.clear();
        let mut b = Backups::new();
        // the text as it is, then as s has it: a file that changes is kept
        // first, with the agents' files, in the same backup
        for (p, text) in &texts {
            if read_text(p) == text.trim() {
                continue;
            }
            b.keep("library", p)?;
            instructions::write_text(p, text)?;
        }
        l.kept = Some(b);
        l.instructions.agents = s.instructions.agents.clone();
        l.instructions.agents.sort();
        for (name, agents) in &s.mcp {
            if let Some(x) = l.server_mut(name) {
                x.agents = agents.clone();
                x.agents.sort();
            } else {
                missing.push(format!("mcp:{name}"));
            }
        }
        for (name, agents) in &s.skills {
            if let Some(x) = l.skill_mut(name) {
                x.agents = agents.clone();
                x.agents.sort();
            } else {
                missing.push(format!("skill:{name}"));
            }
        }
        missing.sort();
        Ok(())
    });
    res.map(|mut res| {
        res.missing = missing;
        res
    })
}

// ---- the page --------------------------------------------------------------

// AgentView is an agent as the page lists it, with where it keeps each.
#[derive(Debug, Serialize)]
pub struct AgentView {
    pub id: String,
    pub name: String,
    pub icon: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mcp: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub skills: String,
    #[serde(default, rename = "skillsAlso", skip_serializing_if = "Vec::is_empty")]
    pub skills_also: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, rename = "noSSE", skip_serializing_if = "is_false")]
    pub no_sse: bool,
    #[serde(default, rename = "noRemote", skip_serializing_if = "is_false")]
    pub no_remote: bool,
    #[serde(rename = "mcpVia", skip_serializing_if = "String::is_empty")]
    pub mcp_via: String,
}

// ServerView is a library server, and what each agent it's on made of it.
#[derive(Debug, Serialize)]
pub struct ServerView {
    #[serde(flatten)]
    pub server: mcp::Server,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    // the agents it couldn't be given to, and why
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub problems: BTreeMap<String, String>,
}

// View is the Library page.
#[derive(Debug, Serialize)]
pub struct View {
    pub agents: Vec<AgentView>,
    pub servers: Vec<ServerView>,
    pub found_servers: Vec<mcp::Found>,
    pub skills: Vec<SkillView>,
    pub found_skills: Vec<skills::FoundSkill>,
    pub instructions: InstructionsView,
    pub dir: String,
    pub backups: String,
}

// Read is the whole page: the library, and what's found in the agents.
// problems are those of the last change, for the page to keep showing.
pub fn read(problems: &[Problem]) -> Result<View> {
    read_at(&store(), &Homes::detect(), problems)
}

pub(crate) fn read_at(store: &Store, homes: &Homes, problems: &[Problem]) -> Result<View> {
    let hidden = settings::load().agents_hidden;
    // the instructions page first, as its read takes the same lock
    let instructions = instructions::read_instructions_at(store, homes)?;
    let _guard = lock();
    let l = load(store)?;
    let targets: Vec<Target> = targets(homes)
        .into_iter()
        .filter(|t| !hidden.iter().any(|id| id == t.agent.spec.id))
        .collect();
    let mut agents = Vec::new();
    for t in &targets {
        let mut av = AgentView {
            id: t.agent.spec.id.to_owned(),
            name: t.agent.spec.name.to_owned(),
            icon: agent_icon(t.agent.spec.id).to_owned(),
            instructions: t
                .instructions
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            mcp: String::new(),
            skills: t
                .skills
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            skills_also: t.skills_also.clone(),
            note: t.note.clone(),
            no_sse: false,
            no_remote: false,
            mcp_via: t.mcp_via.clone(),
        };
        if let Some(f) = &t.mcp {
            av.mcp = f.path.to_string_lossy().into_owned();
            av.no_sse = mcp::supports(&f.format, "sse").is_some();
            av.no_remote = mcp::supports(&f.format, "http").is_some();
        }
        agents.push(av);
    }
    let of = |what: &str| -> BTreeMap<String, String> {
        problems
            .iter()
            .filter(|p| p.what == what)
            .map(|p| (p.agent.clone(), p.error.clone()))
            .collect()
    };
    let mut servers = Vec::new();
    for s in &l.mcp {
        let mut problems = of(&format!("mcp:{}", s.name));
        for t in &targets {
            if let Some(f) = &t.mcp
                && s.agents.iter().any(|id| id == t.agent.spec.id)
                && let Some(error) = mcp::supports(&f.format, &s.transport)
            {
                problems.insert(t.agent.spec.id.to_owned(), error.to_owned());
            }
        }
        servers.push(ServerView {
            server: s.clone(),
            icon: String::new(),
            problems,
        });
    }
    let mut skills = Vec::new();
    for s in &l.skills {
        let (mut source, mut kind) = (String::new(), String::new());
        if let Some(src) = &s.source {
            source = src.to_string();
            kind = src.kind.clone();
        }
        let origin = ccswitch::cc_switch_origin(homes, s)
            .map(|mut o| {
                // where CC Switch put it on disk is no path in the repository
                o.path = String::new();
                o.to_string()
            })
            .unwrap_or_default();
        let meta = skills::read_meta(&skills::skill_dir(store, &s.name));
        skills.push(SkillView {
            name: s.name.clone(),
            description: meta
                .as_ref()
                .map(|m| m.description.clone())
                .unwrap_or_default(),
            source,
            kind,
            origin,
            icon: String::new(),
            agents: s.agents.clone(),
            missing: meta.is_none(),
            problems: of(&format!("skill:{}", s.name)),
        });
    }
    Ok(View {
        agents,
        servers,
        found_servers: mcp::found_servers(&l, homes),
        found_skills: skills::found_skills(&l, homes, store),
        skills,
        instructions,
        dir: store.dir().to_string_lossy().into_owned(),
        backups: store.backup_dir().to_string_lossy().into_owned(),
    })
}

// ---- the command line -------------------------------------------------------

pub(crate) fn usage() -> &'static str {
    USAGE
}

const USAGE: &str = "magpie library                     what the library gives each agent: instructions, MCP servers, skills\n  magpie library sync                write it into the agents again (after one is installed, or edited by hand)\n  magpie library instructions        print the shared instructions\n  magpie library instructions set <file|->   replace them with a file's text (- for stdin)\n  magpie library instructions agents <a,b…|none>   the agents that get them\n  magpie library mcp add <name> <url | command args…> [agents=a,b…]\n  magpie library mcp agents <name> <a,b…|none>\n  magpie library mcp rm <name>\n  magpie library skill agents <name> <a,b…|none>\n  magpie library skill rm <name>     (skills are installed from the app's Library page)\n  magpie library rtk                 which agents run their shell commands through RTK (rtk-ai.app), to save tokens\n  magpie library rtk on|off <agent>  switch it, with RTK's own installer\n";

// Library command: the instructions, MCP servers and skills magpie keeps
// once and writes into every agent. args are those after `library`, as the
// other commands get theirs.
pub async fn command(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return status().await;
    }
    let sub = args[0].as_str();
    let rest = &args[1..];
    match sub {
        "sync" => {
            let res = sync()?;
            print_result(&res);
            Ok(())
        }
        "instructions" if rest.is_empty() => {
            let v = read_instructions()?;
            print!("{}", v.shared);
            if !v.shared.is_empty() && !v.shared.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        "instructions" => match rest {
            [verb, arg] if verb == "set" => {
                let text = if arg == "-" {
                    let mut buffer = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
                        .context("read stdin")?;
                    buffer
                } else {
                    fs::read_to_string(arg).with_context(|| format!("read {arg}"))?
                };
                let res = save_instructions(InstructionsChange {
                    shared: Some(text),
                    ..InstructionsChange::default()
                })?;
                print_result(&res);
                Ok(())
            }
            [verb, arg] if verb == "agents" => {
                let agents = agents_list(arg, "instructions")?;
                let res = save_instructions(InstructionsChange {
                    agents: Some(agents),
                    ..InstructionsChange::default()
                })?;
                print_result(&res);
                Ok(())
            }
            _ => bail!("usage:\n  {USAGE}"),
        },
        "mcp" => match rest {
            [verb, name, cmd @ ..] if verb == "add" && !name.is_empty() => {
                let mut s = Server {
                    name: name.clone(),
                    agents: Vec::new(),
                    ..Server::default()
                };
                let mut cmd = cmd.to_vec();
                let mut i = 0;
                while i < cmd.len() {
                    if let Some(v) = cmd[i].strip_prefix("agents=") {
                        s.agents = agents_list(v, "mcp")?;
                        cmd.remove(i);
                    } else {
                        i += 1;
                    }
                }
                let Some(first) = cmd.first().cloned() else {
                    bail!("a URL or a command is needed");
                };
                if first.starts_with("http://") || first.starts_with("https://") {
                    if cmd.len() > 1 {
                        bail!("a server by URL takes nothing after it");
                    }
                    s.transport = "http".to_owned();
                    s.url = first;
                } else {
                    s.transport = "stdio".to_owned();
                    s.command = first;
                    s.args = cmd[1..].to_vec();
                }
                let res = save_server("", s)?;
                print_result(&res);
                Ok(())
            }
            [verb, name, arg] if verb == "agents" => {
                let agents = agents_list(arg, "mcp")?;
                let res = server_agents(name, agents)?;
                print_result(&res);
                Ok(())
            }
            [verb, name] if verb == "rm" => {
                let res = remove_server(name)?;
                print_result(&res);
                Ok(())
            }
            _ => bail!("usage:\n  {USAGE}"),
        },
        "skill" | "skills" => match rest {
            [verb, name, arg] if verb == "agents" => {
                let agents = agents_list(arg, "skills")?;
                let res = skill_agents(name, agents)?;
                print_result(&res);
                Ok(())
            }
            [verb, name] if verb == "rm" => {
                let res = remove_skill(name)?;
                print_result(&res);
                Ok(())
            }
            _ => bail!("usage:\n  {USAGE}"),
        },
        "rtk" => rtk::rtk_command(rest).await,
        "help" | "-h" | "--help" => {
            print!("  {USAGE}");
            Ok(())
        }
        _ => bail!("no library command {sub:?}\n  {USAGE}"),
    }
}

// agentList is a comma list of agents; none (or nothing) is no agent.
fn agent_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty() && *a != "none")
        .map(str::to_owned)
        .collect()
}

// agents_list is agent_list with each agent's id, refused when the library
// has no place in it for kind.
fn agents_list(s: &str, kind: &str) -> Result<Vec<String>> {
    let homes = Homes::detect();
    agent_list(s)
        .into_iter()
        .map(|a| takes(&homes, &a, kind))
        .collect()
}

fn print_result(res: &SyncResult) {
    if res.changed.is_empty() {
        println!("✓ every agent already has it");
    } else {
        println!("✓ written into {}", res.changed.join(", "));
    }
    for p in &res.problems {
        println!("! {} {}: {}", p.agent, p.what, p.error);
    }
    for m in &res.missing {
        println!("! the library no longer has {m}");
    }
    if !res.backup.is_empty() {
        println!("  what was there before is kept in {}", res.backup);
    }
}

async fn status() -> Result<()> {
    let v = read(&[])?;
    let on = |ids: &[String]| -> String {
        if ids.is_empty() {
            "no agent".to_owned()
        } else {
            ids.join(", ")
        }
    };
    println!("Instructions");
    let to: Vec<String> = v
        .instructions
        .agents
        .iter()
        .filter(|a| a.on)
        .map(|a| a.agent.clone())
        .collect();
    if v.instructions.shared.trim().is_empty() {
        println!("  none yet · magpie library instructions set <file>");
    } else {
        let lines = v
            .instructions
            .shared
            .trim_end_matches('\n')
            .matches('\n')
            .count()
            + 1;
        println!("  {lines} lines → {}", on(&to));
    }
    for a in &v.instructions.agents {
        if a.edited {
            println!("! {} magpie's part was edited in {}", a.agent, a.path);
        }
    }
    println!("MCP servers");
    if v.servers.is_empty() {
        println!("  none yet");
    }
    for s in &v.servers {
        let what = if !s.server.command.is_empty() {
            format!("{} {}", s.server.command, s.server.args.join(" "))
                .trim()
                .to_owned()
        } else {
            s.server.url.clone()
        };
        println!("  {} {} → {}", s.server.name, what, on(&s.server.agents));
        for (a, e) in &s.problems {
            println!("    ! {a} {e}");
        }
    }
    let mut found = Vec::new();
    let mut own = Vec::new();
    for f in &v.found_servers {
        let who = f.server.agents.join(", ");
        let label = if f.own {
            format!("{} ({}'s own)", f.server.name, who)
        } else {
            format!("{} ({})", f.server.name, who)
        };
        if f.own {
            own.push(label);
        } else {
            found.push(label);
        }
    }
    if !found.is_empty() {
        println!("  in your agents, not in the library: {}", found.join(", "));
    }
    if !own.is_empty() {
        println!(
            "  added by the agent itself, left as they are: {}",
            own.join(", ")
        );
    }
    println!("Skills");
    if v.skills.is_empty() {
        println!("  none yet");
    }
    for s in &v.skills {
        println!("  {} → {}", s.name, on(&s.agents));
        if s.missing {
            println!("    ! its folder is gone");
        }
    }
    println!("  kept in {} · magpie library help", v.dir);
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::library::{Homes, Store};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    // Sandbox is a home with every agent magpie can give the library to,
    // and a magpie store beside it, all in a temporary directory.
    pub(crate) struct Sandbox {
        pub(crate) home: PathBuf,
        pub(crate) store: Store,
        pub(crate) homes: Homes,
    }

    impl Sandbox {
        pub(crate) fn new() -> Sandbox {
            let home = std::env::temp_dir().join(format!(
                "magpie-library-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&home);
            for f in [
                ".claude/settings.json",
                ".claude.json",
                ".codex/config.toml",
                ".gemini/settings.json",
                ".config/opencode/opencode.json",
                ".pi/agent/settings.json",
                ".config/goose/config.yaml",
                ".cursor/cli-config.json",
                ".copilot/settings.json",
                ".config/crush/crush.json",
            ] {
                write(home.join(f), "");
            }
            Sandbox {
                store: Store::at(home.join("magpie")),
                homes: Homes::at(&home),
                home,
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    pub(crate) fn write(path: impl AsRef<Path>, s: &str) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, s).unwrap();
    }

    pub(crate) fn read(path: impl AsRef<Path>) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    // write_bytes is a test's own file, bytes and all — CC Switch's database.
    #[cfg(unix)]
    pub(crate) fn write_bytes(path: impl AsRef<Path>, bytes: &[u8]) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    // ok fails a test on an error or a problem: ok(save_server_at(…)).
    pub(crate) fn ok(
        result: anyhow::Result<crate::library::SyncResult>,
    ) -> crate::library::SyncResult {
        let res = result.unwrap();
        assert!(
            res.problems.is_empty(),
            "problems: {}",
            res.problems
                .iter()
                .map(|p| format!("{} {}: {}", p.agent, p.what, p.error))
                .collect::<Vec<_>>()
                .join("; ")
        );
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_file_safe() {
        assert!(check_name("skill", "pdf").is_ok());
        assert!(check_name("skill", "my-skill_2").is_ok());
        assert!(check_name("skill", "").is_err());
        assert!(check_name("skill", "../x").is_err());
        assert!(check_name("skill", "-x").is_err());
        assert!(check_name("skill", "x y").is_err());
        let long = "a".repeat(65);
        assert!(check_name("skill", &long).is_err());
    }

    #[test]
    fn set_turns_agents_on_and_off_sorted() {
        let mut list = vec!["codex".to_owned()];
        set(&mut list, "claude", true);
        assert_eq!(list, vec!["claude", "codex"]);
        set(&mut list, "codex", false);
        assert_eq!(list, vec!["claude"]);
        set(&mut list, "claude", true);
        assert_eq!(list, vec!["claude"]);
    }

    #[test]
    fn setup_summary_counts_what_is_on() {
        let mut s = Setup::default();
        assert_eq!(s.summary(), "library: nothing on");
        s.mcp.insert("fs".to_owned(), vec!["claude".to_owned()]);
        s.skills.insert("pdf".to_owned(), Vec::new());
        s.instructions.shared = "hi".to_owned();
        s.instructions.agents = vec!["claude".to_owned()];
        assert_eq!(s.summary(), "library: 1 server, instructions");
        assert!(!s.empty());
    }
}
