// Skills: a folder with a SKILL.md, kept in the library and linked into
// the agents that get it. One comes from a GitHub repository it can be
// updated from, or a folder on this machine the library links to, so that
// editing the folder is editing the skill.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use time::OffsetDateTime;

use crate::library::{Homes, Library, Store, SyncResult, Target, change_in, check_name, is_name, store, timestamp};

// Skill is a folder with a SKILL.md, kept in the library and linked into
// the agents that get it.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Skill {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default)]
    pub agents: Vec<String>,
}

// Source is where a skill came from: a GitHub repository it can be updated
// from, or a folder on this machine the library links to, so that editing
// the folder is editing the skill.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Source {
    // github | folder
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    #[serde(rename = "ref", default, skip_serializing_if = "String::is_empty")]
    pub ref_: String,
    // in the repository
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    // the folder, for kind folder
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.kind == "folder" {
            return write!(f, "{}", self.dir);
        }
        let mut url = format!("https://github.com/{}", self.repo);
        if !self.ref_.is_empty() || !self.path.is_empty() {
            let ref_ = if self.ref_.is_empty() { "HEAD" } else { &self.ref_ };
            url.push_str(&format!("/tree/{ref_}"));
            if !self.path.is_empty() {
                url.push('/');
                url.push_str(&self.path);
            }
        }
        write!(f, "{url}")
    }
}

pub(crate) fn skills_dir(store: &Store) -> PathBuf {
    store.dir().join("skills")
}

pub(crate) fn skill_dir(store: &Store, name: &str) -> PathBuf {
    skills_dir(store).join(name)
}

// meta is what a SKILL.md says of itself in its front matter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) name: String,
    pub(crate) description: String,
}

pub(crate) fn read_meta(dir: &Path) -> Option<Meta> {
    let bytes = std::fs::read(dir.join("SKILL.md")).ok()?;
    Some(parse_meta(&bytes))
}

// parse_meta reads a SKILL.md's front matter.
pub(crate) fn parse_meta(bytes: &[u8]) -> Meta {
    let mut m = Meta::default();
    let s = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    if let Some(rest) = s.strip_prefix("---\n")
        && let Some((front, _)) = rest.split_once("\n---")
        && let Ok(document) = yaml_edit::Document::from_str(front)
    {
        let value = yaml_edit::YamlValue::from_document(&document);
        if let Some(mapping) = value.as_mapping() {
            m.name = mapping
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            m.description = mapping
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    m.name = m.name.trim().to_owned();
    m
}

// name_for is the name a skill found at dir goes by: what its SKILL.md
// says, else its folder's, made fit for a file name.
fn name_for(dir: &Path, m: &Meta) -> String {
    let n = if m.name.is_empty() {
        dir.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        m.name.clone()
    };
    let n = n
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['-', '_'])
        .to_owned();
    n.chars().take(64).collect()
}

// ---- linking into agents ---------------------------------------------------

// marker is left in a copy magpie makes where it can't link (Windows
// without the right to), to know the copy for its own.
const MARKER: &str = ".magpie-library";

// ours reports whether the entry at p is magpie's link (or copy) of the
// library's skill by that name.
pub(crate) fn ours(p: &Path, name: &str, store: &Store) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return std::fs::read_link(p)
            .map(|to| paths_equal(&to, &skill_dir(store, name)))
            .unwrap_or(false);
    }
    if meta.is_dir() {
        return p.join(MARKER).exists();
    }
    false
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    clean(a) == clean(b)
}

fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn make_link(target: &Path, p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, p)
            .with_context(|| format!("link {}", p.display()))
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, p)
            .with_context(|| format!("link {}", p.display()))
    }
}

fn link(p: &Path, name: &str, store: &Store) -> Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let target = skill_dir(store, name);
    if make_link(&target, p).is_ok() {
        return Ok(());
    }
    if cfg!(not(windows)) {
        // the link failed and there is no copy to fall back to
        return make_link(&target, p);
    }
    // Windows without the right to link: a copy, with the marker in it
    if let Err(error) = copy_dir(&target, p) {
        let _ = std::fs::remove_dir_all(p);
        return Err(error);
    }
    std::fs::write(p.join(MARKER), format!("copied from {}\n", target.display()))
        .with_context(|| format!("write marker in {}", p.display()))
}

