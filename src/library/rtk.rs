// RTK (rtk-ai.app) is a CLI that the shell commands an agent runs go
// through — git status, cargo test, ls — cut down to what the model needs,
// so their output costs fewer tokens. Each agent gets it through a hook its
// own installer writes (rtk init -g …): magpie runs that installer for the
// agents switched on, and reads each one's files to say which have it,
// since rtk init --show knows only of Claude Code, OpenCode and Cursor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::agent;
use crate::library::{Backups, Homes, Store, prune_backups, store};

// RTKURL is where rtk is installed from.
pub const RTK_URL: &str = "https://www.rtk-ai.app";

// rtkSpec is how rtk's installer names one agent, and what it writes there.
struct RtkSpec {
    // after rtk init -g
    flags: &'static [&'static str],
    // patch: the installer takes --auto-patch (Codex's refuses it)
    patch: bool,
    // withClaude: the installer gives it to Claude Code too, which is
    // pointed at a folder thrown away after
    with_claude: bool,
}

fn spec(id: &str) -> Option<RtkSpec> {
    let (flags, patch, with_claude) = match id {
        "claude" => (&[][..], true, false),
        "codex" => (&["--codex"][..], false, false),
        "gemini" => (&["--gemini"][..], true, false),
        "opencode" => (&["--opencode"][..], true, true),
        "cursor" => (&["--agent", "cursor"][..], true, true),
        "copilot" => (&["--copilot"][..], true, false),
        "pi" => (&["--agent", "pi"][..], true, false),
        "omp" => (&["--agent", "omp"][..], true, false),
        "hermes" => (&["--agent", "hermes"][..], true, false),
        _ => return None,
    };
    Some(RtkSpec {
        flags,
        patch,
        with_claude,
    })
}

fn contains(path: &Path, s: &str) -> bool {
    std::fs::read(path).is_ok_and(|bytes| contains_bytes(&bytes, s))
}

fn contains_bytes(bytes: &[u8], s: &str) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.contains(s)
}

fn exists(path: &Path) -> bool {
    path.exists()
}

// has says whether the agent has rtk's hook, by its files.
fn has(homes: &Homes, a: &agent::Agent) -> bool {
    let id = a.spec.id;
    match id {
        "claude" => contains(&homes.claude.join("settings.json"), "rtk hook claude"),
        "codex" => contains(&homes.codex.join("hooks.json"), "rtk hook codex"),
        "gemini" => contains(&homes.home.join(".gemini/settings.json"), "rtk-hook-gemini"),
        "opencode" => exists(
            &a.path
                .parent()
                .unwrap_or(Path::new("."))
                .join("plugins/rtk.ts"),
        ),
        "cursor" => contains(&homes.home.join(".cursor/hooks.json"), "rtk hook cursor"),
        "copilot" => exists(&homes.copilot.join("hooks/rtk-rewrite.json")),
        "pi" => exists(&homes.home.join(".pi/agent/extensions/rtk.ts")),
        "omp" => exists(&homes.home.join(".omp/agent/extensions/rtk.ts")),
        "hermes" => exists(&homes.home.join(".hermes/plugins/rtk-rewrite/plugin.yaml")),
        _ => false,
    }
}

// touches are the files the installer edits (not those it only adds), kept
// aside by magpie first.
fn touches(homes: &Homes, a: &agent::Agent) -> Vec<PathBuf> {
    match a.spec.id {
        "claude" => vec![
            homes.claude.join("settings.json"),
            homes.claude.join("CLAUDE.md"),
        ],
        "codex" => vec![
            homes.codex.join("hooks.json"),
            homes.codex.join("AGENTS.md"),
        ],
        "gemini" => vec![
            homes.home.join(".gemini/settings.json"),
            homes.home.join(".gemini/GEMINI.md"),
        ],
        "cursor" => vec![homes.home.join(".cursor/hooks.json")],
        "copilot" => vec![homes.copilot.join("copilot-instructions.md")],
        "hermes" => vec![homes.home.join(".hermes/config.yaml")],
        _ => Vec::new(),
    }
}

