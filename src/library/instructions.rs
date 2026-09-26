// The shared instructions: one text written into each agent's own file,
// with what each gets besides kept in files beside library.json.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::atomic_write_for_settings;
use crate::library::{
    Backups, Homes, Library, Store, SyncResult, Target, agent_icon, change_in, check_name,
    read_optional, read_text, set, targets,
};

// The part of an agent's instructions file magpie writes sits between these
// two lines; the rest of the file is the user's.
pub(crate) const BLOCK_BEGIN: &str = "<!-- magpie:begin · written by magpie from its Library -->";
pub(crate) const BLOCK_END: &str = "<!-- magpie:end -->";

pub(crate) fn shared_path(store: &Store) -> PathBuf {
    store.dir().join("instructions.md")
}

pub(crate) fn extra_path(store: &Store, agent: &str) -> PathBuf {
    store.dir().join("instructions").join(format!("{agent}.md"))
}

pub(crate) fn write_text(path: &Path, text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", path.display()));
            }
        }
        return Ok(());
    }
    atomic_write_for_settings(path, format!("{text}\n").as_bytes())
}

// wanted is what magpie writes into the agent's file: the shared text and
// the agent's own additions after it; "" for nothing.
fn wanted(store: &Store, l: &Library, agent: &str) -> String {
    if !l.instructions.agents.iter().any(|id| id == agent) {
        return String::new();
    }
    [
        read_text(&shared_path(store)),
        read_text(&extra_path(store, agent)),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}

// split takes a file apart: what is before magpie's part, magpie's part,
// and what is after it. ok is false when magpie has no part in it.
pub(crate) fn split(file: &str) -> (&str, &str, &str, bool) {
    let Some(i) = file.find(BLOCK_BEGIN) else {
        return (file, "", "", false);
    };
    let rest = &file[i + BLOCK_BEGIN.len()..];
    let Some(j) = rest.find(BLOCK_END) else {
        return (file, "", "", false);
    };
    (
        &file[..i],
        rest[..j].trim(),
        &rest[j + BLOCK_END.len()..],
        true,
    )
}

// own is the file without magpie's part: the user's own instructions.
pub(crate) fn own(file: &str) -> String {
    let (before, _, after, _) = split(file);
    format!("{}\n\n{}", before.trim(), after.trim())
        .trim()
        .to_owned()
}

// with_block is the file with magpie's part set to text, or taken out for
// "". A new part goes after what the user has written.
pub(crate) fn with_block(file: &str, text: &str) -> String {
    let file = file.replace("\r\n", "\n");
    let (before, _, after, ok) = split(&file);
    let mut before = before.trim_end_matches('\n').to_owned();
    let mut after = after.trim_start_matches('\n').to_owned();
    if !ok {
        before = file.trim_end_matches('\n').to_owned();
        after = String::new();
    }
    let block = if text.is_empty() {
        String::new()
    } else {
        format!("{BLOCK_BEGIN}\n{text}\n{BLOCK_END}")
    };
    let mut parts: Vec<String> = Vec::new();
    for part in [before, block, after.trim_end_matches('\n').to_owned()] {
        if !part.trim().is_empty() {
            parts.push(part);
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{}\n", parts.join("\n\n"))
}

fn hash(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out = h.finalize();
    out.iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// sync_instructions writes the agent's part as the library has it. A part
// the user edited in the file is left alone until the library's text for
// that agent changes; then it is replaced, the file kept aside first.
pub(crate) fn sync_instructions(
    store: &Store,
    l: &mut Library,
    t: &Target,
    b: &mut Backups,
    res: &mut SyncResult,
) {
    let Some(path) = t.instructions.clone() else {
        return;
    };
    let id = t.agent.spec.id;
    let want = wanted(store, l, id);
    let (raw_len, file) = match read_optional(&path) {
        Ok(Some(bytes)) => (
            bytes.len(),
            String::from_utf8_lossy(&bytes).replace("\r\n", "\n"),
        ),
        Ok(None) => (0, String::new()),
        Err(error) => {
            res.fail(id, "instructions", &error);
            return;
        }
    };
    let (_, have, _, ok) = split(&file);
    let have = have.to_owned();
    let a = l.applied_mut(id);
    if want.is_empty() && !ok {
        a.instructions = false;
        a.instr_hash = String::new();
        return;
    }
    if ok && have == want {
        a.instructions = !want.is_empty();
        a.instr_hash = hash(&want);
        return;
    }
    if ok
        && !want.is_empty()
        && !a.instr_hash.is_empty()
        && hash(&have) != a.instr_hash
        && hash(&want) == a.instr_hash
    {
        return; // edited in the file since, and the library hasn't changed for it
    }
    let next = with_block(&file, &want);
    if let Err(error) = b.keep(id, &path) {
        res.fail(id, "instructions", &error);
        return;
    }
    let error = if next.trim().is_empty() && raw_len > 0 && own(&file).is_empty() {
        std::fs::remove_file(&path).err().map(anyhow::Error::new)
    } else if !next.is_empty() {
        atomic_write_for_settings(&path, next.as_bytes()).err()
    } else {
        None
    };
    if let Some(error) = error {
        res.fail(id, "instructions", &error);
        return;
    }
    let a = l.applied_mut(id);
    a.instructions = !want.is_empty();
    a.instr_hash = hash(&want);
    res.changed(id);
}

fn is_false(value: &bool) -> bool {
    !value
}

// AgentInstructions is one agent's instructions file as the page shows it.
#[derive(Debug, Serialize)]
pub struct AgentInstructions {
    pub agent: String,
    pub name: String,
    pub icon: String,
    pub path: String,
    pub on: bool,
    // what this agent gets besides
    pub extra: String,
    // lines of the user's own in the file
    pub own: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub edited: bool,
    // a file the agent reads instead
    #[serde(rename = "override", default, skip_serializing_if = "String::is_empty")]
    pub override_: String,
}

// InstructionsView is the Instructions page.
#[derive(Debug, Serialize)]
pub struct InstructionsView {
    pub shared: String,
    pub agents: Vec<AgentInstructions>,
}

// ReadInstructions is the shared text and each agent's file.
pub fn read_instructions() -> Result<InstructionsView> {
    read_instructions_at(&crate::library::store(), &Homes::detect())
}

pub(crate) fn read_instructions_at(store: &Store, homes: &Homes) -> Result<InstructionsView> {
    let l = crate::library::load(store)?;
    let mut v = InstructionsView {
        shared: read_text(&shared_path(store)),
        agents: Vec::new(),
    };
    for t in targets(homes) {
        let Some(path) = t.instructions.clone() else {
            continue;
        };
        let id = t.agent.spec.id;
        let mut ai = AgentInstructions {
            agent: id.to_owned(),
            name: t.agent.spec.name.to_owned(),
            icon: agent_icon(id).to_owned(),
            path: path.to_string_lossy().into_owned(),
            on: l.instructions.agents.iter().any(|a| a == id),
            extra: read_text(&extra_path(store, id)),
            own: 0,
            edited: false,
            override_: String::new(),
        };
        let file = read_text(&path);
        let mine = own(&file);
        if !mine.is_empty() {
            ai.own = mine.matches('\n').count() + 1;
        }
        let (_, have, _, ok) = split(&file);
        if ok && wanted(store, &l, id) != have {
            ai.edited = true;
        }
        if let Some(over) = &t.override_
            && !read_text(over).is_empty()
        {
            ai.override_ = over.to_string_lossy().into_owned();
        }
        v.agents.push(ai);
    }
    Ok(v)
}

// InstructionsChange is what the page saves: the shared text, which agents
// get it, and what each gets besides. A none field is left as it is.
#[derive(Debug, Default)]
pub struct InstructionsChange {
    pub shared: Option<String>,
    pub agents: Option<Vec<String>>,
    pub extra: std::collections::BTreeMap<String, String>,
    // Rewrite writes magpie's part again into these agents' files, over
    // edits made there.
    pub rewrite: Vec<String>,
}

pub(crate) fn save_change(store: &Store, l: &mut Library, c: InstructionsChange) -> Result<()> {
    if let Some(shared) = c.shared {
        write_text(&shared_path(store), &shared)?;
    }
    if let Some(agents) = c.agents {
        l.instructions.agents = agents;
        l.instructions.agents.sort();
    }
    for (id, x) in c.extra {
        check_name("agent", &id)?;
        write_text(&extra_path(store, &id), &x)?;
    }
    for id in c.rewrite {
        l.applied_mut(&id).instr_hash = String::new();
    }
    Ok(())
}

// SaveInstructions saves the change and writes it into the agents.
pub fn save_instructions(c: InstructionsChange) -> Result<SyncResult> {
    save_instructions_at(&crate::library::store(), &Homes::detect(), c)
}

pub(crate) fn save_instructions_at(
    store: &Store,
    homes: &Homes,
    c: InstructionsChange,
) -> Result<SyncResult> {
    change_in(store, homes, |l| save_change(store, l, c))
}

// ImportInstructions moves the user's own instructions out of an agent's
// file into the library: they are added to the shared text, taken out of
// the file (which is kept aside first), and the agent gets the shared text
// from then on — so what it reads is what it read before.
pub fn import_instructions(id: &str) -> Result<SyncResult> {
    import_instructions_at(&crate::library::store(), &Homes::detect(), id)
}

pub(crate) fn import_instructions_at(store: &Store, homes: &Homes, id: &str) -> Result<SyncResult> {
    let t =
        crate::library::target_by_id(homes, id).and_then(|t| t.instructions.map(|path| (t, path)));
    let Some((_, path)) = t else {
        anyhow::bail!("{id} has no instructions file magpie knows");
    };
    change_in(store, homes, |l| {
        let raw = read_optional(&path)?;
        let file = raw
            .map(|bytes| String::from_utf8_lossy(&bytes).replace("\r\n", "\n"))
            .unwrap_or_default();
        let mine = own(&file);
        if mine.is_empty() {
            anyhow::bail!("{} has nothing of its own to import", path.display());
        }
        let mut shared = read_text(&shared_path(store));
        if !shared.contains(&mine) {
            shared = format!("{}\n\n{}", shared.trim(), mine).trim().to_owned();
        }
        write_text(&shared_path(store), &shared)?;
        let mut b = Backups::new();
        b.keep(id, &path)?;
        let (_, block, _, ok) = split(&file);
        let next = if ok {
            with_block("", block)
        } else {
            String::new()
        };
        if next.is_empty() {
            std::fs::remove_file(&path)?;
        } else {
            atomic_write_for_settings(&path, next.as_bytes())?;
        }
        set(&mut l.instructions.agents, id, true);
        // what's there now isn't what the library says
        l.applied_mut(id).instr_hash = String::new();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_finds_magpies_part() {
        let text = format!("Before.\n\n{BLOCK_BEGIN}\nUse tabs.\n{BLOCK_END}\nAfter.");
        let (before, block, after, ok) = split(&text);
        assert!(ok);
        assert_eq!(before, "Before.\n\n");
        assert_eq!(block, "Use tabs.");
        assert_eq!(after, "\nAfter.");
        let (before, block, after, ok) = split("no block here");
        assert!(!ok);
        assert_eq!(before, "no block here");
        assert_eq!((block, after), ("", ""));
    }

    #[test]
    fn with_block_appends_after_the_users_text() {
        let text = "Use tabs.";
        let next = with_block("# Mine\n\nBe brief.\n", text);
        assert_eq!(
            next,
            format!("# Mine\n\nBe brief.\n\n{BLOCK_BEGIN}\n{text}\n{BLOCK_END}\n")
        );
        // a second write replaces the block in place
        let next2 = with_block(&next, "Use spaces.");
        assert!(next2.contains("Use spaces."));
        assert!(!next2.contains("Use tabs."));
        // empty takes the block out, keeping the user's part
        let next3 = with_block(&next, "");
        assert_eq!(next3, "# Mine\n\nBe brief.\n");
        // a file only magpie wrote goes back to nothing
        let next4 = with_block(&format!("{BLOCK_BEGIN}\n{text}\n{BLOCK_END}\n"), "");
        assert_eq!(next4, "");
        assert_eq!(
            with_block("line\r\n", "x"),
            format!("line\n\n{BLOCK_BEGIN}\nx\n{BLOCK_END}\n")
        );
    }

    #[test]
    fn own_keeps_what_the_user_wrote() {
        let text = format!("# Mine\n\n{BLOCK_BEGIN}\nx\n{BLOCK_END}\n\nBe brief.");
        assert_eq!(own(&text), "# Mine\n\nBe brief.");
        assert_eq!(own("Only mine."), "Only mine.");
        assert_eq!(own(&format!("{BLOCK_BEGIN}\nx\n{BLOCK_END}\n")), "");
    }

    #[test]
    fn hash_is_short_and_stable() {
        assert_eq!(hash("abc"), hash("abc"));
        assert_ne!(hash("abc"), hash("abd"));
        assert_eq!(hash("abc").len(), 16);
    }
}