fn unlink(p: &Path) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        return std::fs::remove_file(p).with_context(|| format!("remove {}", p.display()));
    }
    std::fs::remove_dir_all(p).with_context(|| format!("remove {}", p.display()))
}

// real_dir is where a folder really is, links followed.
pub(crate) fn real_dir(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| clean(p))
}

pub(crate) fn sync_skills(
    store: &Store,
    l: &mut Library,
    t: &Target,
    res: &mut SyncResult,
    all: &[Target],
) {
    let Some(skills_path) = t.skills.clone() else {
        return;
    };
    let id = t.agent.spec.id;
    // another agent may read the very same folder (one linked to the
    // other): what that agent is to have stays
    let here = real_dir(&skills_path);
    let sharers: Vec<String> = all
        .iter()
        .filter(|o| {
            o.agent.spec.id != id
                && o.skills
                    .as_ref()
                    .is_some_and(|skills| real_dir(skills) == here)
        })
        .map(|o| o.agent.spec.id.to_owned())
        .collect();
    let mut mine = Vec::new();
    let had = l
        .applied
        .get(id)
        .map(|a| a.skills.clone())
        .unwrap_or_default();
    for name in had {
        let wanted = l.skill(&name).is_some_and(|s| {
            s.agents.iter().any(|a| a == id)
                || sharers
                    .iter()
                    .any(|sharer| s.agents.iter().any(|a| a == sharer))
        });
        let p = skills_path.join(&name);
        if wanted || !ours(&p, &name, store) {
            continue;
        }
        if let Err(error) = unlink(&p) {
            res.fail(id, &format!("skill:{name}"), &error);
            mine.push(name);
        } else {
            res.changed(id);
        }
    }
    for s in &l.skills {
        if !s.agents.iter().any(|a| a == id) {
            continue;
        }
        let p = skills_path.join(&s.name);
        if ours(&p, &s.name, store) {
            mine.push(s.name.clone());
            continue;
        }
        if std::fs::symlink_metadata(&p).is_ok() {
            res.fail(
                id,
                &format!("skill:{}", s.name),
                &anyhow::anyhow!(
                    "{} already has a skill of its own called {}",
                    t.agent.spec.name,
                    s.name
                ),
            );
            continue;
        }
        if let Err(error) = link(&p, &s.name, store) {
            res.fail(id, &format!("skill:{}", s.name), &error);
            continue;
        }
        mine.push(s.name.clone());
        res.changed(id);
    }
    l.applied_mut(id).skills = mine;
}

// ---- finding skills to install ---------------------------------------------

// Candidate is a skill found where the user pointed: in a repository, or
// in a folder.
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    // in the repository or the folder; "" for its root
    pub path: String,
    pub name: String,
    pub description: String,
    // the library has a skill by that name
    pub have: bool,
}

// Probe is what was found at a source.
#[derive(Clone, Debug)]
pub struct Probe {
    pub source: String,
    pub kind: String,
    pub candidates: Vec<Candidate>,
    // the folder the candidates' paths are in
    pub(crate) root: PathBuf,
    pub(crate) src: Source,
}

struct ProbeEntry {
    probe: Probe,
    tmp: PathBuf,
    at: OffsetDateTime,
}

static PROBES: Mutex<Option<BTreeMap<String, ProbeEntry>>> = Mutex::new(None);

fn probes() -> std::sync::MutexGuard<'static, Option<BTreeMap<String, ProbeEntry>>> {
    PROBES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// githubSource reads a repository out of what the user typed: owner/repo,