// dir is the agent's folder, made first: rtk writes nothing for an agent
// whose folder isn't there yet.
fn dir(homes: &Homes, a: &agent::Agent) -> PathBuf {
    match a.spec.id {
        "claude" => homes.claude.clone(),
        "codex" => homes.codex.clone(),
        "gemini" => homes.home.join(".gemini"),
        "opencode" => a
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| homes.config.join("opencode")),
        "cursor" => homes.home.join(".cursor"),
        "copilot" => homes.copilot.clone(),
        "pi" => homes.home.join(".pi/agent"),
        "omp" => homes.home.join(".omp/agent"),
        "hermes" => homes.home.join(".hermes"),
        _ => PathBuf::new(),
    }
}

// rtkBundle: rtk's uninstaller for Claude Code or OpenCode takes it out of
// all three of these at once, so the others are put back after.
const RTK_BUNDLE: [&str; 3] = ["claude", "opencode", "cursor"];

// RTKAgent is one agent rtk can be given to, and whether it has it.
#[derive(Debug, Serialize)]
pub struct RTKAgent {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub on: bool,
}

// RTKGain is what rtk says it saved, over every command it has recorded.
#[derive(Debug, Serialize)]
pub struct RTKGain {
    pub commands: i64,
    pub input: i64,
    pub saved: i64,
    pub pct: f64,
}

// RTKView is the RTK part of the Library page.
#[derive(Debug, Serialize)]
pub struct RTKView {
    // "" when rtk isn't installed
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain: Option<RTKGain>,
    pub agents: Vec<RTKAgent>,
    pub url: String,
    // the agents a change reached, to be restarted to see it
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backup: String,
}