// or a github.com URL, to a folder in it.
pub(crate) fn github_source(s: &str) -> Option<Source> {
    let mut s = s.trim().to_owned();
    while s.ends_with('/') {
        s.pop();
    }
    if !s.contains("://") && s.starts_with("github.com/") {
        s = format!("https://{s}");
    }
    let parts: Vec<String> = if s.contains("://") {
        let url = url::Url::parse(&s).ok()?;
        if !matches!(url.host_str(), Some("github.com") | Some("www.github.com")) {
            return None;
        }
        url.path()
            .trim_matches('/')
            .split('/')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        let (owner, repo) = s.split_once('/')?;
        let ok_part = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        };
        if !ok_part(owner) || !ok_part(repo) {
            return None;
        }
        s.split('/').map(str::to_owned).collect()
    };
    if parts.len() < 2 {
        return None;
    }
    let mut src = Source {
        kind: "github".to_owned(),
        repo: format!(
            "{}/{}",
            parts[0],
            parts[1].strip_suffix(".git").unwrap_or(&parts[1])
        ),
        ..Source::default()
    };
    if parts.len() >= 4 && (parts[2] == "tree" || parts[2] == "blob") {
        src.ref_ = parts[3].clone();
        let path = parts[4..].join("/");
        let path = path.strip_suffix("SKILL.md").unwrap_or(&path);
        src.path = path.trim_end_matches('/').to_owned();
    }
    Some(src)
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
    {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        return home.join(rest.trim_start_matches(['/', '\\']));
    }
    PathBuf::from(p)
}

// tarball_url is where GitHub hands out a repository's files: codeload,
// what github.com's own "Download ZIP" uses, rather than the API's /tarball
// — the API allows 60 requests an hour to an address without a token, which
// a few installs, or a network shared with others, used up. Tests point the
// base at their own server.
static TARBALL_BASE: Mutex<Option<String>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_tarball_base(base: Option<String>) {
    *TARBALL_BASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = base;
}

fn tarball_url(repo: &str, ref_: &str) -> String {
    if let Some(base) = TARBALL_BASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return format!("{base}/{repo}/{}", if ref_.is_empty() { "HEAD" } else { ref_ });
    }
    let ref_ = if ref_.is_empty() { "HEAD" } else { ref_ };
    format!(
        "https://codeload.github.com/{repo}/tar.gz/{}",
        url_escape(ref_)
    )
}

fn url_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", u32::from(c) & 0xff)
            }
        })
        .collect()
}

// fetch downloads the repository into a new folder and gives it back.
async fn fetch(src: &Source) -> Result<PathBuf> {
    let client = reqwest::Client::builder()
        .user_agent("magpie")
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("build HTTP client")?;
    let response = client
        .get(tarball_url(&src.repo, &src.ref_))
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("couldn't reach GitHub: {error}"))?;
    match response.status().as_u16() {
        404 => {
            if !src.ref_.is_empty() {
                bail!(
                    "GitHub has no {} at {} (a private repository can't be installed from)",
                    src.repo,
                    src.ref_
                );
            }
            bail!(
                "GitHub has no repository {} (a private one can't be installed from)",
                src.repo
            );
        }
        403 | 429 => bail!("GitHub is limiting requests from here; try again in a while"),
        status if status != 200 => bail!("GitHub answered {status}"),
        _ => {}
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| anyhow::anyhow!("couldn't reach GitHub: {error}"))?;
    let tmp = std::env::temp_dir().join(format!(
        "magpie-skill-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    std::fs::create_dir_all(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let unpacked = match crate::library::archive::gunzip(&bytes, 200 << 20) {
        Ok(unpacked) => unpacked,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(error);
        }
    };
    if let Err(error) = crate::library::archive::untar(&unpacked, &tmp, 200 << 20) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(error);
    }
    Ok(tmp)
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// candidates are the skills under root/sub: a folder with a SKILL.md,
// looked for a few folders deep, and not inside another skill.
pub(crate) fn candidates(root: &Path, sub: &str) -> Vec<Candidate> {
    let base = root.join(sub.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut out = Vec::new();
    walk_candidates(&base, root, 4, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn walk_candidates(dir: &Path, root: &Path, depth: u32, out: &mut Vec<Candidate>) {
    if let Some(m) = read_meta(dir) {
        let rel = dir
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        out.push(Candidate {
            path: rel,
            name: name_for(dir, &m),
            description: m.description,
            have: false,
        });
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let p = entry.path();
        if std::fs::metadata(&p).is_ok_and(|meta| meta.is_dir()) {
            walk_candidates(&p, root, depth - 1, out);
        }
    }
}

// ProbeSkills looks for skills at what the user typed: a GitHub repository
// (or a folder in one), or a folder on this machine.
pub async fn probe_skills(input: &str) -> Result<Probe> {
    probe_skills_at(&store(), input).await
}

pub(crate) async fn probe_skills_at(store: &Store, input: &str) -> Result<Probe> {
    let input = input.trim();
    if input.is_empty() {
        bail!("paste a GitHub link or a folder's path");
    }
    {
        let mut probes = probes();
        let stale = probes
            .as_ref()
            .map(|m| {
                m.iter()
                    .filter(|(_, e)| {
                        OffsetDateTime::now_utc() - e.at > time::Duration::minutes(15)
                    })
                    .map(|(k, _)| k.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(m) = probes.as_mut() {
            for key in stale {
                if let Some(entry) = m.remove(&key) {
                    let _ = std::fs::remove_dir_all(&entry.tmp);
                }
            }
        }
    }

    let dir = expand(input);
    let probe = if dir.is_absolute() {
        if !dir.is_dir() {
            bail!("{input} isn't a folder");
        }
        Probe {
            source: input.to_owned(),
            kind: "folder".to_owned(),
            candidates: candidates(&dir, ""),
            root: dir.clone(),
            src: Source {
                kind: "folder".to_owned(),
                dir: dir.to_string_lossy().into_owned(),
                ..Source::default()
            },
        }
    } else if let Some(src) = github_source(input) {
        let tmp = fetch(&src).await?;
        if !src.path.is_empty()
            && !tmp
                .join(src.path.replace('/', std::path::MAIN_SEPARATOR_STR))
                .exists()
        {
            let _ = std::fs::remove_dir_all(&tmp);
            bail!("{} has no folder {}", src.repo, src.path);
        }
        let probe = Probe {
            source: input.to_owned(),
            kind: "github".to_owned(),
            candidates: candidates(&tmp, &src.path),
            root: tmp.clone(),
            src: src.clone(),
        };
        let mut probes = probes();
        if let Some(m) = probes.as_mut()
            && let Some(old) = m.insert(
                input.to_owned(),
                ProbeEntry {
                    probe: probe.clone(),
                    tmp,
                    at: OffsetDateTime::now_utc(),
                },
            )
        {
            let _ = std::fs::remove_dir_all(&old.tmp);
        }
        probe
    } else {
        bail!("that's neither a GitHub repository nor a folder's full path");
    };
    let mut probe = probe;
    if let Ok(l) = crate::library::load(store) {
        for c in &mut probe.candidates {
            c.have = l.skill(&c.name).is_some();
        }
    }
    Ok(probe)
}

fn cached_probe(input: &str) -> Option<Probe> {
    let probes = probes();
    probes
        .as_ref()
        .and_then(|m| m.get(input.trim()))
        .filter(|e| e.tmp.exists())
        .map(|e| e.probe.clone())
}

// InstallSkills adds the skills at those paths of the source to the library
// and gives them to the agents named.
pub async fn install_skills(
    input: &str,
    paths: &[String],
    agents: &[String],
) -> Result<SyncResult> {
    install_skills_at(&store(), &Homes::detect(), input, paths, agents).await
}

pub(crate) async fn install_skills_at(
    store: &Store,
    homes: &Homes,
    input: &str,
    paths: &[String],
    agents: &[String],
) -> Result<SyncResult> {
    let input = input.trim();
    let probe = match cached_probe(input) {
        Some(probe) => probe,
        None => probe_skills_at(store, input).await?,
    };
    let mut installed: Vec<Skill> = Vec::new();
    for path in paths {
        let c = probe
            .candidates
            .iter()
            .find(|c| &c.path == path)
            .with_context(|| format!("no skill at {path:?}"))?;
        check_name("skill", &c.name)?;
        let mut src = probe.src.clone();
        let from = probe
            .root
            .join(c.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let dest = skill_dir(store, &c.name);
        std::fs::create_dir_all(skills_dir(store))
            .with_context(|| format!("create {}", skills_dir(store).display()))?;
        if src.kind == "folder" {
            src.dir = from.to_string_lossy().into_owned();
            if make_link(&from, &dest).is_err() {
                copy_dir(&from, &dest)?;
            }
        } else {
            src.path = c.path.clone();
            if let Err(error) = copy_dir(&from, &dest) {
                let _ = std::fs::remove_dir_all(&dest);
                return Err(error);
            }
        }
        installed.push(Skill {
            name: c.name.clone(),
            source: Some(src),
            agents: agents.to_vec(),
        });
    }
    change_in(store, homes, move |l| {
        for s in installed {
            if l.skill(&s.name).is_some() {
                let _ = std::fs::remove_dir_all(skill_dir(store, &s.name));
                bail!("the library already has a skill called {}", s.name);
            }
            l.skills.push(s);
        }
        Ok(())
    })
}

// UpdateSkill fetches a skill from GitHub again, in place: the agents'
// links go on pointing at it.
pub async fn update_skill(name: &str) -> Result<SyncResult> {
    update_skill_at(&store(), &Homes::detect(), name).await
}

pub(crate) async fn update_skill_at(
    store: &Store,
    homes: &Homes,
    name: &str,
) -> Result<SyncResult> {
    let l = crate::library::load(store)?;
    let s = l
        .skill(name)
        .with_context(|| format!("no skill called {name}"))?
        .clone();
    let mut adopt = false;
    let mut src = match &s.source {
        Some(src) if src.kind == "github" => src.clone(),
        _ => {
            // one CC Switch installed from GitHub becomes the library's own,
            // fetched from there, leaving CC Switch's folder as it is
            let Some(o) = crate::library::ccswitch::cc_switch_origin(store, homes, &s) else {
                bail!("{name} isn't from GitHub; it's kept as it is");
            };
            adopt = true;
            o
        }
    };
    let mut tmp = fetch(&src).await?;
    if adopt && !src.ref_.is_empty() {
        // the branch CC Switch recorded is gone: the default one
        let fallback = Source {
            ref_: String::new(),
            ..src.clone()
        };
        match fetch(&fallback).await {
            Ok(fallback_tmp) => {
                let _ = std::fs::remove_dir_all(&tmp);
                src = fallback;
                tmp = fallback_tmp;
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(error);
            }
        }
    }
    let result = finish_update(store, homes, name, &src, &tmp, adopt);
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn finish_update(
    store: &Store,
    homes: &Homes,
    name: &str,
    src: &Source,
    tmp: &Path,
    adopt: bool,
) -> Result<SyncResult> {
    let mut src = src.clone();
    if adopt {
        let path = origin(tmp, &src, name)
            .with_context(|| format!("{} has no skill {name} any more", src.repo))?;
        src.path = path;
    }
    let from = tmp.join(src.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if read_meta(&from).is_none() {
        bail!(
            "{} has no SKILL.md at {} any more",
            src.repo,
            if src.path.is_empty() { "." } else { &src.path }
        );
    }
    let store = store.clone();
    let homes = homes.clone();
    let name = name.to_owned();
    change_in(&store, &homes, move |l| {
        let next = skill_dir(&store, &format!(".{name}.next"));
        let old = skill_dir(&store, &format!(".{name}.old"));
        let _ = std::fs::remove_dir_all(&next);
        let _ = std::fs::remove_dir_all(&old);
        if let Err(error) = copy_dir(&from, &next) {
            let _ = std::fs::remove_dir_all(&next);
            return Err(error);
        }
        let mut old_path = old;
        let mut renamed = true;
        match std::fs::symlink_metadata(skill_dir(&store, &name)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // a link to CC Switch's folder: only the link goes
                std::fs::remove_file(skill_dir(&store, &name))
                    .with_context(|| format!("remove the link to {name}"))?;
                renamed = false;
            }
            Ok(_) => {
                std::fs::rename(skill_dir(&store, &name), &old).with_context(|| {
                    format!("move {}", skill_dir(&store, &name).display())
                })?;
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&next);
                bail!("{} has no folder in the library", name);
            }
        }
        if let Err(error) = std::fs::rename(&next, skill_dir(&store, &name)) {
            let _ = std::fs::remove_dir_all(&next);
            if renamed {
                let _ = std::fs::rename(&old_path, skill_dir(&store, &name));
            }
            return Err(error).with_context(|| format!("move {}", next.display()));
        }
        if adopt
            && let Some(s) = l.skill_mut(&name)
        {
            s.source = Some(src);
        }
        if !renamed {
            return Ok(());
        }
        std::fs::remove_dir_all(&old_path)
            .with_context(|| format!("remove {}", old_path.display()))
    })
}

// origin finds the skill in the repository fetched for it: the folder named
// as CC Switch named it, or the skill of that name.
fn origin(root: &Path, src: &Source, name: &str) -> Option<String> {
    candidates(root, "").into_iter().find_map(|c| {
        let last = c.path.split('/').next_back().unwrap_or(&c.path);
        (last == src.path || c.name == name).then_some(c.path)
    })
}

// SkillAgents sets which agents get a skill.
pub fn skill_agents(name: &str, agents: Vec<String>) -> Result<SyncResult> {
    skill_agents_at(&store(), &Homes::detect(), name, agents)
}

pub(crate) fn skill_agents_at(
    store: &Store,
    homes: &Homes,
    name: &str,
    agents: Vec<String>,
) -> Result<SyncResult> {
    let name = name.to_owned();
    change_in(store, homes, move |l| {
        let s = l
            .skill_mut(&name)
            .with_context(|| format!("no skill called {name}"))?;
        s.agents = agents;
        s.agents.sort();
        Ok(())
    })
}

// RemoveSkill takes a skill out of the library and every agent; its folder
// is kept aside with the backups, never just deleted.
pub fn remove_skill(name: &str) -> Result<SyncResult> {
    remove_skill_at(&store(), &Homes::detect(), name)
}

pub(crate) fn remove_skill_at(store: &Store, homes: &Homes, name: &str) -> Result<SyncResult> {
    let name = name.to_owned();
    change_in(store, homes, move |l| {
        let i = l
            .skills
            .iter()
            .position(|s| s.name == name)
            .with_context(|| format!("no skill called {name}"))?;
        l.skills.remove(i);
        let p = skill_dir(store, &name);
        if let Ok(meta) = std::fs::symlink_metadata(&p)
            && meta.file_type().is_symlink()
        {
            // a folder of the user's: only the link goes
            return std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()));
        }
        let aside = store
            .backup_dir()
            .join(timestamp())
            .join("skills")
            .join(&name);
        if let Some(parent) = aside.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        move_dir(&p, &aside)
    })
}

// ---- skills the agents have of their own -----------------------------------

// FoundSkill is a skill an agent has that the library doesn't.
#[derive(Clone, Debug, Serialize)]
pub struct FoundSkill {
    pub name: String,
    pub description: String,
    // the agents that have this very folder
    pub agents: Vec<String>,
    // agents with another by that name
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<String>,
    // where it really is, when it's a link
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub link: String,
    #[serde(skip)]
    pub(crate) real: PathBuf,
    // the entry in the first agent's folder
    #[serde(skip)]
    pub(crate) at: PathBuf,
}

pub(crate) fn found_skills(l: &Library, homes: &Homes, store: &Store) -> Vec<FoundSkill> {
    let mut by_name: BTreeMap<String, FoundSkill> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for t in &crate::library::targets(homes) {
        let Some(skills_path) = &t.skills else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(skills_path) else {
            continue;
        };
        let id = t.agent.spec.id;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let p = entry.path();
            if name.starts_with('.')
                || ours(&p, &name, store)
                || l.skill(&name).is_some()
                || !is_name(&name)
            {
                continue;
            }
            let Some(m) = read_meta(&p) else {
                continue;
            };
            let r = real_dir(&p);
            match by_name.get_mut(&name) {
                None => {
                    let is_link = std::fs::symlink_metadata(&p)
                        .is_ok_and(|meta| meta.file_type().is_symlink());
                    order.push(name.clone());
                    by_name.insert(
                        name.clone(),
                        FoundSkill {
                            name,
                            description: m.description,
                            agents: vec![id.to_owned()],
                            others: Vec::new(),
                            link: if is_link {
                                r.to_string_lossy().into_owned()
                            } else {
                                String::new()
                            },
                            real: r,
                            at: p,
                        },
                    );
                }
                Some(found) if found.real == r => {
                    if !found.agents.iter().any(|a| a == id) {
                        found.agents.push(id.to_owned());
                    }
                }
                Some(found) => found.others.push(id.to_owned()),
            }
        }
    }
    order
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .collect()
}

// ImportSkill takes a skill the agents have of their own into the library:
// a folder is moved in, a link to a folder elsewhere is linked to from the
// library, and the agents that had it get the library's from then on.
pub fn import_skill(name: &str) -> Result<SyncResult> {
    import_skill_at(&store(), &Homes::detect(), name)
}

pub(crate) fn import_skill_at(store: &Store, homes: &Homes, name: &str) -> Result<SyncResult> {
    let name = name.to_owned();
    change_in(store, homes, move |l| {
        let f = found_skills(l, homes, store)
            .into_iter()
            .find(|f| f.name == name)
            .with_context(|| {
                format!("no agent has a skill called {name} that the library hasn't")
            })?;
        std::fs::create_dir_all(skills_dir(store))
            .with_context(|| format!("create {}", skills_dir(store).display()))?;
        let mut src = Some(Source {
            kind: "folder".to_owned(),
            dir: f.real.to_string_lossy().into_owned(),
            ..Source::default()
        });
        if !f.link.is_empty() {
            make_link(&f.real, &skill_dir(store, &name))?;
        } else {
            move_dir(&f.at, &skill_dir(store, &name))?;
            src = None;
        }
        for id in &f.agents {
            // links to what was moved, or to the folder elsewhere
            if let Some(t) = crate::library::target_by_id(homes, id)
                && let Some(skills) = &t.skills
            {
                let p = skills.join(&name);
                if let Ok(meta) = std::fs::symlink_metadata(&p)
                    && meta.file_type().is_symlink()
                {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        l.skills.push(Skill {
            name,
            source: src,
            agents: f.agents,
        });
        Ok(())
    })
}

// ---- files -----------------------------------------------------------------

pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    copy_dir_inner(from, from, to)
}

fn copy_dir_inner(root: &Path, from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    let entries =
        std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let p = entry.path();
        let dst = to.join(&name);
        let kind = entry.file_type().with_context(|| format!("read {}", p.display()))?;
        if kind.is_symlink() {
            if let Ok(target) = std::fs::read_link(&p) {
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&target, &dst)
                        .with_context(|| format!("link {}", dst.display()))?;
                }
                #[cfg(windows)]
                {
                    let linked = if p.is_dir() {
                        std::os::windows::fs::symlink_dir(&target, &dst)
                    } else {
                        std::os::windows::fs::symlink_file(&target, &dst)
                    };
                    linked.with_context(|| format!("link {}", dst.display()))?;
                }
            }
            continue;
        }
        if kind.is_dir() {
            if name == ".git" && p != root {
                continue;
            }
            copy_dir_inner(root, &p, &dst)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        std::fs::copy(&p, &dst)
            .with_context(|| format!("copy {} to {}", p.display(), dst.display()))?;
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(&p) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &dst,
                std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777),
            );
        }
    }
    Ok(())
}

// move_dir renames, or copies and removes where a rename can't cross disks.
fn move_dir(from: &Path, to: &Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("move {}", from.display()));
        }
        Err(_) => {}
    }
    if let Err(error) = copy_dir(from, to) {
        let _ = std::fs::remove_dir_all(to);
        return Err(error);
    }
    std::fs::remove_dir_all(from).with_context(|| format!("remove {}", from.display()))
}

// SkillText is a library skill's SKILL.md, for the page to show.
pub fn skill_text(name: &str) -> Result<String> {
    check_name("skill", name)?;
    let path = skill_dir(&store(), name).join("SKILL.md");
    std::fs::read_to_string(&path)
        .with_context(|| format!("{name} has no SKILL.md in the library"))
}

// SkillPath is where a library skill's folder is.
pub fn skill_path(name: &str) -> PathBuf {
    skill_dir(&store(), name)
}

// SkillView is a library skill as the page shows it.
#[derive(Clone, Debug, Serialize)]
pub struct SkillView {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    // github, folder, or "" for one kept in the library
    #[serde(default)]
    pub kind: String,
    // on GitHub, as CC Switch installed it: it can be updated from there
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    #[serde(default)]
    pub agents: Vec<String>,
    // its folder is gone
    #[serde(default, skip_serializing_if = "is_false")]
    pub missing: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub problems: BTreeMap<String, String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