// rtkPath is where the rtk binary is, searched on PATH.
fn rtk_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let suffixes: Vec<String> = if cfg!(windows) {
        std::env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_else(|| vec![".EXE".to_owned(), ".BAT".to_owned(), ".CMD".to_owned()])
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).find_map(|directory| {
        suffixes.iter().find_map(|suffix| {
            let candidate = directory.join(format!("rtk{suffix}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

// rtkAgents are the agents on this machine rtk can be given to.
fn rtk_agents(homes: &Homes) -> Vec<agent::Agent> {
    crate::library::detected(homes)
        .into_iter()
        .filter(|a| spec(a.spec.id).is_some())
        .collect()
}

async fn rtk_run(bin: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut command = tokio::process::Command::new(bin);
    command
        .args(args)
        // rtk asks before a change it isn't told to make: nothing answers it
        .stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .map_err(|_| anyhow::anyhow!("rtk {} timed out", args.join(" ")))?
        .with_context(|| format!("run rtk {}", args.join(" ")))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let text = text.trim().to_owned();
    if !output.status.success() {
        let detail = if text.is_empty() {
            output.status.to_string()
        } else {
            last_lines(&text, 4)
        };
        bail!("rtk {}: {}", args.join(" "), detail);
    }
    Ok(text)
}

fn last_lines(s: &str, n: usize) -> String {
    let lines = s.trim().lines().collect::<Vec<_>>();
    if lines.len() > n {
        lines[lines.len() - n..].join(" ")
    } else {
        lines.join(" ")
    }
}

// ReadRTK says whether rtk is installed, what it saved, and which agents
// have it.
pub async fn read_rtk() -> RTKView {
    read_rtk_in(&Homes::detect(), rtk_path()).await
}

pub(crate) async fn read_rtk_in(homes: &Homes, bin: Option<PathBuf>) -> RTKView {
    let mut v = RTKView {
        path: bin
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        version: String::new(),
        gain: None,
        agents: rtk_agents(homes)
            .iter()
            .map(|a| RTKAgent {
                id: a.spec.id.to_owned(),
                name: a.spec.name.to_owned(),
                icon: crate::library::agent_icon(a.spec.id).to_owned(),
                on: has(homes, a),
            })
            .collect(),
        url: RTK_URL.to_owned(),
        restart: Vec::new(),
        backup: String::new(),
    };
    let Some(bin) = bin else {
        return v;
    };
    if let Ok(out) = rtk_run(&bin, &["--version"], &[]).await {
        v.version = out
            .trim()
            .strip_prefix("rtk")
            .unwrap_or(out.trim())
            .trim()
            .to_owned();
    }
    if let Ok(out) = rtk_run(&bin, &["gain", "--format", "json"], &[]).await
        && let Ok(g) = serde_json::from_str::<serde_json::Value>(&out)
        && let Some(summary) = g.get("summary")
        && summary
            .get("total_commands")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|commands| commands > 0)
    {
        v.gain = Some(RTKGain {
            commands: summary
                .get("total_commands")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default(),
            input: summary
                .get("total_input")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default(),
            saved: summary
                .get("total_saved")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default(),
            pct: summary
                .get("avg_savings_pct")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or_default(),
        });
    }
    v
}

// SetRTK gives rtk to an agent, or takes it away, with rtk's own installer.
pub async fn set_rtk(id: &str, on: bool) -> Result<RTKView> {
    set_rtk_in(&store(), &Homes::detect(), rtk_path(), id, on, &[]).await
}

pub(crate) async fn set_rtk_in(
    store: &Store,
    homes: &Homes,
    bin: Option<PathBuf>,
    id: &str,
    on: bool,
    env: &[(&str, &str)],
) -> Result<RTKView> {
    let Some(bin) = bin else {
        bail!("rtk isn't installed — get it from {RTK_URL}");
    };
    if spec(id).is_none() {
        bail!("rtk has no hook for {id}");
    }
    let agents: HashMap<String, agent::Agent> = rtk_agents(homes)
        .into_iter()
        .map(|a| (a.spec.id.to_owned(), a))
        .collect();
    let Some(a) = agents.get(id) else {
        bail!("{id} isn't installed");
    };
    if has(homes, a) == on {
        return Ok(read_rtk_in(homes, Some(bin)).await);
    }
    // what else the uninstaller takes, to put back
    let mut back: Vec<&agent::Agent> = Vec::new();
    if !on && (id == "claude" || id == "opencode") {
        for other in RTK_BUNDLE {
            if other != id
                && let Some(b) = agents.get(other)
                && has(homes, b)
            {
                back.push(b);
            }
        }
    }
    let mut b = Backups::new();
    for x in std::iter::once(a).chain(back.iter().copied()) {
        for p in touches(homes, x) {
            b.keep(x.spec.id, &p)?;
        }
    }
    if on {
        rtk_install(homes, &bin, env, a).await?;
    } else {
        let mut args = vec!["init", "-g"];
        args.extend_from_slice(spec(id).unwrap().flags);
        args.push("--uninstall");
        rtk_run(&bin, &args, env).await?;
        for x in &back {
            if let Err(error) = rtk_install(homes, &bin, env, x).await {
                bail!(
                    "rtk was taken out of {}, but putting it back into {} failed: {error}",
                    a.spec.name,
                    x.spec.name
                );
            }
        }
        if has(homes, a) {
            bail!("rtk's uninstaller left its hook in {}", a.spec.name);
        }
    }
    if !b.dir.as_os_str().is_empty() {
        prune_backups(store);
    }
    let mut v = read_rtk_in(homes, Some(bin)).await;
    // those put back are as they were: only this one has anything new
    v.restart = vec![id.to_owned()];
    v.backup = b.dir.to_string_lossy().into_owned();
    Ok(v)
}

async fn rtk_install(
    homes: &Homes,
    bin: &Path,
    env: &[(&str, &str)],
    a: &agent::Agent,
) -> Result<()> {
    let sp = spec(a.spec.id).context("rtk has no hook for this agent")?;
    let d = dir(homes, a);
    if !d.as_os_str().is_empty() {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    let mut args = vec!["init", "-g"];
    args.extend_from_slice(sp.flags);
    if sp.patch {
        args.push("--auto-patch");
    }
    let mut env_all: Vec<(&str, &str)> = env.to_vec();
    let tmp;
    let mut tmp_text = String::new();
    if sp.with_claude {
        tmp =
            std::env::temp_dir().join(format!("magpie-rtk-{}-{}", std::process::id(), now_nanos()));
        std::fs::create_dir_all(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        tmp_text = tmp.to_string_lossy().into_owned();
        env_all.push(("CLAUDE_CONFIG_DIR", tmp_text.as_str()));
    }
    let out = rtk_run(bin, &args, &env_all).await;
    if sp.with_claude {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    let out = out?;
    if !has(homes, a) {
        bail!(
            "rtk's installer didn't add its hook to {}: {}",
            a.spec.name,
            last_lines(&out, 3)
        );
    }
    Ok(())
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// RTKTakes is the id of the agent q names when rtk can be given to it.
pub fn rtk_takes(q: &str) -> Result<String> {
    let a = agent::find(q)?;
    if spec(a.spec.id).is_none() {
        let mut ids: Vec<&str> = [
            "claude", "codex", "gemini", "opencode", "cursor", "copilot", "pi", "omp", "hermes",
        ]
        .into_iter()
        .collect();
        ids.sort();
        bail!(
            "rtk has no hook for {} (it has for {})",
            a.spec.name,
            ids.join(", ")
        );
    }
    Ok(a.spec.id.to_owned())
}

// rtkCommand is magpie library rtk …: RTK's hook in each agent.
pub(crate) async fn rtk_command(args: &[String]) -> Result<()> {
    let v = match args {
        [] => read_rtk().await,
        [verb, name] if verb == "on" || verb == "off" => {
            let id = rtk_takes(name)?;
            set_rtk(&id, verb == "on").await?
        }
        _ => bail!("usage:\n  {}", crate::library::usage()),
    };
    if v.path.is_empty() {
        println!("! rtk isn't installed — {}", v.url);
    } else {
        println!("RTK {} · {}", v.version, v.path);
        if let Some(g) = &v.gain {
            println!(
                "  {} tokens saved over {} commands ({:.0}% on average)",
                g.saved, g.commands, g.pct
            );
        }
    }
    for a in &v.agents {
        let mark = if a.on { "on " } else { "off" };
        println!("  {mark} {}", a.name);
    }
    if v.agents.is_empty() {
        println!("  none of the agents here is one RTK has a hook for");
    }
    if !v.restart.is_empty() {
        println!("  restart {} for it to take effect", v.restart.join(", "));
    }
    if !v.backup.is_empty() {
        println!("  what was there before is kept in {}", v.backup);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::library::testing::{Sandbox, write};

    // fakeRTK is an rtk on PATH that acts as rtk 0.50's installer does for
    // Claude Code, OpenCode and Cursor: installing OpenCode or Cursor gives
    // it to Claude Code as well, and uninstalling Claude Code or OpenCode
    // takes all three away.
    const FAKE_RTK: &str = r#"#!/bin/sh
c="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
claude() { [ -d "$c" ] && echo '{"hooks":{"PreToolUse":[{"hooks":[{"command":"rtk hook claude"}]}]}}' > "$c/settings.json"; }
case "$1" in
--version) echo "rtk 9.9.9"; exit 0 ;;
gain) echo '{"summary":{"total_commands":3,"total_input":100,"total_saved":60,"avg_savings_pct":60}}'; exit 0 ;;
esac
shift 2
case "$*" in
"--auto-patch") claude ;;
"--opencode --auto-patch") claude; mkdir -p "$XDG_CONFIG_HOME/opencode/plugins"; echo x > "$XDG_CONFIG_HOME/opencode/plugins/rtk.ts" ;;
"--agent cursor --auto-patch") claude; echo '{"hooks":{"preToolUse":[{"command":"rtk hook cursor"}]}}' > "$HOME/.cursor/hooks.json" ;;
"--agent cursor --uninstall") echo '{}' > "$HOME/.cursor/hooks.json" ;;
"--uninstall"|"--opencode --uninstall")
	echo '{}' > "$c/settings.json"; rm -f "$XDG_CONFIG_HOME/opencode/plugins/rtk.ts"; echo '{}' > "$HOME/.cursor/hooks.json" ;;
*) echo "unknown: $*" >&2; exit 2 ;;
esac
"#;

    struct FakeRtk {
        bin: PathBuf,
        env: Vec<(&'static str, String)>,
    }

    impl FakeRtk {
        fn new(sb: &Sandbox) -> FakeRtk {
            let bin = sb.home.join("bin/rtk");
            write(&bin, FAKE_RTK);
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            FakeRtk {
                bin,
                env: vec![
                    ("HOME", sb.home.to_string_lossy().into_owned()),
                    (
                        "XDG_CONFIG_HOME",
                        sb.home.join(".config").to_string_lossy().into_owned(),
                    ),
                    (
                        "CLAUDE_CONFIG_DIR",
                        sb.home.join(".claude").to_string_lossy().into_owned(),
                    ),
                ],
            }
        }

        fn env(&self) -> Vec<(&'static str, &str)> {
            self.env
                .iter()
                .map(|(k, v)| (*k, v.as_str()))
                .collect::<Vec<_>>()
        }
    }

    async fn view(sb: &Sandbox, rtk: Option<&FakeRtk>) -> RTKView {
        read_rtk_in(&sb.homes, rtk.map(|r| r.bin.clone())).await
    }

    async fn switch(sb: &Sandbox, rtk: &FakeRtk, id: &str, on: bool) {
        set_rtk_in(
            &sb.store,
            &sb.homes,
            Some(rtk.bin.clone()),
            id,
            on,
            &rtk.env(),
        )
        .await
        .unwrap();
    }

    async fn want(sb: &Sandbox, rtk: &FakeRtk, claude: bool, opencode: bool, cursor: bool) {
        let v = view(sb, Some(rtk)).await;
        let on = |id: &str| v.agents.iter().find(|a| a.id == id).unwrap().on;
        assert!(
            on("claude") == claude && on("opencode") == opencode && on("cursor") == cursor,
            "claude, opencode, cursor = {}, {}, {}; want {claude}, {opencode}, {cursor}",
            on("claude"),
            on("opencode"),
            on("cursor")
        );
    }

    #[tokio::test]
    async fn test_rtk() {
        let sb = Sandbox::new();
        let rtk = FakeRtk::new(&sb);

        let v = view(&sb, Some(&rtk)).await;
        assert_eq!(v.version, "9.9.9");
        assert_eq!(v.gain.as_ref().unwrap().saved, 60);
        want(&sb, &rtk, false, false, false).await;
        // OpenCode's and Cursor's installers leave Claude Code as it was
        switch(&sb, &rtk, "opencode", true).await;
        switch(&sb, &rtk, "cursor", true).await;
        want(&sb, &rtk, false, true, true).await;
        switch(&sb, &rtk, "claude", true).await;
        want(&sb, &rtk, true, true, true).await;
        // Claude Code's uninstaller takes the other two with it: they're put back
        switch(&sb, &rtk, "claude", false).await;
        want(&sb, &rtk, false, true, true).await;
        switch(&sb, &rtk, "claude", true).await;
        switch(&sb, &rtk, "opencode", false).await;
        want(&sb, &rtk, true, false, true).await;
        switch(&sb, &rtk, "cursor", false).await;
        want(&sb, &rtk, true, false, false).await;

        assert!(
            set_rtk_in(
                &sb.store,
                &sb.homes,
                Some(rtk.bin.clone()),
                "goose",
                true,
                &rtk.env()
            )
            .await
            .is_err(),
            "goose has no rtk hook, yet it was switched on"
        );
        let v = read_rtk_in(&sb.homes, None).await;
        assert!(
            v.path.is_empty(),
            "rtk found at {} with no binary given",
            v.path
        );
        assert!(
            set_rtk_in(&sb.store, &sb.homes, None, "claude", false, &rtk.env())
                .await
                .is_err(),
            "switched without rtk installed"
        );
    }
}
